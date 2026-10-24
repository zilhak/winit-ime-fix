//! XIM messages use the existing X connection, but never enter a libX11 XIM wait.
use super::super::{ffi, XConnection};
use std::sync::Arc;
use x11rb::protocol::xproto::{self, ConnectionExt, EventMask};
use x11rb::{connection::Connection, wrapper::ConnectionExt as _};
use xim_parser::{Request, XimWrite};

pub(super) const MAX_PACKET: usize = 256 * 1024;
type Result<T> = std::result::Result<T, String>;
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

pub(super) struct Transport {
    pub x: Arc<XConnection>,
    pub window: u32,
    pub owner: u32,
    pub selection: u32,
    previous_mask: EventMask,
    peer: u32,
    pub locales: u32,
    pub transport: u32,
    connect: u32,
    protocol: u32,
    more: u32,
    properties: [u32; 20],
    next_property: usize,
    partial: Vec<u8>,
}
impl Transport {
    pub fn open(x: Arc<XConnection>, name: Option<&str>) -> Result<Self> {
        let c = x.xcb_connection();
        let root = x.default_root().root;
        let atom = |s: &str| {
            c.intern_atom(false, s.as_bytes()).map_err(err)?.reply().map(|r| r.atom).map_err(err)
        };
        let servers = atom("XIM_SERVERS")?;
        let list = c
            .get_property(false, root, servers, xproto::AtomEnum::ATOM, 0, 1024)
            .map_err(err)?
            .reply()
            .map_err(err)?;
        if list.bytes_after != 0 {
            return Err("too many XIM servers".into());
        }
        let mut selected = None;
        for selection in list.value32().into_iter().flatten() {
            let n = c.get_atom_name(selection).map_err(err)?.reply().map_err(err)?.name;
            let Some(n) = n.strip_prefix(b"@server=") else {
                continue;
            };
            if name.is_some_and(|s| s.as_bytes() != n) {
                continue;
            }
            let owner = c.get_selection_owner(selection).map_err(err)?.reply().map_err(err)?.owner;
            if owner != 0 {
                selected = Some((selection, owner));
                break;
            }
        }
        let (selection, owner) = selected.ok_or("no XIM server")?;
        let locales = atom("LOCALES")?;
        let transport = atom("TRANSPORT")?;
        let connect = atom("_XIM_XCONNECT")?;
        let protocol = atom("_XIM_PROTOCOL")?;
        let more = atom("_XIM_MOREDATA")?;
        let mut properties = [0; 20];
        for (i, a) in properties.iter_mut().enumerate() {
            *a = atom(&format!("_WINIT_XIM_{i}"))?;
        }
        let previous_mask =
            c.get_window_attributes(owner).map_err(err)?.reply().map_err(err)?.your_event_mask;

        let window = c.generate_id().map_err(err)?;
        c.create_window(
            0,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            xproto::WindowClass::INPUT_ONLY,
            0,
            &Default::default(),
        )
        .map_err(err)?
        .check()
        .map_err(err)?;
        let this = Self {
            x,
            window,
            owner,
            selection,
            previous_mask,
            peer: 0,
            locales,
            transport,
            connect,
            protocol,
            more,
            properties,
            next_property: 0,
            partial: Vec::new(),
        };
        this.x
            .xcb_connection()
            .change_window_attributes(
                owner,
                &xproto::ChangeWindowAttributesAux::default()
                    .event_mask(previous_mask | EventMask::STRUCTURE_NOTIFY),
            )
            .map_err(err)?
            .check()
            .map_err(err)?;
        this.convert(transport)?;
        Ok(this)
    }
    pub fn alive(&self) -> Result<bool> {
        let c = self.x.xcb_connection();
        if c.get_selection_owner(self.selection).map_err(err)?.reply().map_err(err)?.owner
            != self.owner
        {
            return Ok(false);
        }
        Ok(c.get_window_attributes(self.owner)
            .map_err(err)?
            .reply()
            .map_err(err)?
            .your_event_mask
            .contains(EventMask::STRUCTURE_NOTIFY))
    }
    pub fn convert(&self, target: u32) -> Result<()> {
        let c = self.x.xcb_connection();
        c.convert_selection(self.window, self.selection, target, target, x11rb::CURRENT_TIME)
            .map_err(err)?
            .check()
            .map_err(err)?;
        c.flush().map_err(err)
    }
    fn cm(
        &self,
        target: u32,
        kind: u32,
        data: xproto::ClientMessageData,
        format: u8,
    ) -> Result<()> {
        let e = xproto::ClientMessageEvent {
            response_type: xproto::CLIENT_MESSAGE_EVENT,
            format,
            sequence: 0,
            window: target,
            type_: kind,
            data,
        };
        let c = self.x.xcb_connection();
        c.send_event(false, target, EventMask::NO_EVENT, e).map_err(err)?.check().map_err(err)?;
        c.flush().map_err(err)
    }
    pub fn xconnect(&self) -> Result<()> {
        self.cm(self.owner, self.connect, [self.window, 0, 0, 0, 0].into(), 32)
    }
    pub fn selection(&self, e: &ffi::XSelectionEvent) -> Result<Option<u32>> {
        if e.requestor as u32 != self.window {
            return Ok(None);
        }
        let target = e.target as u32;
        if e.property == 0 || (target != self.locales && target != self.transport) {
            return Err("XIM selection refused".into());
        }
        let c = self.x.xcb_connection();
        let r = c
            .get_property(true, self.window, e.property as u32, target, 0, 4096)
            .map_err(err)?
            .reply()
            .map_err(err)?;
        if r.bytes_after != 0 || r.format != 8 {
            return Err("invalid XIM negotiation".into());
        }
        if target == self.transport && !r.value.starts_with(b"@transport=X/") {
            return Err("unsupported XIM transport".into());
        }
        Ok(Some(target))
    }
    /// A complete packet, or a connect indication, belongs only to this generation's window.
    pub fn message(&mut self, e: &ffi::XClientMessageEvent) -> Result<Option<Request>> {
        if e.window as u32 != self.window {
            return Ok(None);
        }
        if e.message_type as u32 == self.connect {
            if e.format != 32 {
                return Err("invalid XIM connect".into());
            }
            let d = e.data.as_longs();
            if d[0] == 0 || d[1] != 0 {
                return Err("unsupported XIM connect".into());
            }
            self.peer = d[0] as u32;
            self.send(Request::Connect {
                client_major_protocol_version: 1,
                client_minor_protocol_version: 0,
                endian: xim_parser::Endian::Native,
                client_auth_protocol_names: vec![],
            })?;
            return Ok(None);
        }
        let kind = e.message_type as u32;
        if kind != self.protocol && kind != self.more {
            return Err("unknown XIM message".into());
        }
        if e.format == 8 {
            if self.partial.len() + 20 > MAX_PACKET {
                return Err("XIM packet limit".into());
            }
            self.partial.extend(e.data.as_bytes().iter().map(|n| n.to_ne_bytes()[0]));
            if kind == self.more {
                return Ok(None);
            }
        } else if e.format == 32 && self.partial.is_empty() && kind == self.protocol {
            let d = e.data.as_longs();
            let len = usize::try_from(d[0]).map_err(err)?;
            if !(4..=MAX_PACKET).contains(&len) {
                return Err("XIM property limit".into());
            }
            let r = self
                .x
                .xcb_connection()
                .get_property(
                    true,
                    self.window,
                    d[1] as u32,
                    xproto::AtomEnum::ANY,
                    0,
                    len.div_ceil(4) as u32,
                )
                .map_err(err)?
                .reply()
                .map_err(err)?;
            if r.bytes_after != 0 || r.format != 8 || r.value.len() != len {
                return Err("invalid XIM property".into());
            }
            self.partial = r.value;
        } else {
            return Err("invalid XIM packet format".into());
        }
        let result = decode(&self.partial);
        self.partial.clear();
        result.map(Some)
    }

