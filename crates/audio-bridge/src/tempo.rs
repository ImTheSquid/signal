//! Tempo and phase, as a prediction rather than an event.
//!
//! The light is told "the next beat is in N ms, the period is P", never "a beat
//! just happened". Reacting to detection lands a lamp 0.10-0.33 of a beat late
//! once the 100ms relay dwell is included; predicting collapses that whole
//! chain into one calibration constant. It also makes packet loss free, since
//! the script extrapolates the grid itself.
//!
//! Everything is timed off samples consumed, never wall clock, so the offline
//! path and the live path produce identical results.

use anyhow::{Context, Result};
use bliss_audio_aubio_rs::{OnsetMode, Tempo};

use crate::bands::{Ema, Levels};

/// aubio's analysis window and step. The hop sets how precisely a beat instant
/// can be placed — 5.33ms at 48kHz, which is 1% of a beat at 128 BPM and well
/// inside the error the relays impose anyway.
pub const HOP: usize = 256;
const WINDOW: usize = 1024;

/// Confidence decays with this time constant when evidence stops arriving,
/// reaching ~0.1 of its value after 8 seconds.
const CONFIDENCE_TAU_S: f32 = 3.3;

/// Above this, aubio's tempo estimate is adopted as-is.
const CONF_TRACKING: f32 = 0.55;
/// Below this, the grid freezes and free-runs.
const CONF_COASTING: f32 = 0.25;
/// Between the two, a new estimate is only accepted if it barely differs from
/// the one already held — enough to follow a pitch-fader ride, not enough to
/// jump to the other deck mid-transition.
const DEGRADED_TOLERANCE: f32 = 0.02;

/// Broadband energy below this fraction of its own long-run reference is
/// treated as no evidence at all.
///
/// This gate is the one thing aubio will not do, and the most important piece
/// here: a pad with a 1Hz LFO is perfectly periodic at 60 BPM, and without it
/// the tracker locks to that and destroys a grid it already knew.
const SILENCE_RATIO: f32 = 0.05;
const ENERGY_FAST_S: f32 = 2.0;
const ENERGY_SLOW_S: f32 = 60.0;

/// Tempi outside this are not dance music, and accepting them costs a grid.
const MIN_BPM: f32 = 60.0;
const MAX_BPM: f32 = 200.0;

/// What the tracker currently believes, sampled at an instant.
#[derive(Clone, Copy, Debug, Default)]
pub struct Grid {
    /// None until a tempo has ever been established.
    pub period_ms: Option<f32>,
    /// Time from now until the next beat. None when there is no grid.
    pub ms_to_next_beat: Option<f32>,
    /// Counts beats from an arbitrary origin. Phase-locked to the beat but
    /// **not** to the bar — there is no downbeat estimator, so nothing may read
    /// `beat_index % 4 == 0` as a downbeat.
    pub beat_index: u8,
    pub confidence: f32,
    /// Live evidence is driving the grid.
    pub tracking: bool,
    /// The grid is free-running on a frozen period because evidence collapsed.
    pub coasting: bool,
    /// Milliseconds since the last detected beat, for accent rendering.
    pub ms_since_beat: Option<f32>,
    /// How strong the last detected beat was, 0..=1.
    pub onset_strength: f32,
}

/// The seam that keeps aubio a bet rather than a commitment. Everything
/// downstream — the wire, the render script — consumes `Grid` and does not care
/// what produced it.
pub trait Tracker {
    /// Feed exactly [`HOP`] samples.
    fn push(&mut self, hop: &[f32], levels: &Levels) -> Result<()>;
    fn grid(&self) -> Grid;
}

pub struct AubioTracker {
    tempo: Tempo,
    sample_rate: f32,
    samples_seen: u64,

    /// The held grid. Survives aubio losing the plot, which is the point.
    period_ms: Option<f32>,
    anchor_ms: f64,
    beat_index: u8,
    last_beat_ms: Option<f64>,
    last_beat_strength: f32,

    confidence: f32,
    confidence_decay: f32,
    energy_fast: Ema,
    energy_slow: Ema,
}

