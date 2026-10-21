// Liveness of the XIM server behind an open input method.
//
// libX11 waits for XIM replies with `XIfEvent` and no timeout. A request sent to a server that has
// already exited (or has been replaced, e.g. by `ibus restart`) therefore blocks the calling
// thread forever. libX11 only learns about the exit when the server window's `DestroyNotify` is
// passed through `XFilterEvent`, which may happen after we have already queued the request (for
// instance when an XInput2 `FocusOut` is processed before that `DestroyNotify`).
//
// Before every synchronous XIM request we therefore compare the current owners of the server
// selections with the owners recorded when the input method was opened. `XGetSelectionOwner` is a
// round trip to the X server only, so it returns even when the XIM server is gone. A restarted
// server can get the same window ID as the one it replaces (the X server reuses the resource ID
// base of a disconnected client), so an equal owner is only trusted while this connection still
// has the `StructureNotify` selection libX11 placed on the old window. If the owner changed we
// look for the server window's `DestroyNotify` (the round trip guarantees that every
// event the X server generated before the reply is in the Xlib queue) and filter only that event,
// so libX11 tears the input method down through its normal path (`_XimServerDestroy`, which calls
// our destroy callback). If no such event is queued the old server may still be alive; the
// request is skipped, but the input method is not treated as destroyed.
//
// This cannot help when the server dies after the check, when it is alive but not answering, or
// when it dies while `XOpenIM` negotiates with it. An input method opened with an empty name whose
// server is gone or replaced before it is recorded is indistinguishable from local processing and
// is recorded as local.

use std::ffi::CStr;
use std::os::raw::{c_char, c_int};
use std::sync::Arc;
use std::{mem, ptr};

use x11rb::protocol::xproto::{self, ConnectionExt as _, EventMask};

use super::super::atoms::*;
use super::{ffi, XConnection};

/// Selection atom names of XIM servers start with this category (`XIM_SERVER_CATEGORY`).
const SERVER_CATEGORY: &[u8] = b"@server=";
/// Locale modifier that names the input method (`XIMMODIFIER`).
const IM_MODIFIER: &[u8] = b"@im=";
/// Input method names handled by the Thai input method of libX11 when the locale is Thai.
const THAI_IM_NAMES: [&[u8]; 5] = [b"", b"BasicCheck", b"Strict", b"Thaicat", b"Passthrough"];

/// Server selections an input method depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ServerBinding {
    /// libX11 processes input locally (compose tables) and never waits for a server.
    Local,
    /// Selections whose owner libX11 connected to (it selected `StructureNotify` on the owner).
    Servers(Vec<ServerSelection>),
    /// libX11 connected to a server that is already gone or replaced, so none can be trusted.
    Unverified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ServerSelection {
    atom: ffi::Atom,
    owner: ffi::Window,
}

impl ServerBinding {
    /// Records the server of `im`, which `XOpenIM` just opened with `modifiers` (the value
    /// returned by `XSetLocaleModifiers(NULL)`, i.e. what libX11 actually used).
    ///
    /// libX11 (`_XimPreConnect`) walks `XIM_SERVERS` in order and uses the first atom that has an
    /// owner, whose name matches the input method name and that accepts the locale and transport.
    /// The locale/transport negotiation cannot be repeated without talking to the servers, so the
    /// connected atom is identified by the `StructureNotify` selection that libX11 places on its
    /// owner (`_XimRegisterServerFilter`), which `your_event_mask` reports for this connection.
    pub(crate) unsafe fn record(xconn: &Arc<XConnection>, im: ffi::XIM, modifiers: &[u8]) -> Self {
        let im_name = im_name_from_modifiers(modifiers);
        // `_XimCheckIfLocalProcessing`: these names never reach a server.
        if im_name == b"local" || im_name == b"none" {
            return ServerBinding::Local;
        }

        let connected: Vec<ServerSelection> = unsafe { xim_server_atoms(xconn) }
            .into_iter()
            .filter(|(_, name)| server_name_matches(name, im_name))
            .filter_map(|(atom, _)| {
                let owner = unsafe { selection_owner(xconn, atom) };
                (owner != 0 && selects_structure_notify(xconn, owner))
                    .then_some(ServerSelection { atom, owner })
            })
            .collect();
        if !connected.is_empty() {
            return ServerBinding::Servers(connected);
        }

        // A protocol input method selects `StructureNotify` on its server before `XOpenIM`
        // returns, so libX11 either processed this one itself or its server is gone or replaced.
        if im_name.is_empty() {
            // libX11 processes an empty name locally when the locale has a compose file, which
            // cannot be told apart from a lost server here.
            return ServerBinding::Local;
        }
        if THAI_IM_NAMES.contains(&im_name) && unsafe { is_thai_locale(xconn, im) } {
            // `_XimCheckIfThaiProcessing`.
            return ServerBinding::Local;
        }
        // libX11 serves every other name through a server.
        ServerBinding::Unverified
    }

