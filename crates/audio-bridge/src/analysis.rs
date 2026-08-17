//! One hop of audio in, one view of the music out.
//!
//! The live path and the offline path share this exactly. That is the point:
//! tuning happens against a WAV file with a deterministic clock, and what runs
//! at the gig is the same code with a different source of samples.

use anyhow::{bail, Context, Result};

use crate::bands::{Bands, Levels};
use crate::downbeat::{Phrase, PhraseClock};
use crate::tempo::{AubioTracker, Grid, Tracker, HOP};
use crate::wire::Beat;

/// Anything above this in a hop means the input is clipping, which usually
/// means the clone is being fed too hot rather than the music being loud.
const CLIP_THRESHOLD: f32 = 0.999;

/// Broadband envelope below this counts as no audio at all — a different thing
/// from "quiet", and the difference the operator needs to see when the DJ
/// software is pointed at the wrong output device.
const PRESENCE_FLOOR: f32 = 1.0e-4;

pub struct Analyzer {
    bands: Bands,
    tracker: Box<dyn Tracker>,
    phrase: PhraseClock,
    sample_rate: u32,
    hops: u64,
}

/// What one hop produced.
pub struct HopResult {
    pub levels: Levels,
    pub grid: Grid,
    /// A beat landed in this hop.
    pub beat: bool,
    /// Where the sixteen-beat phrase is believed to start. Deliberately not on
    /// [`Beat`] — nothing goes to the light until this is measured.
    pub phrase: Phrase,
}