impl AubioTracker {
    pub fn new(sample_rate: u32) -> Result<Self> {
        let tempo = Tempo::new(OnsetMode::SpecFlux, WINDOW, HOP, sample_rate)
            .context("constructing aubio tempo")?;
        let hop_s = HOP as f32 / sample_rate as f32;
        Ok(AubioTracker {
            tempo,
            sample_rate: sample_rate as f32,
            samples_seen: 0,
            period_ms: None,
            anchor_ms: 0.0,
            beat_index: 0,
            last_beat_ms: None,
            last_beat_strength: 0.0,
            confidence: 0.0,
            confidence_decay: (-hop_s / CONFIDENCE_TAU_S).exp(),
            energy_fast: Ema::new(hop_s, ENERGY_FAST_S),
            energy_slow: Ema::new(hop_s, ENERGY_SLOW_S),
        })
    }

    fn now_ms(&self) -> f64 {
        self.samples_seen as f64 * 1000.0 / self.sample_rate as f64
    }

    /// aubio's own confidence, gated on there being any energy to be confident
    /// about, then floored by a decaying memory of what it used to be.
    fn update_confidence(&mut self, aubio_confidence: f32, levels: &Levels) {
        let fast = self.energy_fast.update(levels.raw_energy);
        let slow = self.energy_slow.update(levels.raw_energy);
        let audible = slow > 0.0 && fast / slow >= SILENCE_RATIO;

        let instant = if audible {
            aubio_confidence.clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.confidence = instant.max(self.confidence * self.confidence_decay);
    }

    /// Decide whether to believe a fresh tempo estimate.
    fn consider_period(&mut self, bpm: f32) {
        if !(MIN_BPM..=MAX_BPM).contains(&bpm) {
            return;
        }
        let candidate = 60_000.0 / bpm;
        match self.period_ms {
            // Nothing held yet, or firm evidence: take it.
            None => self.period_ms = Some(candidate),
            Some(_) if self.confidence >= CONF_TRACKING => self.period_ms = Some(candidate),
            // Degraded: follow a drifting tempo, refuse a jump. During a
            // two-deck transition the alternative is oscillating between them.
            Some(held) if self.confidence >= CONF_COASTING => {
                if ((candidate - held) / held).abs() <= DEGRADED_TOLERANCE {
                    self.period_ms = Some(candidate);
                }
            }
            // Coasting: frozen.
            Some(_) => {}
        }
    }
}

impl Tracker for AubioTracker {
    fn push(&mut self, hop: &[f32], levels: &Levels) -> Result<()> {
        debug_assert_eq!(hop.len(), HOP);
        let beat = self
            .tempo
            .do_result(hop)
            .map_err(|e| anyhow::anyhow!("aubio tempo: {e}"))?;
        self.samples_seen += hop.len() as u64;

        self.update_confidence(self.tempo.get_confidence(), levels);
        self.consider_period(self.tempo.get_bpm());

        if beat != 0.0 && self.confidence >= CONF_COASTING {
            // aubio reports the beat position in its own sample clock, which is
            // the same one `samples_seen` counts, so this needs no correction.
            let at = self.tempo.get_last_ms() as f64;
            self.anchor_ms = at;
            self.last_beat_ms = Some(at);
            self.last_beat_strength = beat.clamp(0.0, 1.0);
            self.beat_index = self.beat_index.wrapping_add(1);
        }
        Ok(())
    }

    fn grid(&self) -> Grid {
        let now = self.now_ms();
        let coasting = self.confidence < CONF_COASTING;
        let ms_to_next_beat = self.period_ms.map(|period| {
            let period = period as f64;
            // Extrapolate from the anchor rather than from the last beat, so a
            // run of undetected beats costs precision, not the grid.
            let elapsed = (now - self.anchor_ms).rem_euclid(period);
            (period - elapsed) as f32
        });

        Grid {
            period_ms: self.period_ms,
            ms_to_next_beat,
            beat_index: self.beat_index,
            confidence: self.confidence,
            tracking: self.confidence >= CONF_TRACKING && self.period_ms.is_some(),
            coasting: coasting && self.period_ms.is_some(),
            ms_since_beat: self.last_beat_ms.map(|at| (now - at) as f32),
            onset_strength: self.last_beat_strength,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bands::Bands;

    const FS: u32 = 48_000;

    /// A click train: a short decaying burst of noise at each beat, which is a
    /// far better stand-in for a kick than an impulse.
    fn click_train(bpm: f32, seconds: f32) -> Vec<f32> {
        let period = 60.0 / bpm;
        let n = (FS as f32 * seconds) as usize;
        let mut out = vec![0.0f32; n];
        // Deterministic pseudo-noise; no rand dependency for a fixture.
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = |()| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 40) as f32 / 8_388_608.0 - 1.0
        };
        let mut beat = 0.0f32;
        while beat < seconds {
            let start = (beat * FS as f32) as usize;
            let len = (FS as f32 * 0.05) as usize;
            for i in 0..len {
                if start + i >= n {
                    break;
                }
                let decay = (-(i as f32) / (FS as f32 * 0.010)).exp();
                out[start + i] += next(()) * decay;
            }
            beat += period;
        }
        out
    }

