use super::*;

impl Remote {
    pub(super) fn pump(&mut self) -> Result<(), String> {
        if self.pending.is_some() || self.transport.is_none() || self.style.is_none() {
            return Ok(());
        }
        let windows: Vec<_> = self.contexts.keys().copied().collect();
        for w in windows {
            let c = &self.contexts[&w];
            if c.removed {
                if let Some(id) = c.id {
                    self.send(
                        Request::DestroyIc { input_method_id: self.im, input_context_id: id },
                        Some(Wait::Destroy(id)),
                    )?;
                    return Ok(());
                }
                self.contexts.remove(&w);
                continue;
            }
            if c.reset {
                if let Some(id) = c.id {
                    self.send(
                        Request::DestroyIc { input_method_id: self.im, input_context_id: id },
                        Some(Wait::Destroy(id)),
                    )?;
                    return Ok(());
                }
            }
            if c.id.is_none() {
                let attrs = vec![
                    self.attr(AttributeName::InputStyle, self.style.unwrap())?,
                    self.attr(AttributeName::ClientWindow, w as u32)?,
                    self.attr(AttributeName::FocusWindow, w as u32)?,
                ];
                self.send(
                    Request::CreateIc { input_method_id: self.im, ic_attributes: attrs },
                    Some(Wait::Create(w)),
                )?;
                return Ok(());
            }
            let id = c.id.unwrap();
            let focus = c.focused && c.allowed;
            if focus != c.applied_focus {
                let r = if focus {
                    Request::SetIcFocus { input_method_id: self.im, input_context_id: id }
                } else {
                    Request::UnsetIcFocus { input_method_id: self.im, input_context_id: id }
                };
                self.send(r, None)?;
                self.contexts.get_mut(&w).unwrap().applied_focus = focus;
            }
            let c = &self.contexts[&w];
            if c.applied_spot != Some(c.spot) && self.keys.is_empty() {
                let (x, y) = c.spot;
                if self.attrs.contains_key(&AttributeName::SpotLocation)
                    && self.attrs.contains_key(&AttributeName::PreeditAttributes)
                {
                    let spot =
                        self.attr(AttributeName::SpotLocation, xim_parser::Point { x, y })?;
                    let nested = self.attr(AttributeName::PreeditAttributes, spot)?;
                    self.contexts.get_mut(&w).unwrap().applied_spot = Some((x, y));
                    self.send(
                        Request::SetIcValues {
                            input_method_id: self.im,
                            input_context_id: id,
                            ic_attributes: vec![nested],
                        },
                        Some(Wait::Spot(id)),
                    )?;
                    return Ok(());
                }
                self.contexts.get_mut(&w).unwrap().applied_spot = Some((x, y));
            }
        }
        if let Some(k) = self.keys.pop_front() {
            let Some(id) =
                self.contexts.get(&k.window).filter(|c| c.allowed && !c.removed).and_then(|c| c.id)
            else {
                self.replay_key(k);
                return Ok(());
            };
            let xev = xim_parser::XEvent {
                response_type: k.type_ as u8,
                detail: k.keycode as u8,
                sequence: k.serial as u16,
                time: k.time as u32,
                root: k.root as u32,
                event: k.window as u32,
                child: k.subwindow as u32,
                root_x: k.x_root as i16,
                root_y: k.y_root as i16,
                event_x: k.x as i16,
                event_y: k.y as i16,
                state: k.state as u16,
                same_screen: k.same_screen != 0,
            };
            self.send(
                Request::ForwardEvent {
                    input_method_id: self.im,
                    input_context_id: id,
                    flag: xim_parser::ForwardEventFlag::empty(),
                    serial_number: (k.serial >> 16) as u16,
                    xev,
                },
                None,
            )?;
            // IBus 1.5 ignores the client's ForwardEvent synchronous flag. A mandatory
            // IC query bounds transport progress without assuming key consumption/effect ACK.
            let attrs = self.attrs.get(&AttributeName::FilterEvents).copied().into_iter().collect();
            self.send(
                Request::GetIcValues {
                    input_method_id: self.im,
                    input_context_id: id,
                    ic_attributes: attrs,
                },
                Some(Wait::Key(id)),
            )?;
        }
        Ok(())
    }
}