impl Analyzer {
    pub fn new(sample_rate: u32) -> Result<Self> {
        Ok(Analyzer {
            bands: Bands::new(sample_rate, HOP),
            tracker: Box::new(AubioTracker::new(sample_rate)?),
            phrase: PhraseClock::new(),
            sample_rate,
            hops: 0,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Seconds of audio consumed so far. Derived from hops, not wall clock.
    pub fn elapsed_s(&self) -> f64 {
        self.hops as f64 * HOP as f64 / self.sample_rate as f64
    }

    pub fn push_hop(&mut self, hop: &[f32]) -> Result<HopResult> {
        if hop.len() != HOP {
            bail!("hop must be exactly {HOP} samples, got {}", hop.len());
        }
        for &s in hop {
            self.bands.push(s);
        }
        let levels = self.bands.sample();
        let beat = self.tracker.push(hop, &levels)?;
        let grid = self.tracker.grid();
        let phrase = self.phrase.push(&levels, &grid);
        self.hops += 1;
        Ok(HopResult {
            levels,
            grid,
            beat,
            phrase,
        })
    }
}

/// Assemble what goes on the wire, applying the one latency knob.
///
/// `offset_ms` shifts the reported beat instant so the render script never has
/// to do latency arithmetic. Negative fires early, which is the direction to
/// bias: light arriving before sound reads as tight, light arriving after
/// reads as wrong, and the tolerance is far wider on the early side.
pub fn to_beat(result: &HopResult, hop: &[f32], offset_ms: f32) -> Beat {
    let mut grid = result.grid;
    if let Some(ms) = grid.ms_to_next_beat {
        // Clamped rather than wrapped: if the offset puts the target in the
        // past, "now" is the honest answer. Naming the next beat instead would
        // claim a full period of quiet that is not there.
        grid.ms_to_next_beat = Some((ms + offset_ms).max(0.0));
    }
    Beat {
        grid,
        levels: result.levels,
        // The decaying peak, not the instantaneous envelope: a kick pattern is
        // silence between hits, and testing the instant would report "no audio"
        // to the light on every offbeat.
        audio_present: result.levels.raw_peak > PRESENCE_FLOOR,
        clipping: hop.iter().any(|s| s.abs() >= CLIP_THRESHOLD),
    }
}

/// A click train: a short decaying noise burst on each beat, which stands in
/// for a kick far better than an impulse does.
///
/// This is the calibration signal. Driving the light from a known-perfect grid
/// is what makes the lamp-rise measurement meaningful — any offset seen on
/// camera is the rig's, not the tracker's.
pub fn synth_click_train(bpm: f32, seconds: f32, sample_rate: u32) -> Vec<f32> {
    let fs = sample_rate as f32;
    let n = (fs * seconds) as usize;
    let mut out = vec![0.0f32; n];

    // Deterministic pseudo-noise: a fixture must not vary between runs, and
    // this is not worth a dependency.
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut noise = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 40) as f32 / 8_388_608.0 - 1.0
    };

    let burst = (fs * 0.05) as usize;
    let decay_samples = fs * 0.010;
    let period = 60.0 / bpm;
    let mut beat = 0.0f32;
    while beat < seconds {
        let start = (beat * fs) as usize;
        for i in 0..burst {
            match out.get_mut(start + i) {
                Some(slot) => *slot += noise() * (-(i as f32) / decay_samples).exp(),
                None => break,
            }
        }
        beat += period;
    }
    out
}

/// Four-on-the-floor with a bar and a phrase in it: a kick every beat, a clap on
/// two and four, and a bright crash every sixteen.
///
/// `synth_click_train` is deliberately the opposite — uniform bursts, no
/// strong/weak pattern, which is what makes it a calibration grid and also what
/// makes it useless for a downbeat estimator. This is the fixture with an answer
/// in it, and the answer is known by construction: beat zero is phase zero, so
/// the truth for any beat is its index mod 16.
///
/// Note what is *not* distinguishable here, on purpose, because it is not
/// distinguishable in real music either: the clap falls on phases 1 and 3, so
/// bar phase 0 and bar phase 2 render identically. Only the crash says which of
/// the sixteen beats is the first, which is why the estimator locks the phrase
/// and takes the bar from it rather than the other way round.
pub fn synth_pattern(bpm: f32, seconds: f32, sample_rate: u32) -> Vec<f32> {
    let fs = sample_rate as f32;
    let n = (fs * seconds) as usize;
    let mut out = vec![0.0f32; n];

    // Same generator and the same reason as the click train: a fixture must not
    // vary between runs.
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut noise = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 40) as f32 / 8_388_608.0 - 1.0
    };

    let period = 60.0 / bpm;
    let mut t = 0.0f32;
    let mut beat = 0usize;
    while t < seconds {
        let start = (t * fs) as usize;
        let phase = beat % 16;

        // A decaying 55Hz sine, so the kick lands in the 30-160Hz band rather
        // than smearing across all three the way a noise burst does.
        for i in 0..(fs * 0.12) as usize {
            let x = i as f32 / fs;
            let env = (-x / 0.045).exp();
            match out.get_mut(start + i) {
                Some(slot) => {
                    *slot += (std::f32::consts::TAU * 55.0 * x).sin() * env * 0.9
                }
                None => break,
            }
        }

        if phase % 4 == 1 || phase % 4 == 3 {
            for i in 0..(fs * 0.06) as usize {
                let x = i as f32 / fs;
                let env = (-x / 0.020).exp();
                match out.get_mut(start + i) {
                    Some(slot) => *slot += noise() * env * 0.35,
                    None => break,
                }
            }
        }

        // Long and loud, and the only cue to the phrase. It rings into the next
        // beat on purpose — crashes do — which is a fair test of an estimator
        // built on a rise detector rather than on absolute level.
        if phase == 0 {
            for i in 0..(fs * 0.5) as usize {
                let x = i as f32 / fs;
                let env = (-x / 0.18).exp();
                match out.get_mut(start + i) {
                    Some(slot) => *slot += noise() * env * 0.55,
                    None => break,
                }
            }
        }

        t += period;
        beat += 1;
    }
    out
}

/// Read a WAV file down to mono at its own sample rate.
///
/// No resampling anywhere in this crate — everything is parameterised on the
/// rate, so a 44.1k file just makes the hop 5.8ms instead of 5.33ms.
pub fn read_wav_mono(path: &str) -> Result<(Vec<f32>, u32)> {
    let mut reader =
        hound::WavReader::open(path).with_context(|| format!("opening {path}"))?;
    let spec = reader.spec();
    let channels = spec.channels.max(1) as usize;

    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<Result<Vec<_>, _>>()
            .context("reading float samples")?,
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 * scale))
                .collect::<Result<Vec<_>, _>>()
                .context("reading integer samples")?
        }
    };

    let mono = interleaved
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect();
    Ok((mono, spec.sample_rate))
}
