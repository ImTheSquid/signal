//! Which of sixteen beats is the first one.
//!
//! The tracker knows where the beats are and says so, but its counter starts
//! wherever the daemon did — `Grid::beat_index` is phase-locked to the beat and
//! not to the bar. This finds the missing offset.
//!
//! It locks the **sixteen-beat phrase**, and bar phase falls out as `% 4`, which
//! is the opposite of the obvious order. Estimating the bar directly looks easier
//! — four bins instead of sixteen, so four times the evidence each — but in
//! loop-based dance music a four-bar loop repeats near-identically, and there is
//! frequently no audio cue at all distinguishing bar one from bar three. Phrase
//! boundaries are where the large events live: drops, breakdowns, filter opens,
//! crashes. The harder-looking target carries more signal, and it is the one the
//! light actually wants.
//!
//! Nothing here reaches the wire. `flags::BAR_VALID` stays clear until the
//! numbers say it should not.

use crate::bands::Levels;
use crate::tempo::Grid;

const BINS: usize = 16;

/// Beats over which a bin's evidence halves. Sixteen bins need several phrases
/// to fill, so this cannot be short; but a DJ set changes phrase alignment at
/// every transition, so it cannot be long either. Forty-eight beats is a little
/// over three phrases, ~22s at 128 BPM.
const HALF_LIFE_BEATS: f32 = 48.0;

/// A period this much different is a different track, so the evidence is stale.
const TRACK_CHANGE: f32 = 0.03;


/// The estimate, as of a hop.
#[derive(Clone, Copy, Debug, Default)]
pub struct Phrase {
    /// The `beat_index` value believed to be phase zero.
    pub anchor: u8,
    /// How far the winning bin stands above the other fifteen, in standard
    /// deviations of those fifteen.
    ///
    /// Not a 0..1 concentration, and not for lack of trying one: circular
    /// concentration over sixteen bins divides the peak's excess by the *total*,
    /// and since every beat carries a kick, the baseline swamps it. Measured, a
    /// real phrase scored 0.060 against 0.037 for a flat click train — a ratio no
    /// threshold can use. An outlier test is not diluted by the baseline.
    ///
    /// Calibration: sixteen bins of pure noise put their own maximum about 1.8
    /// sigma above the mean, so ~1.8 is the "no signal" reading, not zero.
    pub excess_sigma: f32,
    /// Beats of evidence since the last reset. Low means "no opinion yet",
    /// which is a different thing from low confidence.
    pub beats: u32,
}

impl Phrase {
    /// Phrase phase of a beat index, 0..16, given this anchor.
    pub fn phase_of(&self, beat_index: u8) -> u8 {
        (beat_index + BINS as u8 - self.anchor % BINS as u8) % BINS as u8
    }

    /// Bar phase, 0..4 — derived, never estimated separately.
    pub fn bar_phase_of(&self, beat_index: u8) -> u8 {
        self.phase_of(beat_index) % 4
    }
}

pub struct PhraseClock {
    bins: [f32; BINS],
    /// Which beat the novelty currently accumulating belongs to.
    open_beat: Option<u8>,
    peak_flux: f32,
    prev_bands: Option<(f32, f32, f32)>,
    last_period: Option<f32>,
    beats: u32,
}

impl Default for PhraseClock {
    fn default() -> Self {
        Self::new()
    }
}

impl PhraseClock {
    pub fn new() -> Self {
        PhraseClock {
            bins: [0.0; BINS],
            open_beat: None,
            peak_flux: 0.0,
            prev_bands: None,
            last_period: None,
            beats: 0,
        }
    }

    fn reset(&mut self) {
        self.bins = [0.0; BINS];
        self.open_beat = None;
        self.peak_flux = 0.0;
        self.prev_bands = None;
        self.beats = 0;
    }

