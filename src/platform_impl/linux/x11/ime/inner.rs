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
    /// which may replace `im` and `contexts`, so no reference into `*inner` may be held across
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
        // The destroy callback opened a new input method (a fallback or the new server). Its
        // server was recorded when it was opened.
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
            // server may still send preedit callbacks to it, so keep its callback data.
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