    /// Whether every recorded selection still has the recorded owner, and that owner is still
    /// the window libX11 selected `StructureNotify` on rather than a new one with a reused ID.
    pub(crate) unsafe fn owners_unchanged(&self, xconn: &Arc<XConnection>) -> bool {
        match self {
            ServerBinding::Local => true,
            ServerBinding::Servers(servers) => servers.iter().all(|server| {
                (unsafe { selection_owner(xconn, server.atom) }) == server.owner
                    && selects_structure_notify(xconn, server.owner)
            }),
            ServerBinding::Unverified => false,
        }
    }

    /// Server windows whose `DestroyNotify` libX11 may be waiting for.
    pub(crate) fn windows(&self) -> impl Iterator<Item = ffi::Window> + '_ {
        let servers = match self {
            ServerBinding::Local | ServerBinding::Unverified => &[][..],
            ServerBinding::Servers(servers) => &servers[..],
        };
        servers.iter().map(|server| server.owner)
    }
}

/// Takes the queued `DestroyNotify` of `window` out of the Xlib queue and passes it to
/// `XFilterEvent`. Other events keep their order. Returns whether such an event was queued.
///
/// The filter may run the input method destroy callback, which replaces the input method and the
/// input contexts. Callers must not hold references into `ImeInner` across this call.
pub(crate) unsafe fn filter_queued_destroy_notify(
    xconn: &Arc<XConnection>,
    window: ffi::Window,
) -> bool {
    let mut event: ffi::XEvent = unsafe { mem::zeroed() };
    let found = unsafe {
        (xconn.xlib.XCheckTypedWindowEvent)(xconn.display, window, ffi::DestroyNotify, &mut event)
    } == ffi::True;
    if found {
        unsafe { (xconn.xlib.XFilterEvent)(&mut event, 0) };
    }
    found
}

/// The input method name libX11 derives from the locale modifiers (`_XimMakeImName`): the text
/// after the first `@im=` up to the next `@`.
pub(crate) fn im_name_from_modifiers(modifiers: &[u8]) -> &[u8] {
    let Some(start) = modifiers
        .windows(IM_MODIFIER.len())
        .position(|window| window == IM_MODIFIER)
        .map(|position| position + IM_MODIFIER.len())
    else {
        return b"";
    };
    let rest = &modifiers[start..];
    let end = rest.iter().position(|&byte| byte == b'@').unwrap_or(rest.len());
    &rest[..end]
}

/// Whether libX11 may connect to the server behind `atom_name` for `im_name`
/// (`_XimCheckServerName`): an empty name accepts every server, otherwise the name must equal one
/// of the comma separated names after `@server=`.
pub(crate) fn server_name_matches(atom_name: &[u8], im_name: &[u8]) -> bool {
    if im_name.is_empty() {
        return true;
    }
    match atom_name.strip_prefix(SERVER_CATEGORY) {
        Some(names) => names.split(|&byte| byte == b',').any(|name| name == im_name),
        None => false,
    }
}

/// The locale modifiers libX11 currently uses.
pub(crate) unsafe fn current_locale_modifiers(xconn: &Arc<XConnection>) -> Vec<u8> {
    let modifiers = unsafe { (xconn.xlib.XSetLocaleModifiers)(ptr::null()) };
    if modifiers.is_null() {
        Vec::new()
    } else {
        unsafe { CStr::from_ptr(modifiers) }.to_bytes().to_vec()
    }
}

