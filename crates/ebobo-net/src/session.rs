//! GGRS P2P session driving an `ebobo_console::Console` (`docs/PROTOCOL.md` section 5).
//! Owner: session slice.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use ebobo_console::{Console, Snapshot, System};
use ggrs::{DesyncDetection, GgrsError, GgrsEvent, GgrsRequest, InputStatus, P2PSession, PlayerType, SessionBuilder, SessionState};

use crate::lobby::{auto_input_delay, RttEstimator};
use crate::mesh::Link;
use crate::transport::{PeerId, RouteKind};
use crate::{NetError, INPUT_LOAD, INPUT_PAD, INPUT_RESET, INPUT_SAVE, INPUT_TURBO_A, INPUT_TURBO_B};

/// ggrs configuration: `Input = u32` (pad bits 0-15 `INPUT_PAD`, `INPUT_RESET` bit 16,
/// `INPUT_TURBO_A` bit 17, `INPUT_TURBO_B` bit 18, `INPUT_SAVE` bit 19, `INPUT_LOAD` bit 20),
/// `State = ebobo_console::Snapshot`, `Address = PeerId`.
pub struct EboboConfig;

impl ggrs::Config for EboboConfig {
    type Input = u32;
    type InputPredictor = PredictHeld;
    type State = Snapshot;
    type Address = PeerId;
}

/// Bits that act on the one frame they are sent for: reset, save for everyone, load for everyone.
const ONE_FRAME_BITS: u32 = INPUT_RESET | INPUT_SAVE | INPUT_LOAD;

/// Repeats the last input (the GGPO standard) without the one-frame bits (`ONE_FRAME_BITS`):
/// they are never held, so predicting them again would always miss. A set one-frame bit
/// therefore always comes from an input its player really sent. Turbo bits are held bits and
/// repeat.
pub struct PredictHeld;

impl ggrs::InputPredictor<u32> for PredictHeld {
    fn predict(previous: u32) -> u32 {
        previous & !ONE_FRAME_BITS
    }
}

