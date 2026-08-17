//! TempoCNN: the period, as a classification rather than a periodicity.
//!
//! aubio reports *a* periodicity and frequently the wrong metrical level. Measured
//! against rekordbox's grids over 60 library tracks: aubio with the hand-built comb
//! in `metrical.rs` gets 68% of tempi right, this gets 90%. The difference is that a
//! comb score has no notion of which tempi are plausible, so it cannot tell a
//! correct 127 BPM from a 127 that should be 190 — and a classifier over 256
//! absolute tempo bins can, because 87 and 174 are separate classes competing on
//! evidence rather than two readings of one peak.
//!
//! The model has no phase. It says only how fast, and `AubioTracker` keeps
//! supplying where the beats fall.

// burn 0.22 selects the backend globally rather than by type parameter, so `Device`
// and `Tensor<D>` come from the prelude with no backend to name — the same way the
// generated model is written.
use burn::prelude::*;

use crate::melbands::{Window, FRAMES, N_MELS};

mod generated {
    include!(concat!(env!("OUT_DIR"), "/model/tempocnn.rs"));
}

/// Class *i* of the softmax is `i + 30` BPM.
const BASE_BPM: usize = 30;
const CLASSES: usize = 256;

/// Below this, a doubling is considered. Above it, doubling would leave the range
/// any of this music lives in.
const DOUBLE_BELOW: f32 = 115.0;

/// Mass at 2x, relative to the peak, that counts as a real competing candidate.
///
/// Measured over the four tracks in the danger zone across both sets: the ones that
/// genuinely needed doubling read 0.396, 0.232, 0.081 and 0.485, and the
/// legitimately-slow ones read 0.021 and 0.022. The rule takes tempo accuracy from
/// 83% to 90%, and held out from 81% to 88%. The margin is real but thin, and it is
/// evidenced by four tracks rather than forty.
const DOUBLE_MASS: f32 = 0.05;

/// BPM either side of a target counted as that target's mass.
const MASS_WIDTH: usize = 3;

pub struct TempoModel {
    model: generated::Model,
    device: Device,
}

#[derive(Clone, Copy, Debug)]
pub struct Estimate {
    pub bpm: f32,
    /// Probability at the winning class, before any doubling.
    pub peak: f32,
    /// Whether the doubling rule fired.
    pub doubled: bool,
}

impl Estimate {
    pub fn period_ms(&self) -> f32 {
        60_000.0 / self.bpm
    }
}

impl TempoModel {
    /// Loads the weights. `Model::new` would build the same graph with
    /// *uninitialised* parameters and run perfectly happily, returning a flat
    /// 1/256 softmax — so the weights are not optional and not a detail.
    pub fn new() -> Self {
        let device = Default::default();
        let weights = concat!(env!("OUT_DIR"), "/model/tempocnn.bpk");
        TempoModel {
            model: generated::Model::from_file(weights, &device),
            device,
        }
    }

    pub fn estimate(&self, window: &Window) -> Estimate {
        debug_assert_eq!(window.0.len(), N_MELS * FRAMES);
        let input = Tensor::<1>::from_data(TensorData::from(window.0.as_slice()), &self.device)
            .reshape([1, N_MELS, FRAMES, 1]);
        let dist: Vec<f32> = self.model.forward(input).to_data().to_vec().unwrap();
        interpret(&dist)
    }
}

/// Softmax to a BPM, with the doubling rule applied.
fn interpret(dist: &[f32]) -> Estimate {
    let (idx, peak) = dist
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map(|(i, &p)| (i, p))
        .unwrap_or((0, 0.0));

    // Quadratic interpolation against the neighbours, as tempocnn's `--interpolate`
    // does: the classes are 1 BPM apart and the true tempo rarely is.
    let bpm = interpolate(dist, idx);

    let doubled = bpm < DOUBLE_BELOW && mass(dist, bpm * 2.0) / peak.max(1e-9) > DOUBLE_MASS;
    Estimate {
        bpm: match doubled {
            true => bpm * 2.0,
            false => bpm,
        },
        peak,
        doubled,
    }
}

/// Probability within `MASS_WIDTH` BPM of a target.
fn mass(dist: &[f32], bpm: f32) -> f32 {
    let centre = bpm.round() as i64 - BASE_BPM as i64;
    let lo = (centre - MASS_WIDTH as i64).max(0) as usize;
    let hi = ((centre + MASS_WIDTH as i64 + 1).max(0) as usize).min(dist.len());
    match hi > lo {
        true => dist[lo..hi].iter().sum(),
        false => 0.0,
    }
}