/// Atoms listed in the root window's `XIM_SERVERS` property, in order, with their names.
unsafe fn xim_server_atoms(xconn: &Arc<XConnection>) -> Vec<(ffi::Atom, Vec<u8>)> {
    let servers_atom = xconn.atoms()[XIM_SERVERS];
    // libX11 reads the property from screen 0.
    let root = unsafe { (xconn.xlib.XRootWindow)(xconn.display, 0) };
    let Ok(atoms) = xconn.get_property::<xproto::Atom>(
        root as xproto::Window,
        servers_atom,
        xproto::Atom::from(xproto::AtomEnum::ATOM),
    ) else {
        return Vec::new();
    };

    let mut atoms: Vec<ffi::Atom> = atoms.into_iter().map(|atom| atom as ffi::Atom).collect();
    if atoms.is_empty() {
        return Vec::new();
    }
    let mut names: Vec<*mut c_char> = vec![ptr::null_mut(); atoms.len()];
    let status = unsafe {
        (xconn.xlib.XGetAtomNames)(
            xconn.display,
            atoms.as_mut_ptr(),
            atoms.len() as c_int,
            names.as_mut_ptr(),
        )
    };
    let mut result = Vec::with_capacity(atoms.len());
    for (atom, name) in atoms.into_iter().zip(names) {
        if name.is_null() {
            continue;
        }
        if status != 0 {
            result.push((atom, unsafe { CStr::from_ptr(name) }.to_bytes().to_vec()));
        }
        unsafe { (xconn.xlib.XFree)(name.cast()) };
    }
    if let Err(err) = xconn.check_errors() {
        tracing::warn!("failed to read XIM_SERVERS: {err:?}");
        return Vec::new();
    }
    result
}

/// Whether the language of the locale of `im` is Thai (`THAI_LANGUAGE_NAME`).
unsafe fn is_thai_locale(xconn: &Arc<XConnection>, im: ffi::XIM) -> bool {
    let locale = unsafe { (xconn.xlib.XLocaleOfIM)(im) };
    !locale.is_null() && locale_language(unsafe { CStr::from_ptr(locale) }.to_bytes()) == b"th"
}

/// The language part of a locale name such as `th_TH.UTF-8`.
fn locale_language(locale: &[u8]) -> &[u8] {
    let end =
        locale.iter().position(|&byte| matches!(byte, b'_' | b'.' | b'@')).unwrap_or(locale.len());
    &locale[..end]
}

unsafe fn selection_owner(xconn: &Arc<XConnection>, atom: ffi::Atom) -> ffi::Window {
    unsafe { (xconn.xlib.XGetSelectionOwner)(xconn.display, atom) }
}

/// Whether this connection selected `StructureNotify` on `window` (`your_event_mask`). A window
/// destroyed in the meantime is reported as `false`. Uses one XCB round trip; its error does not
/// go through the Xlib error handler.
fn selects_structure_notify(xconn: &Arc<XConnection>, window: ffi::Window) -> bool {
    xconn
        .xcb_connection()
        .get_window_attributes(window as xproto::Window)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .is_some_and(|attributes| attributes.your_event_mask.contains(EventMask::STRUCTURE_NOTIFY))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn im_name_takes_first_im_modifier() {
        // XSetLocaleModifiers appends XMODIFIERS to the program modifiers.
        assert_eq!(im_name_from_modifiers(b"@im=ibus@im=ibus"), b"ibus");
        assert_eq!(im_name_from_modifiers(b"@im=local@im=ibus"), b"local");
        assert_eq!(im_name_from_modifiers(b"@im=@im=ibus"), b"");
        assert_eq!(im_name_from_modifiers(b"@im=fcitx"), b"fcitx");
        assert_eq!(im_name_from_modifiers(b""), b"");
        assert_eq!(im_name_from_modifiers(b"@other=x"), b"");
    }

    #[test]
    fn locale_language_stops_at_territory_codeset_or_modifier() {
        assert_eq!(locale_language(b"th_TH.UTF-8"), b"th");
        assert_eq!(locale_language(b"th"), b"th");
        assert_eq!(locale_language(b"th.TIS-620"), b"th");
        assert_eq!(locale_language(b"ko_KR.UTF-8"), b"ko");
        assert_eq!(locale_language(b"C"), b"C");
    }

    #[test]
    fn server_name_matches_aliases_and_empty_name() {
        assert!(server_name_matches(b"@server=ibus", b"ibus"));
        assert!(server_name_matches(b"@server=fcitx,ibus", b"ibus"));
        assert!(!server_name_matches(b"@server=ibusx", b"ibus"));
        assert!(!server_name_matches(b"@server=fcitx", b"ibus"));
        assert!(!server_name_matches(b"ibus", b"ibus"));
        assert!(server_name_matches(b"@server=fcitx", b""));
        assert!(server_name_matches(b"anything", b""));
    }
}
