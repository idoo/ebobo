//! Packet framing shared by UDP and relay routes (`docs/PROTOCOL.md` section 3).
//! Frozen contract: every net module encodes and parses packets only through this file.

pub const MAGIC: u8 = 0xEB;
pub const MAX_PACKET: usize = 1200;
pub const ROM_CHUNK_DATA: usize = 1024;

pub mod kind {
    pub const PUNCH: u8 = 0x01;
    pub const PUNCH_ACK: u8 = 0x02;
    pub const KEEPALIVE: u8 = 0x03;
    pub const CTRL: u8 = 0x10;
    pub const CTRL_ACK: u8 = 0x11;
    pub const ROM_CHUNK: u8 = 0x20;
    pub const ROM_ACK: u8 = 0x21;
    pub const GGRS: u8 = 0x30;
    pub const BYE: u8 = 0x3F;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Packet<'a> {
    Punch { token: u64, from: u8, to: u8 },
    PunchAck { token: u64, from: u8, to: u8 },
    Keepalive,
    Ctrl { seq: u32, json: &'a [u8] },
    CtrlAck { seq: u32 },
    RomChunk { offset: u32, total: u32, data: &'a [u8] },
    RomAck { next: u32 },
    Ggrs(&'a [u8]),
    Bye,
}

impl<'a> Packet<'a> {
    /// Parses one packet; `None` for anything malformed or foreign.
    pub fn parse(buf: &'a [u8]) -> Option<Packet<'a>> {
        let (&magic, rest) = buf.split_first()?;
        if magic != MAGIC {
            return None;
        }
        let (&kind, body) = rest.split_first()?;
        let u32_at = |at: usize| -> Option<u32> {
            Some(u32::from_le_bytes(body.get(at..at + 4)?.try_into().ok()?))
        };
        match kind {
            kind::PUNCH | kind::PUNCH_ACK => {
                if body.len() != 10 {
                    return None;
                }
                let token = u64::from_le_bytes(body[..8].try_into().ok()?);
                let (from, to) = (body[8], body[9]);
                Some(if kind == kind::PUNCH {
                    Packet::Punch { token, from, to }
                } else {
                    Packet::PunchAck { token, from, to }
                })
            }
            kind::KEEPALIVE => body.is_empty().then_some(Packet::Keepalive),
            kind::CTRL => Some(Packet::Ctrl { seq: u32_at(0)?, json: &body[4..] }),
            kind::CTRL_ACK => (body.len() == 4).then_some(Packet::CtrlAck { seq: u32_at(0)? }),
            kind::ROM_CHUNK => {
                let data = body.get(8..)?;
                if data.len() > ROM_CHUNK_DATA {
                    return None;
                }
                Some(Packet::RomChunk { offset: u32_at(0)?, total: u32_at(4)?, data })
            }
            kind::ROM_ACK => (body.len() == 4).then_some(Packet::RomAck { next: u32_at(0)? }),
            kind::GGRS => Some(Packet::Ggrs(body)),
            kind::BYE => body.is_empty().then_some(Packet::Bye),
            _ => None,
        }
    }

    /// Appends the encoded packet to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.push(MAGIC);
        match *self {
            Packet::Punch { token, from, to } | Packet::PunchAck { token, from, to } => {
                let kind = if matches!(self, Packet::Punch { .. }) { kind::PUNCH } else { kind::PUNCH_ACK };
                out.push(kind);
                out.extend_from_slice(&token.to_le_bytes());
                out.push(from);
                out.push(to);
            }
            Packet::Keepalive => out.push(kind::KEEPALIVE),
            Packet::Ctrl { seq, json } => {
                out.push(kind::CTRL);
                out.extend_from_slice(&seq.to_le_bytes());
                out.extend_from_slice(json);
            }
            Packet::CtrlAck { seq } => {
                out.push(kind::CTRL_ACK);
                out.extend_from_slice(&seq.to_le_bytes());
            }
            Packet::RomChunk { offset, total, data } => {
                out.push(kind::ROM_CHUNK);
                out.extend_from_slice(&offset.to_le_bytes());
                out.extend_from_slice(&total.to_le_bytes());
                out.extend_from_slice(data);
            }
            Packet::RomAck { next } => {
                out.push(kind::ROM_ACK);
                out.extend_from_slice(&next.to_le_bytes());
            }
            Packet::Ggrs(body) => {
                out.push(kind::GGRS);
                out.extend_from_slice(body);
            }
            Packet::Bye => out.push(kind::BYE),
        }
    }

    pub fn to_vec(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16);
        self.encode(&mut out);
        out
    }
}