/// Parabola through the peak and its neighbours, in BPM.
fn interpolate(dist: &[f32], idx: usize) -> f32 {
    let bpm = (idx + BASE_BPM) as f32;
    if idx == 0 || idx + 1 >= dist.len() {
        return bpm;
    }
    let (a, b, c) = (dist[idx - 1], dist[idx], dist[idx + 1]);
    let denom = a - 2.0 * b + c;
    if denom.abs() < 1e-12 {
        return bpm;
    }
    // Offset is bounded to half a class; anything larger means the peak is not
    // where argmax said and the parabola is not describing it.
    let offset = (0.5 * (a - c) / denom).clamp(-0.5, 0.5);
    bpm + offset
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spike(at: usize, height: f32) -> Vec<f32> {
        let mut d = vec![(1.0 - height) / (CLASSES as f32 - 1.0); CLASSES];
        d[at] = height;
        d
    }

    #[test]
    fn a_clean_peak_reads_as_its_own_bpm() {
        // Class 115 is 145 BPM.
        let e = interpret(&spike(115, 0.9));
        assert!(!e.doubled, "doubled a clean 145 BPM peak");
        assert!((e.bpm - 145.0).abs() < 0.6, "read {} for 145 BPM", e.bpm);
    }

    /// The case the rule exists for: 200 BPM heard as 100, with real mass at 200.
    #[test]
    fn a_half_tempo_peak_with_mass_at_double_is_doubled() {
        let mut d = spike(70, 0.60); // 100 BPM
        for i in 167..=173 {
            d[i] = 0.04; // ~0.28 total around 200 BPM
        }
        let e = interpret(&d);
        assert!(e.doubled, "did not double 100 BPM with 0.28 mass at 200");
        assert!((e.bpm - 200.0).abs() < 1.0, "read {} for 200 BPM", e.bpm);
    }

    /// The case the rule must not break. 90 BPM tracks exist, and the measured
    /// ratio for a genuine one was 0.021 — well under the bar.
    #[test]
    fn a_genuinely_slow_peak_is_left_alone() {
        let mut d = spike(60, 0.93); // 90 BPM
        d[150] = 0.01; // a whisper at 180
        let e = interpret(&d);
        assert!(!e.doubled, "doubled a genuine 90 BPM track");
        assert!((e.bpm - 90.0).abs() < 0.6, "read {} for 90 BPM", e.bpm);
    }

    /// Doubling only applies where doubling could be right.
    #[test]
    fn a_fast_peak_is_never_doubled() {
        let mut d = spike(120, 0.5); // 150 BPM
        d[240] = 0.3; // 270 BPM, outside anything this plays
        let e = interpret(&d);
        assert!(!e.doubled, "doubled 150 BPM to 300");
    }

    /// Parity with ONNX Runtime, which is the only thing that says the conversion
    /// preserved the model rather than merely producing something that runs.
    ///
    /// The reference comes from `tools/ref_check.py` on the same deterministic ramp:
    /// argmax 117 = 147 BPM at p=0.395794. Keras to ONNX was separately checked to
    /// 6.6e-07, so this closes the chain from the published `cnn.h5` to what the
    /// daemon executes.
    #[test]
    fn the_generated_model_matches_onnx_runtime() {
        let ramp: Vec<f32> = (0..N_MELS * FRAMES)
            .map(|i| (i % 97) as f32 / 97.0)
            .collect();
        let m = TempoModel::new();
        let e = m.estimate(&Window(ramp));

        // No doubling: 147 is well above the threshold, so this reads the raw peak.
        assert!(!e.doubled, "doubled the reference input");
        assert!(
            (e.bpm - 147.0).abs() < 0.6,
            "got {} BPM, ONNX Runtime gives 147",
            e.bpm
        );
        assert!(
            (e.peak - 0.395_794).abs() < 1e-4,
            "peak probability {} against ONNX Runtime's 0.395794",
            e.peak
        );
    }

    /// The weights have to actually load. `Model::new` returns uninitialised
    /// parameters and a flat softmax without complaining, so "it ran" is not
    /// evidence of anything.
    #[test]
    fn the_loaded_model_is_not_a_flat_softmax() {
        let m = TempoModel::new();
        let flat = Window(vec![0.5; N_MELS * FRAMES]);
        let e = m.estimate(&flat);
        assert!(
            e.peak > 2.0 / CLASSES as f32,
            "peak probability {} is indistinguishable from 1/256, so the weights \
             did not load",
            e.peak
        );
    }
}