/// NES pad bits for netplay input word `input` on simulated frame `frame`: the held pad bits
/// (`INPUT_PAD`), plus A / B while their turbo bit is set and the turbo phase
/// `(frame / 2) % 2 == 0` is on. Every peer derives the phase from the GGRS frame, so
/// re-simulated frames match. Only for `System::Nes`: other systems ignore the turbo bits.
pub(crate) fn expand_turbo(input: u32, frame: i32) -> u16 {
    let mut pad = (input & INPUT_PAD) as u16;
    if (frame / 2) % 2 == 0 {
        if input & INPUT_TURBO_A != 0 {
            pad |= ebobo_console::nes::pad::A as u16;
        }
        if input & INPUT_TURBO_B != 0 {
            pad |= ebobo_console::nes::pad::B as u16;
        }
    }
    pad
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
    /// The player on controller `slot` saved the game for everyone (`INPUT_SAVE`). Reported
    /// once per save, when its frame is confirmed; `take_saved` then returns the state.
    Saved { slot: usize },
    /// The player on controller `slot` loaded the shared save for everyone (`INPUT_LOAD`).
    /// Reported once per load, never again when its frame is re-simulated.
    Loaded { slot: usize },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeerStats {
    pub peer: Option<PeerId>,
    pub slot: usize,
    pub route: Option<RouteKind>,
    /// Smoothed RTT (RFC 6298 SRTT over GGRS pings).
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
    /// This machine's current input delay in frames.
    pub input_delay: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameOutcome {
    /// A new frame was produced (false while synchronizing or when GGRS asked to wait).
    pub advanced: bool,
    /// Frames re-simulated because remote inputs differed from the prediction.
    pub rollback_frames: u32,
}

/// The ggrs socket: shares the link with the session so `take_departed`/`stats`/`leave` stay
/// reachable after ggrs took ownership of its socket. Also counts payload bytes sent per peer
/// (ggrs 0.13 reports no bandwidth).
struct SharedSocket {
    link: Rc<RefCell<Option<Link>>>,
    sent: Rc<RefCell<[u64; 4]>>,
}

impl ggrs::NonBlockingSocket<PeerId> for SharedSocket {
    fn send_to(&mut self, msg: &ggrs::Message, addr: &PeerId) {
        if let (Some(total), Ok(len)) = (self.sent.borrow_mut().get_mut(addr.0 as usize), bincode::serialized_size(msg)) {
            *total += len;
        }
        if let Some(link) = self.link.borrow_mut().as_mut() {
            link.send_to(msg, addr);
        }
    }

    /// Also drives the link (signaling, route health, punches) on every GGRS poll.
    fn receive_all_messages(&mut self) -> Vec<(PeerId, ggrs::Message)> {
        self.link.borrow_mut().as_mut().map_or_else(Vec::new, |l| l.receive_all_messages())
    }
}

// Tuned with `session_profile_matrix` (session_tests.rs; 8 s runs, 60 fps pacing, random input
// every frame) on mobile (60±30 ms, bursty loss), far (150±20 ms), asym (15/180 ms) and a 4 s
// blackout, 2 and 4 peers, input delay 1-6:
// - prediction 10: far 2p stalls drop from 14-33 % (window 8) to 4-11 %, mobile 2p delay 1 from
//   2.4 % to 1.9 %; 12 gains only ~1 point more but a 12-frame rollback costs 12.3 ms mean /
//   14.6 ms worst on the core, nearly the whole 16.6 ms frame, against ~10 ms for 10.
// - timeout 20 s: the 4 s blackout never disconnected (one Interrupted per peer, 0 Disconnected);
//   20 s lets a ~15 s signal loss (tunnel) resume instead of dropping the player.
// - notify 1.5 s: no Interrupted on mobile/far/asym in any run (loss bursts last ~50 ms), so the
//   warning only shows for real outages.
const MAX_PREDICTION: usize = 10;
// With more than two players GGRS (vendored, GGPO's rule) confirms a frame only once every peer
// reports holding every player's input for it, one network hop later than local receipt, and
// the prediction window is counted from that frame; 10 would stall 3-4 player games that the
// same link runs smoothly with two. A 16-frame rollback costs more core time per frame, but
// rollbacks stay as deep as the input actually late, not as deep as the window.
const MAX_PREDICTION_MULTI: usize = 16;

/// The prediction window for a game of `players`: see `MAX_PREDICTION_MULTI`.
pub(crate) fn max_prediction(players: usize) -> usize {
    if players > 2 { MAX_PREDICTION_MULTI } else { MAX_PREDICTION }
}
const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const DISCONNECT_NOTIFY: Duration = Duration::from_millis(1500);

/// GGRS refreshes its ping every 200 ms (`QUALITY_REPORT_INTERVAL`); sampling faster would feed
/// the estimator the same value several times.
const RTT_SAMPLE_EVERY: Duration = Duration::from_millis(200);
/// Adaptive input delay bounds, as `auto_input_delay`.
const DELAY_MIN: usize = 1;
const DELAY_MAX: usize = 6;
/// Raise quickly: a target above the delay means the link got slower than the delay plans for.
const DELAY_RAISE_AFTER: Duration = Duration::from_secs(1);
/// Lower slowly: a short calm spell on a jittery link must not trade a frame of lag for rollbacks.
const DELAY_LOWER_AFTER: Duration = Duration::from_secs(10);
/// Each change shifts the input stream by a frame; space them so one settles before the next.
const DELAY_MIN_GAP: Duration = Duration::from_secs(1);
/// A checkpoint (the "last moment" state) is recorded on every frame that is a multiple of this:
/// 5 s at 60 fps.
const CHECKPOINT_EVERY: i32 = 300;
/// Frame of the save a game was started from: before every simulated frame.
const START_FRAME: i32 = -1;

/// A shared save: the state at the start of `frame`, saved by the player on controller `slot`.
struct SaveRecord {
    frame: i32,
    slot: usize,
    state: Snapshot,
}

/// Adaptive input delay: moves `delay` one frame at a time toward the target computed from the
/// worst link (`auto_input_delay`). Pure; time comes in as `now`.
#[derive(Clone, Debug)]
pub(crate) struct DelayControl {
    delay: usize,
    auto: bool,
    /// Since when the target has been above `delay` without a break.
    above_since: Option<Instant>,
    /// Since when the target has been below `delay` without a break.
    below_since: Option<Instant>,
    changed: Option<Instant>,
}

impl DelayControl {
    /// `auto == false`: the host pinned `delay`, it never moves.
    pub(crate) fn new(delay: usize, auto: bool) -> DelayControl {
        DelayControl { delay, auto, above_since: None, below_since: None, changed: None }
    }

    pub(crate) fn delay(&self) -> usize {
        self.delay
    }

    /// Feeds the current target (`None`: no warm peer). Returns the new delay when it changed.
    pub(crate) fn update(&mut self, target: Option<usize>, now: Instant) -> Option<usize> {
        let Some(target) = target.filter(|_| self.auto).map(|t| t.clamp(DELAY_MIN, DELAY_MAX)) else {
            self.above_since = None;
            self.below_since = None;
            return None;
        };
        let (since, after, step): (Instant, Duration, fn(usize) -> usize) = if target > self.delay {
            self.below_since = None;
            (*self.above_since.get_or_insert(now), DELAY_RAISE_AFTER, |d| d + 1)
        } else if target < self.delay {
            self.above_since = None;
            (*self.below_since.get_or_insert(now), DELAY_LOWER_AFTER, |d| d - 1)
        } else {
            self.above_since = None;
            self.below_since = None;
            return None;
        };
        let gap_ok = self.changed.is_none_or(|c| now.saturating_duration_since(c) >= DELAY_MIN_GAP);
        if now.saturating_duration_since(since) < after || !gap_ok {
            return None;
        }
        self.delay = step(self.delay);
        self.changed = Some(now);
        Some(self.delay)
    }
}

pub struct NetSession {
    ggrs: P2PSession<EboboConfig>,
    link: Rc<RefCell<Option<Link>>>,
    sent: Rc<RefCell<[u64; 4]>>,
    started: Instant,
    slots: Vec<PeerId>,
    local: usize,
    pending: Vec<NetEvent>,
    last_rollback: u32,
    /// GGRS frame the next `AdvanceFrame` simulates (turbo phase).
    sim_frame: i32,
    /// Smoothed RTT per handle; the local one stays empty.
    rtt: Vec<RttEstimator>,
    rtt_sampled: Option<Instant>,
    /// Handles whose player left; never sampled again.
    gone: Vec<bool>,
    frame_ms: f64,
    delay: DelayControl,
    /// Shared saves by frame, oldest first: the newest final one (the start save of a game
    /// continued from a save counts as final) and every newer one that is not final yet.
    /// `INPUT_LOAD` on frame `f` restores the newest with a frame below `f`. A rollback to
    /// frame `r` drops the ones from `r` on; re-simulation records them again.
    saves: Vec<SaveRecord>,
    /// Loads that restored a save and are not final yet, as (frame, slot); dropped and made
    /// again by rollbacks like `saves`.
    loads: Vec<(i32, usize)>,
    /// Every frame up to this one is final: simulated with the real inputs of all connected
    /// players, with the rollback those inputs caused already done, and never simulated again
    /// (GGRS rolls back only above its confirmed frame). Saves, loads and checkpoints of such
    /// frames are the same on every peer; they are reported and handed out only then. See
    /// `finalize`.
    final_through: i32,
    /// One-frame bits handed to `advance` that GGRS has not taken yet; they go out with the
    /// next local input it takes.
    carry: u32,
    /// GGRS frame of the last `advance` call. GGRS takes one local input per frame: a call
    /// repeated for the same frame (the session waits at the prediction window) is ignored.
    submitted: Option<i32>,
    /// Local inputs GGRS will drop: one for every frame the input delay was lowered by (it
    /// would land on a frame that already has an input).
    drops: u32,
    /// The newest final `INPUT_SAVE` state `take_saved` has not handed out yet.
    saved_ready: Option<Snapshot>,
    /// The newest checkpoint, not final yet: (frame, state at the start of it).
    checkpoint_pending: Option<(i32, Snapshot)>,
    /// The newest final checkpoint `take_checkpoint` has not handed out yet.
    checkpoint_ready: Option<Snapshot>,
    /// Tests: the input words of every simulated frame, as last simulated.
    #[cfg(test)]
    input_log: Vec<[u32; 4]>,
    /// Tests: frame whose saved state hash is recorded in `probe_hash` (last save wins).
    #[cfg(test)]
    probe: (i32, Option<u64>),
    /// Tests: input delay applied after the next advanced frame, bypassing `DelayControl`.
    #[cfg(test)]
    force_delay: Option<usize>,
}

fn io(e: GgrsError) -> NetError {
    NetError::Io(format!("ggrs: {e}"))
}

impl NetSession {
    /// Builds the GGRS P2P session: `slots.len()` players, local handle = index of
    /// `link.me()` in `slots`, others remote. `fps` = 60 NTSC / 50 PAL. `delay_auto`: the local
    /// delay adapts to the worst link during the game, starting at `input_delay`.
    /// `shared_save`: the state every peer loaded before the first frame (a game continued
    /// from a save), `None` for a game from power-on; it is what `INPUT_LOAD` loads until
    /// somebody saves.
    pub fn new(
        link: Link,
        slots: &[PeerId],
        input_delay: usize,
        delay_auto: bool,
        fps: usize,
        shared_save: Option<Snapshot>,
    ) -> Result<NetSession, NetError> {
        let mut session = NetSession::with_tuning(link, slots, input_delay, delay_auto, fps, max_prediction(slots.len()), DISCONNECT_TIMEOUT, DISCONNECT_NOTIFY)?;
        session.saves.extend(shared_save.map(|state| SaveRecord { frame: START_FRAME, slot: 0, state }));
        Ok(session)
    }

    #[allow(clippy::too_many_arguments)]
    fn with_tuning(
        link: Link,
        slots: &[PeerId],
        input_delay: usize,
        delay_auto: bool,
        fps: usize,
        max_prediction: usize,
        disconnect_timeout: Duration,
        notify_delay: Duration,
    ) -> Result<NetSession, NetError> {
        let me = link.me();
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
            .with_max_prediction_window(max_prediction)
            .with_fps(fps)
            .map_err(io)?
            .with_desync_detection_mode(DesyncDetection::On { interval: 60 })
            .with_disconnect_timeout(disconnect_timeout)
            .with_disconnect_notify_delay(notify_delay);
        for (handle, &peer) in slots.iter().enumerate() {
            let kind = if handle == local { PlayerType::Local } else { PlayerType::Remote(peer) };
            builder = builder.add_player(kind, handle).map_err(io)?;
        }
        let link = Rc::new(RefCell::new(Some(link)));
        let sent = Rc::new(RefCell::new([0u64; 4]));
        let socket = SharedSocket { link: link.clone(), sent: sent.clone() };
        let ggrs = builder.start_p2p_session(socket).map_err(io)?;
        Ok(NetSession {
            ggrs,
            link,
            sent,
            started: Instant::now(),
            slots: slots.to_vec(),
            local,
            pending: Vec::new(),
            last_rollback: 0,
            sim_frame: 0,
            rtt: vec![RttEstimator::default(); slots.len()],
            rtt_sampled: None,
            gone: vec![false; slots.len()],
            frame_ms: 1000.0 / fps.max(1) as f64,
            delay: DelayControl::new(input_delay, delay_auto),
            saves: Vec::new(),
            loads: Vec::new(),
            final_through: START_FRAME,
            carry: 0,
            submitted: None,
            drops: 0,
            saved_ready: None,
            checkpoint_pending: None,
            checkpoint_ready: None,
            #[cfg(test)]
            input_log: Vec::new(),
            #[cfg(test)]
            probe: (NO_PROBE, None),
            #[cfg(test)]
            force_delay: None,
        })
    }

    /// Controller index this machine plays.
    pub fn local_slot(&self) -> usize {
        self.local
    }

    /// An `INPUT_LOAD` sent now would find a save: the game started from one, or somebody saved.
    pub fn has_save(&self) -> bool {
        !self.saves.is_empty()
    }

    /// The newest confirmed `INPUT_SAVE` state not yet taken (identical on every peer), for
    /// the save file. Older untaken ones are gone.
    pub fn take_saved(&mut self) -> Option<Snapshot> {
        self.saved_ready.take()
    }

    /// The newest confirmed periodic checkpoint not yet taken (identical on every peer at the
    /// same frame), for the "last moment" file. Older untaken ones are gone.
    pub fn take_checkpoint(&mut self) -> Option<Snapshot> {
        self.checkpoint_ready.take()
    }

    /// Moves `final_through` forward and reports what became final: `Saved` and `Loaded` in
    /// frame order (a save before a load of the same frame), the newest save for `take_saved`,
    /// the checkpoint for `take_checkpoint`. Saves older than the newest final one are dropped.
    ///
    /// Must run right after the requests of a `ggrs.advance_frame()` call were carried out,
    /// with no poll in between. That call polled first, then rolled back to the first frame
    /// whose prediction a received input contradicted, and `confirmed_frame()` is the last
    /// frame for which every connected player's input has arrived: remote ones by that poll,
    /// the local one up to the current frame plus the input delay; with more than two players
    /// also every peer has reported holding it, so a leaver's disconnect is never moved down to
    /// it or below. So every frame up to `confirmed_frame()` that was simulated (`sim_frame - 1`
    /// is the last) ran with real inputs, and a later rollback starts above it, because a new
    /// input is always for a frame above the confirmed one.
    fn finalize(&mut self) {
        let through = self.ggrs.confirmed_frame().min(self.sim_frame - 1);
        if through <= self.final_through {
            return;
        }
        let from = self.final_through;
        self.final_through = through;
        let saved = self.saves.iter().filter(|s| s.frame > from && s.frame <= through);
        let loaded = self.loads.iter().filter(|l| l.0 <= through);
        let mut events: Vec<(i32, NetEvent)> = saved
            .map(|s| (s.frame, NetEvent::Saved { slot: s.slot }))
            .chain(loaded.map(|&(frame, slot)| (frame, NetEvent::Loaded { slot })))
            .collect();
        // Stable: saves were chained first, so a save stays before a load of the same frame.
        events.sort_by_key(|e| e.0);
        self.pending.extend(events.into_iter().map(|e| e.1));
        self.loads.retain(|l| l.0 > through);
        if let Some(newest) = self.saves.iter().rposition(|s| s.frame <= through) {
            if self.saves[newest].frame > from {
                self.saved_ready = Some(self.saves[newest].state.clone());
            }
            self.saves.drain(..newest);
        }
        if self.checkpoint_pending.as_ref().is_some_and(|c| c.0 <= through) {
            self.checkpoint_ready = self.checkpoint_pending.take().map(|c| c.1);
        }
    }

    /// True once every peer is synchronized.
    pub fn is_running(&self) -> bool {
        self.ggrs.current_state() == SessionState::Running
    }

    /// Network I/O; cheap, call it every <= 2 ms, also between frames and while not advancing.
    pub fn poll(&mut self) {
        self.ggrs.poll_remote_clients();
        let departed = self.link.borrow_mut().as_mut().map(Link::take_departed).unwrap_or_default();
        for peer in departed {
            if let Some(handle) = self.slots.iter().position(|&p| p == peer) {
                if handle != self.local {
                    let _ = self.ggrs.disconnect_player(handle);
                    self.gone[handle] = true;
                    self.pending.push(NetEvent::Disconnected { peer });
                }
            }
        }
        self.sample_rtt(Instant::now());
    }

    /// One RTT sample per remote player still in the game, every `RTT_SAMPLE_EVERY`.
    fn sample_rtt(&mut self, now: Instant) {
        if self.rtt_sampled.is_some_and(|t| now.saturating_duration_since(t) < RTT_SAMPLE_EVERY) {
            return;
        }
        self.rtt_sampled = Some(now);
        for handle in 0..self.slots.len() {
            if handle == self.local || self.gone[handle] {
                continue;
            }
            // `NotEnoughData` during the first second.
            if let Ok(n) = self.ggrs.network_stats(handle) {
                self.rtt[handle].add_sample(n.ping as f64);
            }
        }
    }

    /// In-game delay target: `auto_input_delay` of the worst warm remote link; `None` while no
    /// link is warm.
    fn delay_target(&self) -> Option<usize> {
        let worst = (0..self.slots.len())
            .filter(|&h| h != self.local && !self.gone[h] && self.rtt[h].is_warm())
            .filter_map(|h| self.rtt[h].delay_rtt())
            .reduce(f64::max)?;
        Some(auto_input_delay(worst, self.frame_ms))
    }

    /// Adds the local input and advances one frame, handling GGRS save/load/advance requests on
    /// `console`. Audio of re-simulated frames is dropped, only the newest frame's samples remain
    /// buffered in `console`.
    ///
    /// One-frame bits (`INPUT_RESET`, `INPUT_SAVE`, `INPUT_LOAD`) are never lost: when GGRS
    /// does not take this call's input, they are kept for the next one it takes. So pass each
    /// request once, whatever the outcome.
    pub fn advance(&mut self, console: &mut Console, local_input: u32) -> Result<FrameOutcome, NetError> {
        self.carry |= local_input & ONE_FRAME_BITS;
        if !self.is_running() {
            self.poll();
            return Ok(FrameOutcome::default());
        }
        let before = self.ggrs.current_frame();
        // GGRS takes the first input submitted for a frame, except the one after each frame
        // the input delay was lowered by.
        let fresh = self.submitted.replace(before) != Some(before);
        let taken = fresh && self.drops == 0;
        if fresh {
            self.drops = self.drops.saturating_sub(1);
        }
        let mut word = local_input & !ONE_FRAME_BITS;
        if taken {
            word |= std::mem::take(&mut self.carry);
        }
        self.ggrs.add_local_input(self.local, word).map_err(io)?;
        // GGRS 0.13 P2P never returns `PredictionThreshold`: at the window it returns no new
        // `AdvanceFrame`, but it still carries out a rollback the received inputs caused, so
        // the requests can re-simulate frames without adding one. After a delay decrease GGRS
        // drops the next submitted inputs internally; `advance_frame` still succeeds.
        let requests = self.ggrs.advance_frame().map_err(io)?;
        let new_frame = self.ggrs.current_frame() > before;
        let advances = requests.iter().filter(|r| matches!(r, GgrsRequest::AdvanceFrame { .. })).count();
        // Frames before this many were shown and heard already.
        let resimulated = advances - new_frame as usize;
        let mut seen = 0;
        for request in requests {
            match request {
                GgrsRequest::SaveGameState { cell, frame } => {
                    let hash = console.state_hash();
                    #[cfg(test)]
                    if frame == self.probe.0 {
                        self.probe.1 = Some(hash);
                    }
                    cell.save(frame, Some(console.snapshot()), Some(hash as u128));
                    self.sim_frame = frame;
                }
                GgrsRequest::LoadGameState { cell, frame } => {
                    let snapshot = cell.load().expect("ggrs asked to load a state it never saved");
                    console.restore(&snapshot);
                    self.sim_frame = frame;
                    // What was recorded for the frames about to be re-simulated is void.
                    self.saves.retain(|s| s.frame < frame);
                    self.loads.retain(|l| l.0 < frame);
                    self.checkpoint_pending.take_if(|c| c.0 >= frame);
                }
                GgrsRequest::AdvanceFrame { inputs } => {
                    let frame = self.sim_frame;
                    // Turbo is an NES feature; other systems ignore the turbo bits.
                    let turbo = console.system() == System::Nes;
                    let mut pads = [0u16; 4];
                    let mut reset = false;
                    // Lowest controller that sent the bit.
                    let (mut saver, mut loader) = (None, None);
                    #[cfg(test)]
                    let mut words = [0u32; 4];
                    for (slot, &(input, status)) in inputs.iter().enumerate().take(4) {
                        if status == InputStatus::Disconnected {
                            continue;
                        }
                        pads[slot] = if turbo { expand_turbo(input, frame) } else { (input & INPUT_PAD) as u16 };
                        reset |= input & INPUT_RESET != 0;
                        if input & INPUT_SAVE != 0 {
                            saver.get_or_insert(slot);
                        }
                        if input & INPUT_LOAD != 0 {
                            loader.get_or_insert(slot);
                        }
                        #[cfg(test)]
                        {
                            words[slot] = input;
                        }
                    }
                    #[cfg(test)]
                    {
                        self.input_log.resize(self.input_log.len().max(frame as usize + 1), [0; 4]);
                        self.input_log[frame as usize] = words;
                    }
                    // In this order: a save and a load on one frame save the state the frame
                    // starts with and load the save before it.
                    if let Some(slot) = saver {
                        self.saves.push(SaveRecord { frame, slot, state: console.snapshot() });
                    }
                    if frame > 0 && frame % CHECKPOINT_EVERY == 0 {
                        self.checkpoint_pending = Some((frame, console.snapshot()));
                    }
                    if let Some(slot) = loader {
                        if let Some(save) = self.saves.iter().rev().find(|s| s.frame < frame) {
                            console.restore(&save.state);
                            self.loads.push((frame, slot));
                        }
                    }
                    if reset {
                        console.reset();
                    }
                    seen += 1;
                    // Re-simulated frames are never shown or heard: skip pixels and audio.
                    console.set_fast_forward(seen <= resimulated);
                    console.run_frame(pads);
                    if seen <= resimulated {
                        console.clear_audio();
                    }
                    self.sim_frame += 1;
                }
            }
        }
        self.last_rollback = resimulated as u32;
        self.finalize();
        // Delay changes go after `advance_frame`, never between it and `add_local_input`, and
        // only on a frame whose input GGRS took, so the input queue has a new input since the
        // last change (two increases without one would send overlapping fill frames). An input
        // with a one-frame bit is skipped: an increase copies the last input, which would
        // repeat the reset, save or load.
        if new_frame && taken && word & ONE_FRAME_BITS == 0 {
            self.adapt_delay(Instant::now())?;
        }
        Ok(FrameOutcome { advanced: new_frame, rollback_frames: self.last_rollback })
    }

    fn adapt_delay(&mut self, now: Instant) -> Result<(), NetError> {
        let old = self.delay.delay();
        #[cfg(test)]
        let forced = self.force_delay.take().inspect(|&d| self.delay.delay = d);
        #[cfg(not(test))]
        let forced = None;
        let target = self.delay_target();
        let Some(delay) = forced.or_else(|| self.delay.update(target, now)) else { return Ok(()) };
        self.drops += old.saturating_sub(delay) as u32;
        self.ggrs.set_input_delay(self.local, delay).map_err(io)
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
                GgrsEvent::Disconnected { addr } => {
                    if let Some(handle) = self.slots.iter().position(|&p| p == addr) {
                        self.gone[handle] = true;
                    }
                    NetEvent::Disconnected { peer: addr }
                }
                GgrsEvent::DesyncDetected { frame, .. } => NetEvent::Desync { frame },
                // Pacing goes through `frames_ahead`.
                GgrsEvent::WaitRecommendation { .. } => continue,
            });
        }
        out
    }

    pub fn stats(&self) -> NetStats {
        let link = self.link.borrow();
        let sent = self.sent.borrow();
        let secs = self.started.elapsed().as_secs_f64().max(1.0);
        let peers = self
            .slots
            .iter()
            .enumerate()
            .filter(|&(slot, _)| slot != self.local)
            .map(|(slot, &peer)| {
                let route = link.as_ref().and_then(|l| l.route_kind(peer));
                let mut stats = PeerStats { peer: Some(peer), slot, route, ..PeerStats::default() };
                let bytes = sent.get(peer.0 as usize).copied().unwrap_or(0);
                stats.kbps_sent = (bytes as f64 * 8.0 / 1000.0 / secs) as u32;
                if let Ok(n) = self.ggrs.network_stats(slot) {
                    stats.ping_ms = self.rtt[slot].srtt.map_or(n.ping.min(u32::MAX as u128) as u32, |r| r.round() as u32);
                    stats.local_frames_behind = n.local_frames_behind;
                    stats.remote_frames_behind = n.remote_frames_behind;
                }
                stats
            })
            .collect();
        NetStats { peers, last_rollback: self.last_rollback, input_delay: self.delay.delay() }
    }

    /// Frames we are ahead of the slowest peer (> 0: slow down a little).
    pub fn frames_ahead(&self) -> i32 {
        self.ggrs.frames_ahead()
    }

    /// Says BYE to every peer.
    pub fn leave(self) {
        if let Some(link) = self.link.borrow_mut().take() {
            link.close();
        }
    }
}

#[cfg(test)]
const NO_PROBE: i32 = ggrs::NULL_FRAME;

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