    pub fn send(&mut self, request: Request) -> Result<()> {
        if !self.alive()? {
            return Err("XIM selection owner changed".into());
        }
        tracing::debug!("XIM send {}", request.name());
        let len = request.size();
        if len > MAX_PACKET || self.peer == 0 {
            return Err("XIM send bounds".into());
        }
        let data = xim_parser::write_to_vec(request);
        if data.len() <= 20 {
            let mut d = [0; 20];
            d[..data.len()].copy_from_slice(&data);
            self.cm(self.peer, self.protocol, d.into(), 8)
        } else {
            let prop = self.properties[self.next_property];
            self.next_property = (self.next_property + 1) % self.properties.len();
            self.x
                .xcb_connection()
                .change_property8(
                    xproto::PropMode::REPLACE,
                    self.peer,
                    prop,
                    xproto::AtomEnum::STRING,
                    &data,
                )
                .map_err(err)?
                .check()
                .map_err(err)?;
            self.cm(self.peer, self.protocol, [data.len() as u32, prop, 0, 0, 0].into(), 32)
        }
    }
}
impl Drop for Transport {
    fn drop(&mut self) {
        let c = self.x.xcb_connection();
        let restore = || -> Result<()> {
            if self.alive()? {
                c.change_window_attributes(
                    self.owner,
                    &xproto::ChangeWindowAttributesAux::default().event_mask(self.previous_mask),
                )
                .map_err(err)?
                .check()
                .map_err(err)?;
            }
            Ok(())
        };
        if let Err(e) = restore() {
            tracing::debug!("XIM owner already gone during release: {e}");
        }
        if let Err(e) = c
            .destroy_window(self.window)
            .map_err(err)
            .and_then(|cookie| cookie.check().map_err(err))
        {
            tracing::warn!("XIM window release failed: {e}");
        }
        if let Err(e) = c.flush() {
            tracing::warn!("XIM release flush failed: {e}");
        }
    }
}