    /// One hop. Returns the estimate as it now stands.
    pub fn push(&mut self, levels: &Levels, grid: &Grid) -> Phrase {
        // A track change invalidates the alignment, not the tempo. Two tracks
        // mixed at the same BPM will not trip this — the decay is what handles
        // that case, more slowly.
        if let (Some(now), Some(was)) = (grid.period_ms, self.last_period) {
            if was > 0.0 && (now - was).abs() / was > TRACK_CHANGE {
                self.reset();
            }
        }
        if grid.period_ms.is_some() {
            self.last_period = grid.period_ms;
        }

        // Rise, not level: the flux detector peaks at an onset and decays
        // through the ring-out, so a crash that bleeds into the next beat is
        // credited to the beat it started on.
        self.peak_flux = self.peak_flux.max(levels.flux);

        match self.open_beat {
            Some(open) if open != grid.beat_index => {
                self.close(open, levels);
                self.open_beat = Some(grid.beat_index);
                self.peak_flux = levels.flux;
            }
            None => {
                self.open_beat = Some(grid.beat_index);
                self.peak_flux = levels.flux;
            }
            _ => {}
        }

        self.report()
    }

    /// Credit a finished beat's novelty to its bin.
    fn close(&mut self, index: u8, levels: &Levels) {
        let bands = (levels.low, levels.mid, levels.high);
        // Rise only, never fall. An absolute difference credits a crash twice —
        // once to the beat where the highs jump up, again to the beat where they
        // fall back — which put the estimate one beat late.
        //
        // All three bands, kept after trying to drop the low one. The reasoning for
        // dropping it was that the kick is on every beat and so says nothing about
        // *which* beat, only diluting the bins that do. It measured worse: phrase
        // consistency 53% -> 48%, bar 64% -> 60%. The kick is not uniform after all —
        // it drops out in breakdowns and doubles on fills — and that is information.
        let change = match self.prev_bands {
            Some((l, m, h)) => {
                (bands.0 - l).max(0.0) + (bands.1 - m).max(0.0) + (bands.2 - h).max(0.0)
            }
            // The first beat has nothing to be a change from.
            None => 0.0,
        };
        self.prev_bands = Some(bands);

        let decay = 0.5f32.powf(1.0 / HALF_LIFE_BEATS);
        for b in &mut self.bins {
            *b *= decay;
        }
        self.bins[index as usize % BINS] += self.peak_flux + change;
        self.beats = self.beats.saturating_add(1);
    }

    fn report(&self) -> Phrase {
        let total: f32 = self.bins.iter().sum();
        if total <= 0.0 {
            // Still report the beat count: "no evidence yet" and "evidence that
            // points nowhere" are different states, and only the second is a
            // reason to distrust a lock.
            return Phrase {
                beats: self.beats,
                ..Phrase::default()
            };
        }

        // A plain argmax. Hysteresis was tried — requiring a challenger to beat the
        // held bin by 15% before the anchor moves — on the theory that the ~53%
        // consistency was an anchor flipping between near-equal bins. It measured
        // *identical*, so the anchor is not flipping; it genuinely finds different
        // phases at different times.
        let (anchor, peak) = self
            .bins
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(i, &w)| (i as u8, w))
            .unwrap_or((0, 0.0));

