//! Remote input is an event-driven protocol. Only local compose uses libX11 XIM objects.
mod reply;
mod requests;
mod transport;
use super::{ffi, ImeEvent, ImeEventSender, XConnection};
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};
use transport::Transport;
use xim_parser::{Attribute, AttributeName, InputStyle, Request, XimWrite};

const DEADLINE: Duration = Duration::from_secs(2);
const RETRY: Duration = Duration::from_secs(5);
const MAX_KEYS: usize = 64;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wait {
    Transport,
    Locales,
    Connect,
    Open,
    Encoding,
    Styles,
    Create(ffi::Window),
    Spot(u16),
    Destroy(u16),
    Key(u16),
}
struct Pending {
    kind: Wait,
    until: Instant,
}
#[derive(Default)]
struct Context {
    allowed: bool,
    focused: bool,
    id: Option<u16>,
    spot: (i16, i16),
    applied_spot: Option<(i16, i16)>,
    applied_focus: bool,
    text: Vec<char>,
    removed: bool,
    reset: bool,
    mask: u32,
}
pub(super) struct Remote {
    x: Arc<XConnection>,
    sender: ImeEventSender,
    name: Option<String>,
    transport: Option<Transport>,
    pending: Option<Pending>,
    retry: Instant,
    im: u16,
    style: Option<InputStyle>,
    attrs: HashMap<AttributeName, u16>,
    contexts: HashMap<ffi::Window, Context>,
    keys: VecDeque<ffi::XKeyEvent>,
    replay: VecDeque<ffi::XKeyEvent>,
    retired_owners: VecDeque<u32>,
}
impl Remote {
    pub fn new(x: Arc<XConnection>, sender: ImeEventSender) -> Option<Self> {
        let modifiers = std::env::var("XMODIFIERS").unwrap_or_default();
        let name = modifiers
            .split('@')
            .find_map(|s| s.strip_prefix("im="))
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        if name.as_deref().is_some_and(|s| ["local", "none"].contains(&s)) {
            return None;
        }
        let locale = unsafe { libc::setlocale(libc::LC_CTYPE, std::ptr::null()) };
        let thai = !locale.is_null()
            && unsafe { std::ffi::CStr::from_ptr(locale) }.to_bytes().starts_with(b"th");
        if thai
            && name
                .as_deref()
                .is_none_or(|n| ["BasicCheck", "Strict", "Thaicat", "Passthrough"].contains(&n))
        {
            return None;
        }
        Some(Self {
            x,
            sender,
            name,
            transport: None,
            pending: None,
            retry: Instant::now(),
            im: 0,
            style: None,
            attrs: HashMap::new(),
            contexts: HashMap::new(),
            keys: VecDeque::new(),
            replay: VecDeque::new(),
            retired_owners: VecDeque::new(),
        })
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.pending
            .as_ref()
            .map(|p| p.until)
            .or_else(|| self.transport.is_none().then_some(self.retry))
    }
    fn emit(&self, w: ffi::Window, e: ImeEvent) {
        if matches!(&e, ImeEvent::Start | ImeEvent::Update(..) | ImeEvent::Commit(..))
            && !self
                .contexts
                .get(&w)
                .is_some_and(|c| c.allowed && c.focused && !c.removed && !c.reset)
        {
            return;
        }
        if let Err(error) = self.sender.send((w, e)) {
            tracing::warn!("XIM event receiver closed: {error}");
        }
    }
    fn wait(&mut self, kind: Wait) {
        self.pending = Some(Pending { kind, until: Instant::now() + DEADLINE });
    }
    fn send(&mut self, r: Request, wait: Option<Wait>) -> Result<(), String> {
        self.transport.as_mut().ok_or("XIM disconnected")?.send(r)?;
        if let Some(w) = wait {
            self.wait(w);
        }
        Ok(())
    }
    fn attr(&self, name: AttributeName, value: impl XimWrite) -> Result<Attribute, String> {
        Ok(Attribute {
            id: *self.attrs.get(&name).ok_or("missing XIM attribute")?,
            value: xim_parser::write_to_vec(value),
        })
    }
    fn replay_key(&mut self, key: ffi::XKeyEvent) {
        let mut event = ffi::XEvent { key };
        unsafe {
            (self.x.xlib.XPutBackEvent)(self.x.display, &mut event);
        }
        self.replay.push_front(key);
    }
    fn retire(&mut self, reason: &str) {
        tracing::warn!("XIM connection retired: {reason}; local compose remains available");
        if let Some(t) = self.transport.as_ref() {
            if !self.retired_owners.contains(&t.owner) {
                self.retired_owners.push_back(t.owner);
                if self.retired_owners.len() > 32 {
                    self.retired_owners.pop_front();
                }
            }
        }
        self.transport = None;
        self.pending = None;
        self.im = 0;
        self.style = None;
        self.attrs.clear();
        self.retry = Instant::now() + RETRY;
        let windows: Vec<_> = self.contexts.keys().copied().collect();
        for w in windows {
            if let Some(c) = self.contexts.get_mut(&w) {
                c.id = None;
                c.applied_focus = false;
                c.applied_spot = None;
                c.text.clear();
            }
            self.emit(w, ImeEvent::End);
        }
        // Unsent keys can return to local compose. The sent-but-unacknowledged key has unknown
        // outcome and is never replayed: a prior commit must not be duplicated.
        self.contexts.retain(|_, c| !c.removed);
        while let Some(k) = self.keys.pop_back() {
            if self.contexts.get(&k.window).is_some_and(|c| c.focused && c.allowed) {
                self.replay_key(k);
            }
        }
    }
    pub fn tick(&mut self, advertised: bool) {
        let now = Instant::now();
        if advertised && self.transport.as_ref().is_some_and(|t| !t.alive().unwrap_or(false)) {
            self.retire("server owner changed");
        }
        if self.pending.as_ref().is_some_and(|p| now >= p.until) {
            self.retire("reply deadline expired");
        }
        if self.transport.is_none() && (advertised || now >= self.retry) {
            match Transport::open(self.x.clone(), self.name.as_deref()) {
                Ok(t) => {
                    self.transport = Some(t);
                    self.wait(Wait::Transport);
                },
                Err(e) => {
                    tracing::debug!("XIM unavailable: {e}");
                    self.retry = now + RETRY;
                },
            }
        }
        if let Err(e) = self.pump() {
            self.retire(&e);
        }
    }
    pub fn context(&mut self, w: ffi::Window, allowed: bool) {
        let c = self.contexts.entry(w).or_default();
        c.allowed = allowed;
        c.removed = false;
    }
    pub fn remove(&mut self, w: ffi::Window) {
        self.keys.retain(|k| k.window != w);
        if let Some(c) = self.contexts.get_mut(&w) {
            c.removed = true;
            c.allowed = false;
            c.focused = false;
        }
        if self.contexts.get(&w).is_some_and(|c| c.id.is_none())
            && self.pending.as_ref().map(|p| p.kind) != Some(Wait::Create(w))
        {
            self.contexts.remove(&w);
        }
    }
    pub fn focus(&mut self, w: ffi::Window, focus: bool) {
        if !focus {
            self.keys.retain(|k| k.window != w);
        }
        if let Some(c) = self.contexts.get_mut(&w) {
            c.focused = focus;
        }
    }
    pub fn spot(&mut self, w: ffi::Window, x: i16, y: i16) {
        if let Some(c) = self.contexts.get_mut(&w) {
            c.spot = (x, y);
        }
    }
    pub fn allowed(&mut self, w: ffi::Window, allowed: bool) {
        if let Some(c) = self.contexts.get_mut(&w) {
            if c.allowed != allowed && c.id.is_some() {
                c.reset = true;
            }
            c.allowed = allowed;
        }
    }
    pub fn filter(&mut self, e: &ffi::XEvent) -> Option<bool> {
        self.tick(false);
        let kind = e.get_type();
        if kind == ffi::KeyPress || kind == ffi::KeyRelease {
            let key = unsafe { e.key };
            if self.replay.front().is_some_and(|k| same_key(k, &key)) {
                self.replay.pop_front();
                return Some(
                    !self.contexts.get(&key.window).is_some_and(|c| c.focused && !c.removed),
                );
            }
            let c = self.contexts.get(&key.window)?;
            if !c.allowed || c.id.is_none() || self.transport.is_none() {
                return None;
            }
            if c.mask & (1 << (kind - 2)) == 0 {
                return Some(false);
            }
            self.keys.push_back(key);
            if self.keys.len() > MAX_KEYS {
                self.retire("key queue capacity");
            } else if let Err(error) = self.pump() {
                self.retire(&error);
            }
            return Some(true);
        }
        if kind == ffi::DestroyNotify {
            let w = unsafe { e.destroy_window.window } as u32;
            if self.transport.as_ref().is_some_and(|t| t.owner == w) {
                self.retire("server window destroyed");
                self.retired_owners.retain(|owner| *owner != w);
                return Some(true);
            }
            if self.retired_owners.contains(&w) {
                self.retired_owners.retain(|owner| *owner != w);
                return Some(true);
            }
        }
        let t = self.transport.as_ref()?;
        let window = unsafe { e.any.window } as u32;
        let ours = if kind == ffi::SelectionNotify {
            (unsafe { e.selection.requestor }) as u32 == t.window
        } else {
            window == t.window
        };
        if !ours {
            return None;
        }
        let result = self.consume(e);
        if let Err(error) = result {
            self.retire(&error);
        }
        if let Err(error) = self.pump() {
            self.retire(&error);
        }
        Some(true)
    }
    fn consume(&mut self, e: &ffi::XEvent) -> Result<(), String> {
        if e.get_type() == ffi::SelectionNotify {
            let t = self.transport.as_ref().ok_or("retired XIM")?;
            if let Some(target) = t.selection(unsafe { &e.selection })? {
                if target == t.transport {
                    t.convert(t.locales)?;
                    self.wait(Wait::Locales);
                } else {
                    t.xconnect()?;
                    self.wait(Wait::Connect);
                }
            }
        } else if e.get_type() == ffi::ClientMessage {
            if let Some(request) = self
                .transport
                .as_mut()
                .ok_or("retired XIM")?
                .message(unsafe { &e.client_message })?
            {
                self.reply(request)?;
            }
        }
        Ok(())
    }
}
fn same_key(a: &ffi::XKeyEvent, b: &ffi::XKeyEvent) -> bool {
    a.window == b.window
        && a.type_ == b.type_
        && a.serial == b.serial
        && a.time == b.time
        && a.keycode == b.keycode
        && a.state == b.state
}
