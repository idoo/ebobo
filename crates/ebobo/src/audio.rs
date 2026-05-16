//! Audio output queue with drift control. Owner: platform slice.

use sdl2::audio::{AudioQueue, AudioSpecDesired};

const START_MS: f64 = 40.0;
/// Below this the queue is about to run dry: pause and prebuffer again.
const REARM_MS: f64 = 5.0;
const TARGET_MS: f64 = 60.0;
const OVERFLOW_MS: f64 = 200.0;
const MAX_ADJUST: f64 = 0.005;

/// SDL audio queue: 48 kHz mono f32.
pub struct Audio {
    queue: AudioQueue<f32>,
    rate: f64,
    volume: f32,
    paused: bool,
    playing: bool,
    buf: Vec<f32>,
}

/// Proportional drift control: more samples when the queue runs short, fewer when it is long.
fn drift_factor(queued_ms: f64) -> f64 {
    let err = (TARGET_MS - queued_ms) / TARGET_MS;
    1.0 + (err * MAX_ADJUST).clamp(-MAX_ADJUST, MAX_ADJUST)
}

impl Audio {
    pub fn new(sdl: &sdl2::Sdl) -> Result<Audio, String> {
        let audio = sdl.audio()?;
        let desired = AudioSpecDesired {
            freq: Some(48_000),
            channels: Some(1),
            samples: Some(512),
        };
        let queue: AudioQueue<f32> = audio.open_queue(None, &desired)?;
        let rate = queue.spec().freq as f64;
        Ok(Audio {
            queue,
            rate,
            volume: 1.0,
            paused: false,
            playing: false,
            buf: Vec::new(),
        })
    }

    fn queued_ms(&self) -> f64 {
        let samples = self.queue.size() as f64 / std::mem::size_of::<f32>() as f64;
        samples * 1000.0 / self.rate
    }

    /// Device sample rate (what the core should produce, before `rate_factor`).
    pub fn sample_rate(&self) -> f64 {
        self.rate
    }

    /// Queues samples (volume applied here). Starts playback once ~40 ms are buffered; when the
    /// queue nearly runs dry (< 5 ms) playback pauses and prebuffers again.
    pub fn push(&mut self, samples: &[f32]) {
        if self.paused || samples.is_empty() {
            return;
        }
        let queued = self.queued_ms();
        if queued > OVERFLOW_MS {
            // Recover from a stall: drop the backlog and prebuffer again.
            self.clear();
        } else if self.playing && queued < REARM_MS {
            self.queue.pause();
            self.playing = false;
        }
        let v = self.volume;
        self.buf.clear();
        self.buf.extend(samples.iter().map(|s| s * v));
        let _ = self.queue.queue_audio(&self.buf);
        if !self.playing && self.queued_ms() >= START_MS {
            self.queue.resume();
            self.playing = true;
        }
    }

    /// Multiplier (0.995-1.005) for the core's sample rate that steers the queue toward the
    /// target latency (~60 ms) without audible pitch change.
    pub fn rate_factor(&self) -> f64 {
        drift_factor(self.queued_ms())
    }

    /// Drops queued audio (pause, load state, leaving a game).
    pub fn clear(&mut self) {
        self.queue.pause();
        self.queue.clear();
        self.playing = false;
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        if paused {
            self.queue.pause();
            self.playing = false;
        } else if self.queued_ms() >= START_MS {
            self.queue.resume();
            self.playing = true;
        }
    }

    /// 0.0-1.0.
    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::drift_factor;

    #[test]
    fn audio_drift_factor_clamped_and_proportional() {
        assert_eq!(drift_factor(60.0), 1.0);
        assert!(drift_factor(30.0) > 1.0 && drift_factor(30.0) <= 1.005);
        assert!(drift_factor(90.0) < 1.0);
        assert_eq!(drift_factor(0.0), 1.005);
        assert_eq!(drift_factor(500.0), 0.995);
    }
}
