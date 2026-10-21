use std::cell::Cell;
use std::collections::HashMap;
use std::mem;
use std::sync::Arc;
use std::time::Instant;

use super::{ffi, XConnection, XError};

use super::context::ImeContext;
use super::input_method::{InputMethod, PotentialInputMethods};
use super::server::filter_queued_destroy_notify;
use crate::platform_impl::platform::x11::ime::ImeEventSender;

pub(crate) unsafe fn close_im(xconn: &Arc<XConnection>, im: ffi::XIM) -> Result<(), XError> {
    unsafe { (xconn.xlib.XCloseIM)(im) };
    xconn.check_errors()
}

pub(crate) unsafe fn destroy_ic(xconn: &Arc<XConnection>, ic: ffi::XIC) -> Result<(), XError> {
    unsafe { (xconn.xlib.XDestroyIC)(ic) };
    xconn.check_errors()
}

/// A callback only requests work. A failed attempt waits for an actual server advertisement,
/// rather than retrying on every key or event-loop iteration.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) enum Reconnect {
    #[default]
    Idle,
    Pending,
    WaitingForServer,
}

impl Reconnect {
    pub fn request(&mut self) {
        *self = Self::Pending;
    }

    pub fn take(&mut self, server_changed: bool) -> bool {
        match self {
            Self::Pending => {},
            Self::WaitingForServer if server_changed => {},
            _ => return false,
        }
        *self = Self::WaitingForServer;
        true
    }

    pub fn complete(&mut self, primary: bool) {
        *self = if primary { Self::Idle } else { Self::WaitingForServer };
    }
}

pub(crate) struct ImeInner {
    pub xconn: Arc<XConnection>,
    pub im: Option<InputMethod>,
    pub potential_input_methods: PotentialInputMethods,
    pub contexts: HashMap<ffi::Window, Option<ImeContext>>,
    // WARNING: this is initially zeroed!
    pub destroy_callback: ffi::XIMCallback,
    pub event_sender: ImeEventSender,
    // Indicates whether or not the input method was destroyed on the server end
    // (i.e. if ibus/fcitx/etc. was terminated/restarted)
    pub is_destroyed: bool,
    pub is_fallback: bool,
    // The server selection owner differs from the one recorded when the input method was opened,
    // but libX11 has not destroyed the input method. Synchronous requests are skipped meanwhile.
    // Unlike `is_destroyed`, the input method and its contexts are still owned by libX11 and are
    // only released through its destroy path.
    pub server_suspect: Cell<bool>,
    pub reconnect: Reconnect,
}

impl ImeInner {
    pub(crate) fn new(
        xconn: Arc<XConnection>,
        potential_input_methods: PotentialInputMethods,
        event_sender: ImeEventSender,
    ) -> Self {
        ImeInner {
            xconn,
            im: None,
            potential_input_methods,
            contexts: HashMap::new(),
            destroy_callback: unsafe { mem::zeroed() },
            event_sender,
            is_destroyed: false,
            is_fallback: false,
            server_suspect: Cell::new(false),
            reconnect: Reconnect::default(),
        }
    }

    /// Whether synchronous requests can be sent to the server of the current input method,
    /// without filtering any event. Safe to call while iterating the contexts and from XIM
    /// callbacks.
    pub fn server_reachable(&self) -> bool {
        let reachable = self.owners_unchanged();
        if !reachable {
            self.mark_suspect();
        }
        reachable
    }

    fn owners_unchanged(&self) -> bool {
        let Some(im) = self.im.as_ref().filter(|_| !self.is_destroyed) else {
            return false;
        };
        let started = Instant::now();
        let unchanged = unsafe { im.server.owners_unchanged(&self.xconn) };
        tracing::trace!("XIM server owner check took {:?}", started.elapsed());
        if unchanged && self.server_suspect.replace(false) {
            tracing::info!("XIM server selection owner restored; resuming XIM requests");
        }
        unchanged
    }

    fn mark_suspect(&self) {
        if !self.server_suspect.replace(true) {
            tracing::warn!(
                "XIM server selection owner changed but the server window is not destroyed yet; \
                 skipping XIM requests until it is destroyed or replaced"
            );
        }
    }