        // Mean and deviation of the *losers* only. Including the peak in its own
        // reference inflates the deviation and hides exactly what is being asked.
        let rest: Vec<f32> = self
            .bins
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != anchor as usize)
            .map(|(_, &w)| w)
            .collect();
        let mean = rest.iter().sum::<f32>() / rest.len() as f32;
        let var = rest.iter().map(|w| (w - mean) * (w - mean)).sum::<f32>()
            / rest.len() as f32;
        let sd = var.sqrt();

        Phrase {
            anchor,
            // A zero deviation means every loser is identical, so any peak at all
            // is infinitely surprising; report it as flat instead of as certain.
            excess_sigma: match sd > 0.0 {
                true => (peak - mean) / sd,
                false => 0.0,
            },
            beats: self.beats,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::{synth_click_train, synth_pattern, Analyzer};
    use crate::tempo::HOP;

    const RATE: u32 = 48_000;
    const BPM: f32 = 128.0;
    const SECS: f32 = 150.0;

    struct Run {
        phrase: Phrase,
        /// `(hop start in seconds, beat_index)` for every hop.
        trail: Vec<(f64, u8)>,
    }

    fn run(samples: &[f32]) -> Run {
        let mut a = Analyzer::new(RATE).unwrap();
        let mut trail = Vec::new();
        let mut phrase = Phrase::default();
        for hop in samples.chunks_exact(HOP) {
            let t = a.elapsed_s();
            let r = a.push_hop(hop).unwrap();
            trail.push((t, r.grid.beat_index));
            phrase = r.phrase;
        }
        Run { phrase, trail }
    }

    /// The anchor the estimator *should* find, recovered from the fixture rather
    /// than assumed: `synth_pattern` puts its crash on audio beats 0, 16, 32…, so
    /// `beat_index - audio_beat` is a constant offset once the tempo is locked,
    /// and that offset is the anchor by definition. Taken as a mode over the
    /// second half, because the first half is aubio finding the tempo.
    fn true_anchor(r: &Run) -> u8 {
        let period = 60.0 / BPM as f64;
        let mut votes = [0u32; BINS];
        for &(t, idx) in &r.trail {
            if t < SECS as f64 / 2.0 {
                continue;
            }
            let audio_beat = (t / period).round() as i64;
            let off = (idx as i64 - audio_beat).rem_euclid(BINS as i64) as usize;
            votes[off] += 1;
        }
        votes
            .iter()
            .enumerate()
            .max_by_key(|(_, &v)| v)
            .map(|(i, _)| i as u8)
            .unwrap()
    }

    #[test]
    fn finds_the_phrase_in_a_pattern_that_has_one() {
        let r = run(&synth_pattern(BPM, SECS, RATE));
        let truth = true_anchor(&r);
        println!(
            "pattern: anchor {} truth {} excess {:.2} sigma, beats {}",
            r.phrase.anchor, truth, r.phrase.excess_sigma, r.phrase.beats
        );
        assert_eq!(
            r.phrase.anchor, truth,
            "locked phase {} but the crash is on {truth}",
            r.phrase.anchor
        );
    }

    /// The case that matters more than the one above. A click train has no
    /// strong/weak pattern at all, so there is no phrase to find, and the failure
    /// that would matter live is a *confident wrong answer* — the light placing
    /// its big moves on a boundary that is not there. A flat input has to read as
    /// flat, with enough daylight between the two that a threshold can sit in it.
    #[test]
    fn stays_unconvinced_when_there_is_no_phrase() {
        let pattern = run(&synth_pattern(BPM, SECS, RATE));
        let flat = run(&synth_click_train(BPM, SECS, RATE));
        println!(
            "click train {:.2} sigma vs pattern {:.2} sigma",
            flat.phrase.excess_sigma, pattern.phrase.excess_sigma
        );
        // Sixteen bins of noise put their own max ~1.8 sigma up by chance, so the
        // bar is against that rather than against zero.
        assert!(
            flat.phrase.excess_sigma < 3.0,
            "a flat click train read {:.2} sigma, which a threshold would take \
             for a real phrase",
            flat.phrase.excess_sigma
        );
        assert!(
            pattern.phrase.excess_sigma > flat.phrase.excess_sigma * 2.0,
            "{:.2} sigma for a real phrase against {:.2} for noise leaves no \
             room for a threshold",
            pattern.phrase.excess_sigma,
            flat.phrase.excess_sigma
        );
    }

    /// A track change has to drop the evidence rather than average across it.
    #[test]
    fn forgets_the_old_track_on_a_tempo_change() {
        let mut clock = PhraseClock::new();
        let levels = Levels::default();
        let grid = |period: f32, idx: u8| Grid {
            period_ms: Some(period),
            beat_index: idx,
            ..Grid::default()
        };

        for i in 0..64u8 {
            clock.push(&levels, &grid(469.0, i % 16));
        }
        let before = clock.push(&levels, &grid(469.0, 0)).beats;
        assert!(before > 0, "gathered no evidence at all");

        let after = clock.push(&levels, &grid(600.0, 0)).beats;
        assert_eq!(after, 0, "kept {after} beats of the previous track's phase");
    }
}
