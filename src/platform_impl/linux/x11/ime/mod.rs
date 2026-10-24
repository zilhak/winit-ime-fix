// Important: all XIM calls need to happen from the same thread!

mod callbacks;
mod context;
mod inner;
mod input_method;
mod remote;
mod server;

use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use self::callbacks::*;
use self::context::ImeContext;
pub use self::context::ImeContextCreationError;
use self::inner::{close_im, ImeInner};
use self::input_method::PotentialInputMethods;
use super::{ffi, util, XConnection, XError};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum ImeEvent {
    Enabled,
    Start,
    Update(String, usize),
    End,
    Disabled,
    Commit(String),
}

pub type ImeReceiver = Receiver<ImeRequest>;
pub type ImeSender = Sender<ImeRequest>;
pub type ImeEventReceiver = Receiver<(ffi::Window, ImeEvent)>;
pub type ImeEventSender = Sender<(ffi::Window, ImeEvent)>;

/// Request to control XIM handler from the window.
pub enum ImeRequest {
    /// Set IME spot position for given `window_id`.
    Position(ffi::Window, i16, i16),

    /// Allow IME input for the given `window_id`.
    Allow(ffi::Window, bool),
}

#[derive(Debug)]
pub(crate) enum ImeCreationError {
    // Boxed to prevent large error type
    OpenFailure(Box<PotentialInputMethods>),
    SetDestroyCallbackFailed(#[allow(dead_code)] XError),
}

pub(crate) struct Ime {
    xconn: Arc<XConnection>,
    // The actual meat of this struct is boxed away, since it needs to have a fixed location in
    // memory so we can pass a pointer to it around.
    inner: Box<ImeInner>,
    remote: Option<remote::Remote>,
}

impl Ime {
    pub fn new(
        xconn: Arc<XConnection>,
        event_sender: ImeEventSender,
    ) -> Result<Self, ImeCreationError> {
        let remote = remote::Remote::new(xconn.clone(), event_sender.clone());
        let potential_input_methods = PotentialInputMethods::new(&xconn, remote.is_some());

        let (mut inner, client_data) = {
            let mut inner = Box::new(ImeInner::new(xconn, potential_input_methods, event_sender));
            let inner_ptr = Box::into_raw(inner);
            let client_data = inner_ptr as _;
            let destroy_callback =
                ffi::XIMCallback { client_data, callback: Some(xim_destroy_callback) };
            inner = unsafe { Box::from_raw(inner_ptr) };
            inner.destroy_callback = destroy_callback;
            (inner, client_data)
        };

        let xconn = Arc::clone(&inner.xconn);

        let input_method = inner.potential_input_methods.open_im(
            &xconn,
            Some(&|| {
                let _ = unsafe { set_instantiate_callback(&xconn, client_data) };
            }),
        );

        let is_fallback = input_method.is_fallback();
        if let Some(input_method) = input_method.ok() {
            inner.is_fallback = is_fallback;
            unsafe {
                let result = set_destroy_callback(&xconn, input_method.im, &inner)
                    .map_err(ImeCreationError::SetDestroyCallbackFailed);
                // Closing waits for the server; skip it if the server already changed.
                if result.is_err() && input_method.server.owners_unchanged(&xconn) {
                    let _ = close_im(&xconn, input_method.im);
                }
                result?;
            }
            inner.im = Some(input_method);
            // The initial open and its registration callback have fully returned. A pending
            // retry can now borrow PotentialInputMethods without reentering the initial borrow.
            unsafe { retry_reconnect(&mut *inner, false) };
            Ok(Ime { xconn, inner, remote })
        } else {
            Err(ImeCreationError::OpenFailure(Box::new(inner.potential_input_methods)))
        }
    }

    pub fn is_destroyed(&self) -> bool {
        self.inner.is_destroyed
    }

    /// Whether synchronous XIM requests can be sent now. If the server selection changed owner,
    /// a queued `DestroyNotify` of the server window is handled first, which may replace the input
    /// method and every context; callers look them up again afterwards.
    pub fn ensure_server_alive(&mut self) -> bool {
        let inner: *mut ImeInner = &mut *self.inner;
        let alive = unsafe { ImeInner::ensure_server_alive(inner) };
        // A queued DestroyNotify may have requested a reconnect. XFilterEvent has returned.
        if unsafe { retry_reconnect(inner, false) } {
            self.inner.server_reachable()
        } else {
            alive
        }
    }

    /// Runs at the outer event boundary, after any Xlib callback and before IME requests.
    pub fn io_deadline(&self) -> Option<std::time::Instant> {
        self.remote.as_ref().and_then(|r| r.deadline())
    }
    pub fn filter_remote(&mut self, event: &ffi::XEvent) -> Option<bool> {
        self.remote.as_mut().and_then(|r| r.filter(event))
    }
    pub fn retry_reconnect(&mut self, server_changed: bool) {
        if let Some(remote) = self.remote.as_mut() {
            remote.tick(server_changed);
        }
        unsafe { retry_reconnect(&mut *self.inner, server_changed) };
    }