fn decode(bytes: &[u8]) -> Result<Request> {
    let header = bytes.get(..4).ok_or("short XIM header")?;
    let size = 4 + usize::from(u16::from_ne_bytes([header[2], header[3]])) * 4;
    if size > MAX_PACKET || bytes.len() < size {
        return Err("truncated XIM packet".into());
    }
    let bytes = &bytes[..size];
    let synchronous =
        bytes.get(8..10).is_some_and(|flag| u16::from_ne_bytes([flag[0], flag[1]]) & 1 != 0);
    let mut result = xim_parser::read(bytes).map_err(err)?;
    // xim-parser0.2.2 swaps the synchronous tests of Chars/Keysym. Preserve the wire bit.
    if let Request::Commit { data, .. } = &mut result {
        match data {
            xim_parser::CommitData::Chars { syncronous, .. }
            | xim_parser::CommitData::Keysym { syncronous, .. }
            | xim_parser::CommitData::Both { syncronous, .. } => *syncronous = synchronous,
        }
    }
    Ok(result)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn truncated_header_cannot_read_padding_as_a_reply() {
        let mut data = xim_parser::write_to_vec(Request::ConnectReply {
            server_major_protocol_version: 1,
            server_minor_protocol_version: 0,
        });
        data[2..4].copy_from_slice(&0u16.to_ne_bytes());
        data.resize(20, 0);
        assert!(decode(&data).is_err());
    }
    #[test]
    fn exact_length_and_padded_client_messages_share_the_same_reply() {
        let r = Request::ConnectReply {
            server_major_protocol_version: 1,
            server_minor_protocol_version: 0,
        };
        let mut data = xim_parser::write_to_vec(r.clone());
        assert_eq!(decode(&data).unwrap(), r);
        data.resize(20, 0);
        assert_eq!(decode(&data).unwrap(), r);
        let truncated = &data[..5];
        assert!(decode(truncated).is_err());
    }
    #[test]
    fn commit_sync_flags_follow_the_native_wire_for_all_commit_variants() {
        use xim_parser::CommitData::*;
        for synchronous in [false, true] {
            for data in [
                Chars { commited: b"a".to_vec(), syncronous: synchronous },
                Keysym { keysym: 97, syncronous: synchronous },
                Both { keysym: 97, commited: b"a".to_vec(), syncronous: synchronous },
            ] {
                let r = Request::Commit { input_method_id: 1, input_context_id: 2, data };
                assert_eq!(decode(&xim_parser::write_to_vec(r.clone())).unwrap(), r);
            }
        }
    }
}
