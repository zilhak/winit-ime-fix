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
// round trip to the X server only, so it returns even when the XIM server is gone. If the owner
// changed we look for the server window's `DestroyNotify` (the round trip guarantees that every
// event the X server generated before the reply is in the Xlib queue) and filter only that event,
// so libX11 tears the input method down through its normal path (`_XimServerDestroy`, which calls
// our destroy callback). If no such event is queued the old server may still be alive; the
// request is skipped, but the input method is not treated as destroyed.
//
// This cannot help when the server dies after the check, or when it is alive but not answering.

use std::ffi::CStr;
use std::os::raw::{c_char, c_int};
use std::sync::Arc;
use std::{mem, ptr};

use x11rb::protocol::xproto;

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
    /// Selection atoms and their owners when the input method was opened. One entry when the
    /// server could be identified; otherwise every candidate, any of whose change makes the
    /// server unverifiable.
    Servers(Vec<(ffi::Atom, ffi::Window)>),
}

impl ServerBinding {
    /// Records the server of the input method that `XOpenIM` just opened with `modifiers` (the
    /// value returned by `XSetLocaleModifiers(NULL)`, i.e. what libX11 actually used).
    ///
    /// libX11 (`_XimPreConnect`) walks `XIM_SERVERS` in order and uses the first atom that has an
    /// owner, whose name matches the input method name and that accepts the locale and transport.
    /// The locale/transport negotiation cannot be repeated without talking to the servers, so the
    /// connected atom is identified by the `StructureNotify` selection that libX11 places on its
    /// owner (`_XimRegisterServerFilter`), which `your_event_mask` reports for this connection.
    pub(crate) unsafe fn record(xconn: &Arc<XConnection>, modifiers: &[u8]) -> Self {
        let im_name = im_name_from_modifiers(modifiers);
        // `_XimCheckIfLocalProcessing`: these names never reach a server.
        if im_name == b"local" || im_name == b"none" {
            return ServerBinding::Local;
        }

        let candidates: Vec<(ffi::Atom, ffi::Window)> = unsafe { xim_server_atoms(xconn) }
            .into_iter()
            .filter(|(_, name)| server_name_matches(name, im_name))
            .filter_map(|(atom, _)| {
                let owner = unsafe { selection_owner(xconn, atom) };
                (owner != 0).then_some((atom, owner))
            })
            .collect();

        let connected: Vec<(ffi::Atom, ffi::Window)> = candidates
            .iter()
            .copied()
            .filter(|&(_, owner)| unsafe { selects_structure_notify(xconn, owner) })
            .collect();

        if !connected.is_empty() {
            return ServerBinding::Servers(connected);
        }
        // A protocol input method always selects `StructureNotify` on its server before
        // `XOpenIM` returns. Without one, an empty or Thai name means libX11 chose local or Thai
        // processing (an empty name uses the compose file of the locale when there is one).
        if candidates.is_empty() || THAI_IM_NAMES.contains(&im_name) {
            ServerBinding::Local
        } else {
            // The owner changed while the input method was being opened. Check every candidate.
            ServerBinding::Servers(candidates)
        }
    }

    /// Whether every recorded selection still has the recorded owner.
    pub(crate) unsafe fn owners_unchanged(&self, xconn: &Arc<XConnection>) -> bool {
        match self {
            ServerBinding::Local => true,
            ServerBinding::Servers(servers) => servers
                .iter()
                .all(|&(atom, owner)| unsafe { selection_owner(xconn, atom) } == owner),
        }
    }

    /// Server windows whose `DestroyNotify` libX11 may be waiting for.
    pub(crate) fn windows(&self) -> impl Iterator<Item = ffi::Window> + '_ {
        let servers = match self {
            ServerBinding::Local => &[][..],
            ServerBinding::Servers(servers) => &servers[..],
        };
        servers.iter().map(|&(_, owner)| owner)
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

unsafe fn selection_owner(xconn: &Arc<XConnection>, atom: ffi::Atom) -> ffi::Window {
    unsafe { (xconn.xlib.XGetSelectionOwner)(xconn.display, atom) }
}

/// Whether this connection selected `StructureNotify` on `window`. A window destroyed in the
/// meantime raises `BadWindow`, which is consumed here and reported as `false`.
unsafe fn selects_structure_notify(xconn: &Arc<XConnection>, window: ffi::Window) -> bool {
    let mut attributes: ffi::XWindowAttributes = unsafe { mem::zeroed() };
    let status =
        unsafe { (xconn.xlib.XGetWindowAttributes)(xconn.display, window, &mut attributes) };
    if xconn.check_errors().is_err() || status == 0 {
        return false;
    }
    attributes.your_event_mask & ffi::StructureNotifyMask != 0
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
