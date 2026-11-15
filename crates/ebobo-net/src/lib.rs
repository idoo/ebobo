//! ebobo-net: rooms, NAT hole punching, relay fallback, lobby and GGRS rollback sessions.
//! The wire protocol is specified in `docs/PROTOCOL.md`; this crate implements it.
//!
//! Module ownership (one owner per file):
//! - `packet.rs`: packet framing (lead, frozen).
//! - `transport.rs` + `stun.rs`: UDP/relay routes per peer, ggrs socket, STUN, local addresses.
//! - `mesh.rs`: signaling WebSocket, candidate exchange, hole punching, route decisions,
//!   direct-IP mode.
//! - `lobby.rs`: lobby protocol on top of the mesh (game info, ROM transfer, ready, start)
//!   running on a background thread.
//! - `session.rs`: GGRS P2P session driving an `ebobo_console::Console` (NES or Mega Drive).
//!
//! The netplay input is one `u32` per player and frame: the system's own pad bits in bits 0-15
//! (`INPUT_PAD`) and the `INPUT_*` action bits above them.

mod diag;
mod lobby;
mod mesh;
pub mod packet;
mod session;
mod stun;
mod transport;

use std::fmt;

pub use diag::{NatKind, NetDiag, diagnose};
pub use lobby::{GameStart, Lobby, LobbyConfig, LobbyMode, LobbyStatus, PlayerInfo, Resume, ResumeInfo, ResumeKind, RomState, Stage};
pub use session::{FrameOutcome, NetEvent, NetSession, NetStats, PeerStats};
pub use mesh::Link;
pub use transport::{PeerId, RouteKind};

/// Lobby `proto` value; peers with a different one refuse to play together.
pub const PROTOCOL_VERSION: u32 = 4;
pub const MAX_PLAYERS: usize = 4;
pub const DEFAULT_DIRECT_PORT: u16 = 7777;
/// Netplay input bits 0-15: the pad, in the system's own pad bits (`ebobo_console::nes::pad` in
/// the low byte, `ebobo_console::md::pad`).
pub const INPUT_PAD: u32 = 0xFFFF;
/// Netplay input bit 16: press the console's Reset button on this frame.
pub const INPUT_RESET: u32 = 1 << 16;
/// Netplay input bit 17: turbo A is held. Every peer turns it into A presses from the simulated
/// frame number (on when `(frame / 2) % 2 == 0`), so a held turbo button is a constant input.
/// NES only: other systems ignore it.
pub const INPUT_TURBO_A: u32 = 1 << 17;
/// Netplay input bit 18: turbo B is held; expanded like `INPUT_TURBO_A`.
pub const INPUT_TURBO_B: u32 = 1 << 18;
/// Netplay input bit 19: save the game for everyone on this frame (one frame, like reset).
pub const INPUT_SAVE: u32 = 1 << 19;
/// Netplay input bit 20: load the shared save for everyone on this frame (one frame).
pub const INPUT_LOAD: u32 = 1 << 20;
/// Room code alphabet: no I, O, 0, 1.
pub const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
pub const CODE_LEN: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetError {
    /// Server unreachable, TLS/HTTP/WebSocket failure (detail for logs).
    Server(String),
    /// Host: every random code we tried was taken.
    RoomTaken,
    NoRoom,
    RoomFull,
    RoomLocked,
    HostLeft,
    /// Peer speaks another protocol version.
    Version,
    /// Guest has no ROM with the host's CRC and the transfer failed or was refused.
    RomMissing,
    /// Received ROM did not match the announced CRC.
    RomCrc,
    /// The ROM could not be loaded by the emulator core.
    BadRom(String),
    /// The host's saved game did not reach a player, or arrived damaged.
    SaveTransfer,
    /// Direct mode: nobody answered.
    Timeout,
    /// Peer left or stopped answering.
    Disconnected,
    Io(String),
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetError::Server(detail) => write!(f, "signaling server error: {detail}"),
            NetError::RoomTaken => write!(f, "room code already taken"),
            NetError::NoRoom => write!(f, "no such room"),
            NetError::RoomFull => write!(f, "room is full"),
            NetError::RoomLocked => write!(f, "game already started"),
            NetError::HostLeft => write!(f, "host left"),
            NetError::Version => write!(f, "peer runs a different ebobo version"),
            NetError::RomMissing => write!(f, "ROM missing"),
            NetError::RomCrc => write!(f, "received ROM is corrupt"),
            NetError::BadRom(detail) => write!(f, "ROM cannot be loaded: {detail}"),
            NetError::SaveTransfer => write!(f, "saved game was not delivered"),
            NetError::Timeout => write!(f, "connection timed out"),
            NetError::Disconnected => write!(f, "peer disconnected"),
            NetError::Io(detail) => write!(f, "network error: {detail}"),
        }
    }
}

impl std::error::Error for NetError {}

/// Normalises a typed room code: drops whitespace and dashes, upper-cases, and checks the
/// alphabet and length. `None` when the result is not a valid code.
pub fn normalize_code(input: &str) -> Option<String> {
    let code: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let valid = code.len() == CODE_LEN && code.bytes().all(|b| CODE_ALPHABET.contains(&b));
    valid.then_some(code)
}

/// A fresh random room code.
pub fn random_code() -> String {
    let mut bytes = [0u8; CODE_LEN];
    getrandom::fill(&mut bytes).expect("OS random source");
    bytes
        .iter()
        .map(|b| CODE_ALPHABET[(*b as usize) % CODE_ALPHABET.len()] as char)
        .collect()
}

/// Random 64-bit token for punch packets.
pub(crate) fn random_token() -> u64 {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("OS random source");
    u64::from_le_bytes(bytes)
}