    fn run(signal: &[f32]) -> (AubioTracker, Grid) {
        let mut tracker = AubioTracker::new(FS).unwrap();
        let mut bands = Bands::new(FS, HOP);
        for hop in signal.chunks_exact(HOP) {
            for &s in hop {
                bands.push(s);
            }
            let levels = bands.sample();
            tracker.push(hop, &levels).unwrap();
        }
        let grid = tracker.grid();
        (tracker, grid)
    }

    #[test]
    fn locks_to_a_click_train() {
        for bpm in [100.0, 128.0, 140.0] {
            let (_, grid) = run(&click_train(bpm, 20.0));
            let period = grid.period_ms.expect("should establish a period");
            let expected = 60_000.0 / bpm;
            let err_bpm = (60_000.0 / period - bpm).abs();
            assert!(
                err_bpm < 2.0,
                "{bpm} BPM: got {:.1} BPM (period {period:.1}ms, expected {expected:.1}ms)",
                60_000.0 / period
            );
            assert!(grid.confidence > 0.0, "{bpm} BPM: no confidence");
        }
    }

    /// The predicted beat must actually fall on a beat, which is the thing the
    /// whole design is for. Measured as distance from the true grid.
    #[test]
    fn predicted_beats_land_on_the_grid() {
        let bpm = 128.0;
        let period = 60_000.0 / bpm;
        let (tracker, grid) = run(&click_train(bpm, 20.0));
        let next = grid
            .ms_to_next_beat
            .expect("should predict a beat") as f64;
        let absolute = tracker.now_ms() + next;
        // Distance to the nearest true beat instant.
        let off = (absolute % period as f64).min(period as f64 - absolute % period as f64);
        assert!(
            off < 30.0,
            "predicted beat is {off:.1}ms off the true grid (period {period:.1}ms)"
        );
    }

    #[test]
    fn silence_does_not_hold_confidence_up() {
        let mut signal = click_train(128.0, 20.0);
        signal.extend(std::iter::repeat(0.0).take(FS as usize * 12));
        let (_, grid) = run(&signal);
        assert!(
            grid.confidence < CONF_COASTING,
            "12s of silence should decay confidence, got {}",
            grid.confidence
        );
        assert!(
            grid.period_ms.is_some(),
            "the period should be held through silence, not forgotten"
        );
        assert!(grid.coasting, "should report coasting");
    }

    /// A perfectly periodic sub-audio wobble is not a tempo. Without the energy
    /// gate this is what steals the grid during a breakdown.
    #[test]
    fn a_quiet_periodic_pad_does_not_steal_the_grid() {
        let mut signal = click_train(128.0, 20.0);
        let established = {
            let (_, grid) = run(&signal);
            grid.period_ms.unwrap()
        };
        // A 1Hz-modulated quiet pad: periodic at 60 BPM, 40dB down.
        let n = FS as usize * 15;
        signal.extend((0..n).map(|i| {
            let t = i as f32 / FS as f32;
            let lfo = 0.5 * (1.0 - (2.0 * std::f32::consts::PI * 1.0 * t).cos());
            0.01 * lfo * (2.0 * std::f32::consts::PI * 300.0 * t).sin()
        }));
        let (_, grid) = run(&signal);
        let held = grid.period_ms.unwrap();
        assert!(
            (held - established).abs() < 5.0,
            "pad pulled the period from {established:.1}ms to {held:.1}ms"
        );
    }
}
