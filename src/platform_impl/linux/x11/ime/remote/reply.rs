use super::*;
use xim_parser::{CommitData, ForwardEventFlag};
impl Remote {
    fn complete(&mut self, expected: Wait) -> Result<(), String> {
        if self.pending.as_ref().map(|p| p.kind) != Some(expected) {
            return Err("unexpected XIM reply".into());
        }
        self.pending = None;
        Ok(())
    }
    fn context_window(&self, im: u16, ic: u16) -> Result<ffi::Window, String> {
        if im != self.im {
            return Err("stale XIM method".into());
        }
        self.contexts
            .iter()
            .find_map(|(w, c)| (c.id == Some(ic)).then_some(*w))
            .ok_or_else(|| "stale XIM context".into())
    }
    fn sync(&mut self, im: u16, ic: u16) -> Result<(), String> {
        self.send(Request::SyncReply { input_method_id: im, input_context_id: ic }, None)
    }
    pub(super) fn reply(&mut self, r: Request) -> Result<(), String> {
        tracing::debug!("XIM receive {}", r.name());
        use Request::*;
        match r {
            ConnectReply { .. } => {
                self.complete(Wait::Connect)?;
                let locale = unsafe {
                    std::ffi::CStr::from_ptr(libc::setlocale(libc::LC_CTYPE, std::ptr::null()))
                }
                .to_string_lossy()
                .into_owned();
                self.send(Open { locale }, Some(Wait::Open))?;
            },
            OpenReply { input_method_id, im_attrs, ic_attrs } => {
                self.complete(Wait::Open)?;
                self.im = input_method_id;
                let query = im_attrs
                    .iter()
                    .find(|a| a.name == AttributeName::QueryInputStyle)
                    .ok_or("missing XIM styles attribute")?
                    .id;
                self.attrs = ic_attrs.into_iter().map(|a| (a.name, a.id)).collect();
                // Save the server's IM attribute separately from the IC attribute namespace.
                self.attrs.insert(AttributeName::QueryInputStyle, query);
                self.send(
                    EncodingNegotiation {
                        input_method_id,
                        encodings: vec!["COMPOUND_TEXT".into()],
                        encoding_infos: vec![],
                    },
                    Some(Wait::Encoding),
                )?;
            },
            EncodingNegotiationReply { input_method_id, index, category } => {
                if input_method_id != self.im || index != 0 || category != 0 {
                    return Err("unsupported XIM encoding".into());
                }
                self.complete(Wait::Encoding)?;
                self.send(
                    GetImValues {
                        input_method_id,
                        im_attributes: vec![self.attrs[&AttributeName::QueryInputStyle]],
                    },
                    Some(Wait::Styles),
                )?;
            },
            GetImValuesReply { input_method_id, im_attributes } => {
                if input_method_id != self.im {
                    return Err("stale XIM style reply".into());
                }
                self.complete(Wait::Styles)?;
                let a = im_attributes
                    .iter()
                    .find(|a| a.id == self.attrs[&AttributeName::QueryInputStyle])
                    .ok_or("missing XIM styles")?;
                let list: xim_parser::InputStyleList =
                    xim_parser::read(&a.value).map_err(|e| e.to_string())?;
                let preferred = InputStyle::PREEDIT_CALLBACKS | InputStyle::STATUS_NOTHING;
                self.style = [
                    preferred,
                    InputStyle::PREEDIT_NOTHING | InputStyle::STATUS_NOTHING,
                    InputStyle::PREEDIT_NONE | InputStyle::STATUS_NONE,
                ]
                .into_iter()
                .find(|s| list.styles.contains(s));
                if self.style.is_none() {
                    return Err("no supported XIM style".into());
                }
                tracing::info!("asynchronous XIM input connected");
            },
            CreateIcReply { input_method_id, input_context_id } => {
                if input_method_id != self.im || input_context_id == 0 {
                    return Err("invalid XIM context".into());
                }
                let Some(Wait::Create(w)) = self.pending.as_ref().map(|p| p.kind) else {
                    return Err("unexpected context reply".into());
                };
                // An IC remains owned until its destroy is confirmed, even if its window was
                // removed. Reject aliases before completing the pending request or changing owners.
                if self.contexts.values().any(|c| c.id == Some(input_context_id)) {
                    return Err("duplicate live XIM context".into());
                }
                self.complete(Wait::Create(w))?;
                if let Some(c) = self.contexts.get_mut(&w) {
                    c.id = Some(input_context_id);
                    c.mask = 3;
                }
            },
            SetIcValuesReply { input_method_id, input_context_id } => {
                self.context_window(input_method_id, input_context_id)?;
                self.complete(Wait::Spot(input_context_id))?;
            },
            DestroyIcReply { input_method_id, input_context_id } => {
                if input_method_id != self.im {
                    return Err("stale destroy reply".into());
                }
                self.complete(Wait::Destroy(input_context_id))?;
                self.contexts.retain(|_, c| c.id != Some(input_context_id) || !c.removed);
                for c in self.contexts.values_mut().filter(|c| c.id == Some(input_context_id)) {
                    c.id = None;
                    c.reset = false;
                    c.applied_focus = false;
                    c.applied_spot = None;
                    c.text.clear();
                }
            },
            GetIcValuesReply { input_method_id, input_context_id, .. } => {
                self.context_window(input_method_id, input_context_id)?;
                self.complete(Wait::Key(input_context_id))?;
            },
            SetEventMask { input_method_id, input_context_id, forward_event_mask, .. } => {
                tracing::debug!(forward_event_mask, "XIM filter mask changed");
                if input_method_id != self.im {
                    return Err("stale event mask".into());
                }
                if let Some(c) = self.contexts.values_mut().find(|c| c.id == Some(input_context_id))
                {
                    c.mask = forward_event_mask;
                }
                // Some servers send this immediately before CreateIcReply; the default mask
                // until that reply forwards both key transitions safely.
            },
            ForwardEvent { input_method_id, input_context_id, flag, xev, .. } => {
                let w = self.context_window(input_method_id, input_context_id)?;
                if xev.event as ffi::Window != w || !matches!(xev.response_type, 2 | 3) {
                    return Err("invalid forwarded key".into());
                }
                let key = ffi::XKeyEvent {
                    type_: xev.response_type as i32,
                    display: self.x.display,
                    window: w,
                    root: xev.root as _,
                    subwindow: xev.child as _,
                    time: xev.time as _,
                    x: xev.event_x as _,
                    y: xev.event_y as _,
                    x_root: xev.root_x as _,
                    y_root: xev.root_y as _,
                    state: xev.state as _,
                    keycode: xev.detail as _,
                    same_screen: i32::from(xev.same_screen),
                    serial: xev.sequence as _,
                    send_event: 0,
                };
                self.replay_key(key, ReplayRoute::ServerFiltered);
                if flag.contains(ForwardEventFlag::SYNCHRONOUS) {
                    self.sync(input_method_id, input_context_id)?;
                }
            },
            Commit { input_method_id, input_context_id, data } => {
                let w = self.context_window(input_method_id, input_context_id)?;
                let (text, synchronous) = match data {
                    CommitData::Chars { commited, syncronous }
                    | CommitData::Both { commited, syncronous, .. } => (
                        xim_ctext::compound_text_to_utf8(&commited)
                            .map_err(|e| format!("invalid XIM commit: {e:?}"))?,
                        syncronous,
                    ),
                    // The existing Xutf8LookupString path publishes text, not keysym-only
                    // lookup results. Do not manufacture or replay a different physical key.
                    CommitData::Keysym { syncronous, .. } => (String::new(), syncronous),
                };
                self.emit(w, ImeEvent::End);
                if !text.is_empty() {
                    self.emit(w, ImeEvent::Commit(text));
                }
                if synchronous {
                    self.sync(input_method_id, input_context_id)?;
                }
            },
            Sync { input_method_id, input_context_id } => {
                self.context_window(input_method_id, input_context_id)?;
                self.sync(input_method_id, input_context_id)?;
            },
            PreeditStart { input_method_id, input_context_id } => {
                let w = self.context_window(input_method_id, input_context_id)?;
                self.contexts.get_mut(&w).unwrap().text.clear();
                self.emit(w, ImeEvent::Start);
                self.send(
                    PreeditStartReply { input_method_id, input_context_id, return_value: -1 },
                    None,
                )?;
            },
            PreeditDone { input_method_id, input_context_id } => {
                let w = self.context_window(input_method_id, input_context_id)?;
                self.contexts.get_mut(&w).unwrap().text.clear();
                self.emit(w, ImeEvent::End);
            },
            PreeditDraw {
                input_method_id,
                input_context_id,
                caret,
                chg_first,
                chg_length,
                preedit_string,
                ..
            } => {
                let w = self.context_window(input_method_id, input_context_id)?;
                let new = xim_ctext::compound_text_to_utf8(&preedit_string)
                    .map_err(|e| format!("invalid XIM preedit: {e:?}"))?;
                let c = self.contexts.get_mut(&w).unwrap();
                let start = usize::try_from(chg_first).map_err(|_| "negative XIM range")?;
                let end = start
                    .checked_add(usize::try_from(chg_length).map_err(|_| "negative XIM length")?)
                    .filter(|end| *end <= c.text.len())
                    .ok_or("invalid XIM range")?;
                if c.text.len() - (end - start) + new.chars().count() > 65536 {
                    return Err("XIM preedit capacity".into());
                }
                c.text.splice(start..end, new.chars());
                let pos =
                    usize::try_from(caret).map_err(|_| "negative XIM caret")?.min(c.text.len());
                let byte = c.text.iter().take(pos).map(|c| c.len_utf8()).sum();
                let text = c.text.iter().collect();
                self.emit(w, ImeEvent::Update(text, byte));
            },
            PreeditCaret { input_method_id, input_context_id, position, .. } => {
                self.context_window(input_method_id, input_context_id)?;
                self.send(PreeditCaretReply { input_method_id, input_context_id, position }, None)?;
            },
            Error { code, detail, .. } => return Err(format!("XIM error {code:?}: {detail}")),
            Disconnect {} | DisconnectReply {} => return Err("XIM disconnected".into()),
            // Unsupported trigger-only styles are excluded by style negotiation.
            _ => {},
        }
        Ok(())
    }
}
