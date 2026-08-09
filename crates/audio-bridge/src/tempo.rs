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

/// Beat instants kept for the period fit.
///
/// aubio's own BPM is quantised by its hop-counted period: on a 128.00 BPM
/// metronome it reports 128.95 with a spread of only 0.15, so the error is
/// systematic rather than noisy. 0.95 BPM is a 3.5ms period error, which drifts
/// 30ms off the grid in about four seconds — irrelevant while tracking, since
/// the anchor re-snaps every beat, but it makes coasting through a breakdown
/// worthless.
///
/// The beat *instants* are sample-accurate, so fitting a line through them
/// recovers the period without the bias. At 32 points and ~8ms jitter the
/// standard error on the slope is ~0.15ms, or 0.04 BPM.
const BEAT_HISTORY: usize = 32;

/// Enough points that the fit beats the estimate it replaces.
const MIN_BEATS_TO_FIT: usize = 8;

/// A fit this far from the seed means beats were assigned to the wrong indices,
/// so the fit is meaningless rather than merely imprecise.
const FIT_SANITY: f32 = 0.05;

/// How far a gap may sit from a whole number of beats before the count is
/// treated as unusable. A quarter of a beat is far beyond detector jitter and
/// far short of the half-beat where the rounding itself would flip.
const GAP_TOLERANCE: f64 = 0.25;

/// What the tracker currently believes, sampled at an instant.
#[derive(Clone, Copy, Debug, Default)]
pub struct Grid {
    /// None until a tempo has ever been established.
    pub period_ms: Option<f32>,
    /// Time from now until the next beat. None when there is no grid.
    pub ms_to_next_beat: Option<f32>,
    /// Counts beats from an arbitrary origin, advancing with the grid rather
    /// than with detections — aubio reports roughly one beat in five, so a
    /// detection counter would advance at a fifth of the tempo.
    ///
    /// Phase-locked to the beat but **not** to the bar: there is no downbeat
    /// estimator, so nothing may read `beat_index % 4 == 0` as a downbeat.
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
    /// Feed exactly [`HOP`] samples. Returns whether a beat landed in this hop,
    /// which is what makes the sender skip its tick and go out immediately.
    fn push(&mut self, hop: &[f32], levels: &Levels) -> Result<bool>;
    fn grid(&self) -> Grid;
}

pub struct AubioTracker {
    tempo: Tempo,
    sample_rate: f32,
    samples_seen: u64,

    /// The held grid. Survives aubio losing the plot, which is the point.
    period_ms: Option<f32>,
    /// Period fitted through recent beat instants, preferred when available.
    fitted_period_ms: Option<f32>,
    beat_times: std::collections::VecDeque<f64>,
    anchor_ms: f64,
    /// Fractional beats elapsed, integrated per hop against the current period,
    /// so the count follows the music rather than the detector.
    beats_elapsed: f64,
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
            fitted_period_ms: None,
            beat_times: std::collections::VecDeque::with_capacity(BEAT_HISTORY),
            anchor_ms: 0.0,
            beats_elapsed: 0.0,
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
        let adopted = match self.period_ms {
            // Nothing held yet, or firm evidence: take it.
            None => Some(candidate),
            Some(_) if self.confidence >= CONF_TRACKING => Some(candidate),
            // Degraded: follow a drifting tempo, refuse a jump. During a
            // two-deck transition the alternative is oscillating between them.
            Some(held) if self.confidence >= CONF_COASTING => {
                (((candidate - held) / held).abs() <= DEGRADED_TOLERANCE).then_some(candidate)
            }
            // Coasting: frozen.
            Some(_) => None,
        };

        if let Some(candidate) = adopted {
            // A real tempo change invalidates the history: those instants
            // belong to a grid that no longer exists, and fitting across the
            // seam produces a period matching neither.
            if self
                .period_ms
                .is_some_and(|held| ((candidate - held) / held).abs() > DEGRADED_TOLERANCE)
            {
                self.beat_times.clear();
                self.fitted_period_ms = None;
            }
            self.period_ms = Some(candidate);
        }
    }

    /// Period from the recorded beat instants: total span over total beats.
    ///
    /// aubio does not report every beat — on a click train it emits one every
    /// five or six — so the gaps have to be converted to beat counts before
    /// anything can be averaged.
    ///
    /// Counting happens **per gap**, never against a global index. A gap spans
    /// five or six beats, so rounding it survives a seed several percent wrong.
    /// Indexing every instant off the first one does not: the history spans
    /// ~176 beats, where a 0.9% seed error shifts assignments by more than a
    /// whole beat, and a least-squares fit over those indices converges to a
    /// self-consistent wrong answer rather than correcting the seed.
    ///
    /// Summing safe local counts and dividing the full span by the total keeps
    /// the long baseline without ever making a long-range assignment. With ~8ms
    /// jitter over ~176 beats that is a period error near 0.1ms.
    fn fit_period(&self, seed: f32) -> Option<f32> {
        if self.beat_times.len() < MIN_BEATS_TO_FIT {
            return None;
        }
        let seed = seed as f64;
        let mut total_beats = 0.0f64;
        let mut previous = *self.beat_times.front()?;

        for &t in self.beat_times.iter().skip(1) {
            let gap_beats = (t - previous) / seed;
            let beats = gap_beats.round();
            // A gap that is not close to a whole number of beats means the seed
            // is wrong enough that the count cannot be trusted.
            if beats < 1.0 || (gap_beats - beats).abs() > GAP_TOLERANCE {
                return None;
            }
            total_beats += beats;
            previous = t;
        }

        if total_beats < 1.0 {
            return None;
        }
        let span = *self.beat_times.back()? - *self.beat_times.front()?;
        let fitted = (span / total_beats) as f32;

        let seed = seed as f32;
        ((fitted - seed).abs() / seed <= FIT_SANITY).then_some(fitted)
    }

    fn record_beat(&mut self, at: f64) {
        if self.beat_times.len() == BEAT_HISTORY {
            self.beat_times.pop_front();
        }
        self.beat_times.push_back(at);
        if let Some(seed) = self.period_ms {
            self.fitted_period_ms = self.fit_period(seed).or(self.fitted_period_ms);
        }
    }

    /// The period the grid is drawn from: the fit when it exists, aubio's
    /// estimate until then.
    fn effective_period(&self) -> Option<f32> {
        self.fitted_period_ms.or(self.period_ms)
    }
}

