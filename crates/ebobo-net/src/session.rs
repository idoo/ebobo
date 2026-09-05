//! GGRS P2P session driving an `ebobo_core::Nes` (`docs/PROTOCOL.md` section 5).
//! Owner: session slice.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use ebobo_core::Nes;
use ggrs::{DesyncDetection, GgrsError, GgrsEvent, GgrsRequest, InputStatus, P2PSession, PlayerType, SessionBuilder, SessionState};

use crate::transport::{PeerId, RouteKind, Transport};
use crate::{NetError, INPUT_RESET};

/// ggrs configuration: `Input = u16` (pad bits 0-7, `INPUT_RESET` bit 8),
/// `State = ebobo_core::Snapshot`, `Address = PeerId`.
pub struct EboboConfig;

impl ggrs::Config for EboboConfig {
    type Input = u16;
    type InputPredictor = ggrs::PredictRepeatLast;
    type State = ebobo_core::Snapshot;
    type Address = PeerId;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetEvent {
    Synchronizing { peer: PeerId, done: u32, total: u32 },
    Synchronized { peer: PeerId },
    /// No packets from `peer` for a while; disconnect after `disconnect_in_ms`.
    Interrupted { peer: PeerId, disconnect_in_ms: u64 },
    Resumed { peer: PeerId },
    /// Peer timed out or said BYE; its controller reads as released from now on.
    Disconnected { peer: PeerId },
    /// States diverged at `frame` (should never happen; shown as a warning).
    Desync { frame: i32 },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeerStats {
    pub peer: Option<PeerId>,
    pub slot: usize,
    pub route: Option<RouteKind>,
    pub ping_ms: u32,
    pub kbps_sent: u32,
    pub local_frames_behind: i32,
    pub remote_frames_behind: i32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetStats {
    /// One entry per remote peer.
    pub peers: Vec<PeerStats>,
    /// Frames re-simulated by the last `advance`.
    pub last_rollback: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameOutcome {
    /// A new frame was produced (false while synchronizing or when GGRS asked to wait).
    pub advanced: bool,
    /// Frames re-simulated because remote inputs differed from the prediction.
    pub rollback_frames: u32,
}

/// The ggrs socket: shares the transport with the session so `take_departed`/`close` stay
/// reachable after ggrs took ownership of its socket. Also counts payload bytes sent per peer
/// (ggrs 0.13 reports no bandwidth).
struct SharedSocket {
    transport: Rc<RefCell<Transport>>,
    sent: Rc<RefCell<[u64; 4]>>,
}

impl ggrs::NonBlockingSocket<PeerId> for SharedSocket {
    fn send_to(&mut self, msg: &ggrs::Message, addr: &PeerId) {
        if let (Some(total), Ok(len)) = (self.sent.borrow_mut().get_mut(addr.0 as usize), bincode::serialized_size(msg)) {
            *total += len;
        }
        self.transport.borrow_mut().send_to(msg, addr);
    }

    fn receive_all_messages(&mut self) -> Vec<(PeerId, ggrs::Message)> {
        self.transport.borrow_mut().receive_all_messages()
    }
}

const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DISCONNECT_NOTIFY: Duration = Duration::from_millis(1500);

pub struct NetSession {
    ggrs: P2PSession<EboboConfig>,
    transport: Rc<RefCell<Transport>>,
    sent: Rc<RefCell<[u64; 4]>>,
    started: Instant,
    slots: Vec<PeerId>,
    local: usize,
    pending: Vec<NetEvent>,
    last_rollback: u32,
    /// Tests: frame whose saved state hash is recorded in `probe_hash` (last save wins).
    #[cfg(test)]
    probe: (i32, Option<u64>),
}

fn io(e: GgrsError) -> NetError {
    NetError::Io(format!("ggrs: {e}"))
}

impl NetSession {
    /// Builds the GGRS P2P session: `slots.len()` players, local handle = index of
    /// `transport.me()` in `slots`, others remote. `fps` = 60 NTSC / 50 PAL.
    pub fn new(transport: Transport, slots: &[PeerId], input_delay: usize, fps: usize) -> Result<NetSession, NetError> {
        NetSession::with_timeouts(transport, slots, input_delay, fps, DISCONNECT_TIMEOUT, DISCONNECT_NOTIFY)
    }

    fn with_timeouts(
        transport: Transport,
        slots: &[PeerId],
        input_delay: usize,
        fps: usize,
        disconnect_timeout: Duration,
        notify_delay: Duration,
    ) -> Result<NetSession, NetError> {
        let me = transport.me();
        let local = slots
            .iter()
            .position(|&p| p == me)
            .ok_or_else(|| NetError::Io(format!("local peer {me:?} has no slot")))?;
        if slots.len() > crate::MAX_PLAYERS {
            return Err(NetError::Io(format!("{} players, at most {}", slots.len(), crate::MAX_PLAYERS)));
        }
        let mut builder = SessionBuilder::<EboboConfig>::new()
            .with_num_players(slots.len())
            .map_err(io)?
            .with_input_delay(input_delay)
            .with_max_prediction_window(8)
            .with_fps(fps)
            .map_err(io)?
            .with_desync_detection_mode(DesyncDetection::On { interval: 60 })
            .with_disconnect_timeout(disconnect_timeout)
            .with_disconnect_notify_delay(notify_delay);
        for (handle, &peer) in slots.iter().enumerate() {
            let kind = if handle == local { PlayerType::Local } else { PlayerType::Remote(peer) };
            builder = builder.add_player(kind, handle).map_err(io)?;
        }
        let transport = Rc::new(RefCell::new(transport));
        let sent = Rc::new(RefCell::new([0u64; 4]));
        let socket = SharedSocket { transport: transport.clone(), sent: sent.clone() };
        let ggrs = builder.start_p2p_session(socket).map_err(io)?;
        Ok(NetSession {
            ggrs,
            transport,
            sent,
            started: Instant::now(),
            slots: slots.to_vec(),
            local,
            pending: Vec::new(),
            last_rollback: 0,
            #[cfg(test)]
            probe: (NO_PROBE, None),
        })
    }

    /// NES controller index this machine plays.
    pub fn local_slot(&self) -> usize {
        self.local
    }

    /// True once every peer is synchronized.
    pub fn is_running(&self) -> bool {
        self.ggrs.current_state() == SessionState::Running
    }

    /// Network I/O; call every loop iteration, also while not advancing.
    pub fn poll(&mut self) {
        self.ggrs.poll_remote_clients();
        let departed = self.transport.borrow_mut().take_departed();
        for peer in departed {
            if let Some(handle) = self.slots.iter().position(|&p| p == peer) {
                if handle != self.local {
                    let _ = self.ggrs.disconnect_player(handle);
                    self.pending.push(NetEvent::Disconnected { peer });
                }
            }
        }
    }

    /// Adds the local input and advances one frame, handling GGRS save/load/advance requests on
    /// `nes`. Audio of re-simulated frames is dropped, only the newest frame's samples remain
    /// buffered in `nes`.
    pub fn advance(&mut self, nes: &mut Nes, local_input: u16) -> Result<FrameOutcome, NetError> {
        if !self.is_running() {
            self.poll();
            return Ok(FrameOutcome::default());
        }
        self.ggrs.add_local_input(self.local, local_input).map_err(io)?;
        let requests = match self.ggrs.advance_frame() {
            Ok(r) => r,
            Err(GgrsError::PredictionThreshold) => return Ok(FrameOutcome::default()),
            Err(e) => return Err(io(e)),
        };
        let advances = requests.iter().filter(|r| matches!(r, GgrsRequest::AdvanceFrame { .. })).count();
        let mut seen = 0;
        for request in requests {
            match request {
                GgrsRequest::SaveGameState { cell, frame } => {
                    let hash = nes.state_hash();
                    #[cfg(test)]
                    if frame == self.probe.0 {
                        self.probe.1 = Some(hash);
                    }
                    cell.save(frame, Some(nes.snapshot()), Some(hash as u128));
                }
                GgrsRequest::LoadGameState { cell, .. } => {
                    let snapshot = cell.load().expect("ggrs asked to load a state it never saved");
                    nes.restore(&snapshot);
                }
                GgrsRequest::AdvanceFrame { inputs } => {
                    let mut pads = [0u8; 4];
                    let mut reset = false;
                    for (slot, &(input, status)) in inputs.iter().enumerate().take(4) {
                        if status == InputStatus::Disconnected {
                            continue;
                        }
                        pads[slot] = input as u8;
                        reset |= input & INPUT_RESET != 0;
                    }
                    if reset {
                        nes.reset();
                    }
                    seen += 1;
                    // Re-simulated frames are never shown or heard: skip pixels and audio.
                    nes.set_fast_forward(seen < advances);
                    nes.run_frame(pads);
                    if seen < advances {
                        nes.clear_audio();
                    }
                }
            }
        }
        self.last_rollback = advances.saturating_sub(1) as u32;
        Ok(FrameOutcome { advanced: advances > 0, rollback_frames: self.last_rollback })
    }

    pub fn events(&mut self) -> Vec<NetEvent> {
        let mut out = std::mem::take(&mut self.pending);
        for event in self.ggrs.events() {
            out.push(match event {
                GgrsEvent::Synchronizing { addr, total, count } => NetEvent::Synchronizing { peer: addr, done: count, total },
                GgrsEvent::Synchronized { addr } => NetEvent::Synchronized { peer: addr },
                GgrsEvent::NetworkInterrupted { addr, disconnect_timeout } => {
                    NetEvent::Interrupted { peer: addr, disconnect_in_ms: disconnect_timeout as u64 }
                }
                GgrsEvent::NetworkResumed { addr } => NetEvent::Resumed { peer: addr },
                GgrsEvent::Disconnected { addr } => NetEvent::Disconnected { peer: addr },
                GgrsEvent::DesyncDetected { frame, .. } => NetEvent::Desync { frame },
                // Pacing goes through `frames_ahead`.
                GgrsEvent::WaitRecommendation { .. } => continue,
            });
        }
        out
    }

    pub fn stats(&self) -> NetStats {
        let transport = self.transport.borrow();
        let sent = self.sent.borrow();
        let secs = self.started.elapsed().as_secs_f64().max(1.0);
        let peers = self
            .slots
            .iter()
            .enumerate()
            .filter(|&(slot, _)| slot != self.local)
            .map(|(slot, &peer)| {
                let mut stats = PeerStats { peer: Some(peer), slot, route: transport.route_kind(peer), ..PeerStats::default() };
                let bytes = sent.get(peer.0 as usize).copied().unwrap_or(0);
                stats.kbps_sent = (bytes as f64 * 8.0 / 1000.0 / secs) as u32;
                if let Ok(n) = self.ggrs.network_stats(slot) {
                    stats.ping_ms = n.ping.min(u32::MAX as u128) as u32;
                    stats.local_frames_behind = n.local_frames_behind;
                    stats.remote_frames_behind = n.remote_frames_behind;
                }
                stats
            })
            .collect();
        NetStats { peers, last_rollback: self.last_rollback }
    }

    /// Frames we are ahead of the slowest peer (> 0: slow down a little).
    pub fn frames_ahead(&self) -> i32 {
        self.ggrs.frames_ahead()
    }

    /// Says BYE to every peer.
    pub fn leave(self) {
        self.transport.borrow_mut().close();
    }
}

#[cfg(test)]
const NO_PROBE: i32 = ggrs::NULL_FRAME;

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