    // This pattern is used for various methods here:
    // Ok(_) indicates that nothing went wrong internally
    // Ok(true) indicates that the action was actually performed
    // Ok(false) indicates that the action is not presently applicable
    pub fn create_context(
        &mut self,
        window: ffi::Window,
        with_ime: bool,
    ) -> Result<bool, ImeContextCreationError> {
        if let Some(r) = self.remote.as_mut() {
            r.context(window, with_ime);
        }
        let context = if !self.ensure_server_alive() {
            // Create empty entry in map, so that when IME is rebuilt, this window has a context.
            None
        } else {
            let im = self.inner.im.as_ref().unwrap();

            let context = unsafe {
                ImeContext::new(
                    &self.inner.xconn,
                    im,
                    window,
                    None,
                    self.inner.event_sender.clone(),
                    with_ime,
                )?
            };

            let event = if context.is_allowed() { ImeEvent::Enabled } else { ImeEvent::Disabled };
            self.inner.event_sender.send((window, event)).expect("Failed to send enabled event");

            Some(context)
        };

        let created = context.is_some();
        self.inner.contexts.insert(window, context);
        Ok(created)
    }

    pub fn get_context(&self, window: ffi::Window) -> Option<ffi::XIC> {
        if self.is_destroyed() {
            return None;
        }
        if let Some(Some(context)) = self.inner.contexts.get(&window) {
            Some(context.ic)
        } else {
            None
        }
    }

    pub fn remove_context(&mut self, window: ffi::Window) -> Result<bool, XError> {
        if let Some(r) = self.remote.as_mut() {
            r.remove(window);
        }
        self.ensure_server_alive();
        if let Some(Some(context)) = self.inner.contexts.remove(&window) {
            unsafe {
                self.inner.destroy_context(context)?;
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn focus(&mut self, window: ffi::Window) -> Result<bool, XError> {
        if let Some(r) = self.remote.as_mut() {
            r.focus(window, true);
        }
        if !self.ensure_server_alive() {
            return Ok(false);
        }
        if let Some(&mut Some(ref mut context)) = self.inner.contexts.get_mut(&window) {
            context.focus(&self.xconn).map(|_| true)
        } else {
            Ok(false)
        }
    }

    pub fn unfocus(&mut self, window: ffi::Window) -> Result<bool, XError> {
        if let Some(r) = self.remote.as_mut() {
            r.focus(window, false);
        }
        if !self.ensure_server_alive() {
            return Ok(false);
        }
        if let Some(&mut Some(ref mut context)) = self.inner.contexts.get_mut(&window) {
            context.unfocus(&self.xconn).map(|_| true)
        } else {
            Ok(false)
        }
    }

    pub fn send_xim_spot(&mut self, window: ffi::Window, x: i16, y: i16) {
        if let Some(r) = self.remote.as_mut() {
            r.spot(window, x, y);
        }
        if self.is_destroyed() {
            return;
        }
        let alive = self.ensure_server_alive();
        if let Some(&mut Some(ref mut context)) = self.inner.contexts.get_mut(&window) {
            if alive {
                context.set_spot(&self.xconn, x as _, y as _);
            } else {
                // Applied when the contexts are recreated for a new input method.
                context.ic_spot = ffi::XPoint { x: x as _, y: y as _ };
            }
        }
    }

    pub fn set_ime_allowed(&mut self, window: ffi::Window, allowed: bool) {
        if let Some(r) = self.remote.as_mut() {
            r.allowed(window, allowed);
        }
        if self.is_destroyed() {
            return;
        }

        if !self.ensure_server_alive() {
            // Applied when the contexts are recreated for a new input method.
            if let Some(&mut Some(ref mut context)) = self.inner.contexts.get_mut(&window) {
                context.allowed = allowed;
            }
            return;
        }

        if let Some(&mut Some(ref mut context)) = self.inner.contexts.get_mut(&window) {
            if allowed == context.is_allowed() {
                return;
            }
        }

        // Rebuild only the local context; the asynchronous context keeps its identity.
        if let Some(Some(context)) = self.inner.contexts.remove(&window) {
            if let Err(error) = unsafe { self.inner.destroy_context(context) } {
                tracing::warn!("local input context replacement failed: {error:?}");
            }
        }

        // Create new context supporting IME input.
        let _ = self.create_context(window, allowed);
    }

    pub fn is_ime_allowed(&self, window: ffi::Window) -> bool {
        if self.is_destroyed() {
            false
        } else if let Some(Some(context)) = self.inner.contexts.get(&window) {
            context.is_allowed()
        } else {
            false
        }
    }
}

impl Drop for Ime {
    fn drop(&mut self) {
        unsafe {
            let _ = self.inner.destroy_all_contexts_if_necessary();
            let _ = self.inner.close_im_if_necessary();
        }
    }
}