impl Tracker for AubioTracker {
    fn push(&mut self, hop: &[f32], levels: &Levels) -> Result<bool> {
        debug_assert_eq!(hop.len(), HOP);
        let beat = self
            .tempo
            .do_result(hop)
            .map_err(|e| anyhow::anyhow!("aubio tempo: {e}"))?;
        self.samples_seen += hop.len() as u64;

        self.update_confidence(self.tempo.get_confidence(), levels);
        self.consider_period(self.tempo.get_bpm());

        if let Some(period) = self.effective_period() {
            let hop_ms = HOP as f64 * 1000.0 / self.sample_rate as f64;
            self.beats_elapsed += hop_ms / period as f64;
        }

        let landed = beat != 0.0 && self.confidence >= CONF_COASTING;
        if landed {
            // aubio reports the beat position in its own sample clock, which is
            // the same one `samples_seen` counts, so this needs no correction.
            let at = self.tempo.get_last_ms() as f64;
            self.anchor_ms = at;
            self.last_beat_ms = Some(at);
            self.last_beat_strength = beat.clamp(0.0, 1.0);
            self.record_beat(at);
        }
        Ok(landed)
    }

    fn grid(&self) -> Grid {
        let now = self.now_ms();
        let coasting = self.confidence < CONF_COASTING;
        let period_ms = self.effective_period();
        let ms_to_next_beat = period_ms.map(|period| {
            let period = period as f64;
            // Extrapolate from the anchor rather than from the last beat, so a
            // run of undetected beats costs precision, not the grid.
            let elapsed = (now - self.anchor_ms).rem_euclid(period);
            (period - elapsed) as f32
        });

        Grid {
            period_ms,
            ms_to_next_beat,
            beat_index: (self.beats_elapsed.rem_euclid(16.0)) as u8,
            confidence: self.confidence,
            tracking: self.confidence >= CONF_TRACKING && period_ms.is_some(),
            coasting: coasting && period_ms.is_some(),
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

    fn click_train(bpm: f32, seconds: f32) -> Vec<f32> {
        crate::analysis::synth_click_train(bpm, seconds, FS)
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

    /// Accuracy good enough to coast on. aubio's own BPM carries a systematic
    /// ~0.95 error at 128; the fit through beat instants is what closes it.
    ///
    /// Judged to within an octave deliberately. A uniform click train carries
    /// nothing to separate 175 from 87.5 — every click is identical, so there
    /// is no strong/weak pattern — and at 175 the tracker does settle on half
    /// time. That is not a phase error: every beat of the 87.5 grid is a real
    /// beat. Which subdivision to render is the script's call, and it has the
    /// period to halve or double, so the tracker reports what it measured
    /// rather than guessing at the musical intent.
    #[test]
    fn locks_to_a_click_train_within_an_octave() {
        for bpm in [100.0, 128.0, 140.0, 175.0] {
            let (_, grid) = run(&click_train(bpm, 30.0));
            let period = grid.period_ms.expect("should establish a period");
            let got = 60_000.0 / period;

            let ratio = (got / bpm).log2();
            let octave = ratio.round();
            assert!(
                octave.abs() <= 1.0 && (ratio - octave).abs() < 0.01,
                "{bpm} BPM: got {got:.3} BPM, not within an octave"
            );

            let nearest = bpm * 2.0f32.powf(octave);
            assert!(
                (got - nearest).abs() < 0.2,
                "{bpm} BPM: got {got:.3}, off its own octave {nearest:.3} by {:.3}",
                (got - nearest).abs()
            );
            assert!(grid.confidence > 0.0, "{bpm} BPM: no confidence");
        }
    }

    /// The period must be precise enough that a frozen grid survives a
    /// breakdown. 0.5ms of error drifts 30ms in about 28 seconds; anything
    /// coarser makes coasting pointless.
    #[test]
    fn the_period_is_precise_enough_to_coast_on() {
        let bpm = 128.0;
        let (_, grid) = run(&click_train(bpm, 30.0));
        let period = grid.period_ms.unwrap();
        let truth = 60_000.0 / bpm;
        assert!(
            (period - truth).abs() < 0.5,
            "period off by {:.2}ms ({period:.2} vs {truth:.2})",
            (period - truth).abs()
        );
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