    /// Like [`Self::server_reachable`], but when the owner changed, first lets libX11 handle a
    /// queued `DestroyNotify` of the server window. That runs [`super::xim_destroy_callback`],
    /// which updates destruction/reconnect state. No reference into `*inner` may be held across
    /// this call, and it must not be called while iterating the contexts or from an XIM callback.
    pub unsafe fn ensure_server_alive(inner: *mut ImeInner) -> bool {
        if unsafe { (*inner).owners_unchanged() } {
            return true;
        }
        let (im, windows) = match unsafe { (*inner).im.as_ref() } {
            Some(im) if unsafe { !(*inner).is_destroyed } => {
                (im.im, im.server.windows().collect::<Vec<_>>())
            },
            _ => return false,
        };
        let xconn = unsafe { Arc::clone(&(*inner).xconn) };
        let mut filtered = false;
        for window in windows {
            filtered |= unsafe { filter_queued_destroy_notify(&xconn, window) };
        }
        if !filtered {
            unsafe { (*inner).mark_suspect() };
            return false;
        }
        // Re-read after Xlib callbacks. Reconnection normally stays queued until the caller
        // returns from filtering; the previous input method must not be used in the meantime.
        match unsafe { (*inner).im.as_ref() } {
            Some(new_im) if new_im.im != im => unsafe { (*inner).server_reachable() },
            _ => false,
        }
    }

    pub unsafe fn close_im_if_necessary(&self) -> Result<bool, XError> {
        match self.im.as_ref() {
            Some(_) if self.is_destroyed => Ok(false),
            // The old server may still be alive but is no longer the owner; closing would wait
            // for it. libX11 frees the input method once the server window is destroyed.
            Some(_) if !self.server_reachable() => Ok(false),
            Some(im) => unsafe { close_im(&self.xconn, im.im).map(|_| true) },
            None => Ok(false),
        }
    }

    /// Releases a context removed from `contexts`.
    pub unsafe fn destroy_context(&self, context: ImeContext) -> Result<bool, XError> {
        unsafe { self.destroy_contexts(std::iter::once(context)) }
    }

    /// Releases contexts removed from `contexts`. The server is checked once, not per context.
    pub unsafe fn destroy_contexts(
        &self,
        contexts: impl IntoIterator<Item = ImeContext>,
    ) -> Result<bool, XError> {
        if self.is_destroyed {
            // libX11 frees the contexts of a destroyed input method.
            return Ok(false);
        }
        if !self.server_reachable() {
            // The context stays alive in libX11 until the server window is destroyed, and the
            // server may still send preedit callbacks to it. This intentionally leaks the Rust
            // callback data for the process lifetime; freeing the XIC does not reclaim that Box.
            contexts.into_iter().for_each(mem::forget);
            return Ok(false);
        }
        // `XDestroyIC` frees the context in libX11 even when the server reports an error, so the
        // callback data can be dropped either way. Destroy every context before reporting.
        let mut result = Ok(true);
        for context in contexts {
            if let Err(err) = unsafe { destroy_ic(&self.xconn, context.ic) } {
                result = result.and(Err(err));
            }
        }
        result
    }

    pub unsafe fn destroy_all_contexts_if_necessary(&mut self) -> Result<bool, XError> {
        let contexts = mem::take(&mut self.contexts);
        unsafe { self.destroy_contexts(contexts.into_values().flatten()) }
    }
}

#[cfg(test)]
mod reconnect_tests {
    use super::Reconnect;

    #[test]
    fn an_immediate_callback_is_deferred_until_initialization_has_returned() {
        let mut retry = Reconnect::default();
        retry.request(); // Registration invoked the callback while im was None.
        retry.request(); // Coalesce callbacks; do not recurse into opening an IM.
        assert!(retry.take(false));
        retry.complete(false); // Only a local fallback opened.
        for _ in 0..100 {
            assert!(!retry.take(false)); // Keys/ordinary iterations cannot spin retries.
        }
        assert!(retry.take(true)); // A later XIM_SERVERS notification, even if Xlib call=True.
        retry.complete(true);
        assert!(!retry.take(true)); // A verified primary does not reopen on other advertisements.
    }

    #[test]
    fn a_failed_reconnect_remains_eligible_for_a_later_server_only() {
        let mut retry = Reconnect::default();
        retry.request();
        assert!(retry.take(false));
        // An error leaves the state waiting just as Ok(local) does.
        assert!(!retry.take(false));
        assert!(retry.take(true));
        assert!(!retry.take(false));
        retry.complete(true);
        assert!(!retry.take(false));
        retry.request(); // Destruction of this primary schedules one replacement.
        assert!(retry.take(false));
    }

    #[test]
    fn a_local_only_session_does_not_start_retrying_on_server_changes() {
        let mut retry = Reconnect::default();
        assert!(!retry.take(true));
        assert!(!retry.take(false));
    }
}
