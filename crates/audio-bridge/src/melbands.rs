//! TempoCNN's input features: 40 mel bands at 11025Hz, 256 frames deep.
//!
//! Deliberately separate from `bands.rs`. Those are Butterworth envelopes in the
//! time domain at the capture rate, and they decide the light's colour; this is a
//! spectrogram at a fixed rate feeding a model. Sharing anything between them would
//! couple two things that only look alike.
//!
//! The contract is copied from `tempocnn/feature.py` and is exact, because every
//! part of it fails silently rather than loudly:
//!
//! - mono, resampled to 11025Hz
//! - STFT n_fft=1024, hop 512, so 21.53 frames/sec
//! - mel: 40 bands, 20..5000Hz, **magnitude not power**, and **no log or dB**
//! - a window is 256 frames, 11.89 seconds
//!
//! The filterbank is read as data rather than derived. librosa's default is the
//! Slaney mel scale with area normalisation, not the HTK formula, and a hand-rolled
//! version would shift every band a little and cost accuracy nothing would report.
//! `tools/mel_filters.py` dumps it.

use std::sync::Arc;

use anyhow::{Context, Result};
use realfft::{num_complex::Complex32, RealFftPlanner, RealToComplex};
use rubato::{FftFixedIn, Resampler};

pub const MODEL_RATE: usize = 11_025;
pub const N_FFT: usize = 1024;
pub const HOP: usize = 512;
pub const N_MELS: usize = 40;
pub const BINS: usize = N_FFT / 2 + 1;
/// Frames the model takes at once: 11.89s at 21.53 frames/sec.
pub const FRAMES: usize = 256;

/// Row-major `[N_MELS][BINS]` f32, from `tools/mel_filters.py`.
static FILTERBANK: &[u8] = include_bytes!("../data/mel_40x513_f32.bin");

/// One window, laid out as the model's `[1, 40, 256, 1]` expects: mel-major, so
/// all 256 frames of band 0, then all of band 1.
pub struct Window(pub Vec<f32>);

pub struct MelBands {
    resampler: FftFixedIn<f32>,
    fft: Arc<dyn RealToComplex<f32>>,
    filters: Vec<f32>,
    /// Resampled audio not yet consumed by the resampler's fixed input block.
    pending: Vec<f32>,
    /// Resampled audio waiting to become STFT frames.
    samples: Vec<f32>,
    /// Ring of the last FRAMES mel frames, each N_MELS long.
    frames: std::collections::VecDeque<[f32; N_MELS]>,
    hann: Vec<f32>,
    scratch: Vec<f32>,
    spectrum: Vec<Complex32>,
}

impl MelBands {
    pub fn new(input_rate: u32) -> Result<Self> {
        // 48k is 147/640 and 44.1k is exactly 1/4, so this has to be a general
        // rational resampler rather than a decimation.
        let resampler = FftFixedIn::<f32>::new(input_rate as usize, MODEL_RATE, 1024, 1, 1)
            .context("building the 11025Hz resampler")?;

        let filters: Vec<f32> = FILTERBANK
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        anyhow::ensure!(
            filters.len() == N_MELS * BINS,
            "filterbank is {} floats, expected {}",
            filters.len(),
            N_MELS * BINS
        );

        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(N_FFT);
        Ok(MelBands {
            resampler,
            spectrum: vec![Complex32::default(); BINS],
            fft,
            filters,
            pending: Vec::new(),
            samples: Vec::new(),
            frames: std::collections::VecDeque::with_capacity(FRAMES),
            // librosa's default window: periodic Hann, matching `sym=False`.
            hann: (0..N_FFT)
                .map(|i| {
                    let x = std::f32::consts::TAU * i as f32 / N_FFT as f32;
                    0.5 - 0.5 * x.cos()
                })
                .collect(),
            scratch: vec![0.0; N_FFT],
        })
    }

    /// Feed capture-rate mono samples. Any amount; blocking is handled inside.
    pub fn push(&mut self, input: &[f32]) -> Result<()> {
        self.pending.extend_from_slice(input);
        let need = self.resampler.input_frames_next();
        while self.pending.len() >= need {
            let block: Vec<f32> = self.pending.drain(..need).collect();
            let out = self
                .resampler
                .process(&[block], None)
                .context("resampling to 11025Hz")?;
            self.samples.extend_from_slice(&out[0]);
        }
        self.drain_frames();
        Ok(())
    }

    /// Feed samples that are *already* at 11025Hz, skipping the resampler.
    ///
    /// Exists so the spectrogram can be checked against librosa on its own. At a
    /// ratio of 1.0 the resampler is still an FFT filter with its own latency, so
    /// leaving it in the path would compare two things at once and blame the wrong
    /// one when they disagreed.
    pub fn push_at_model_rate(&mut self, input: &[f32]) {
        self.samples.extend_from_slice(input);
        self.drain_frames();
    }

    fn drain_frames(&mut self) {
        while self.samples.len() >= N_FFT {
            self.frame();
            self.samples.drain(..HOP);
        }
        // Bound the buffer: only the newest FRAMES matter, and a long run must not
        // grow it without limit.
        while self.frames.len() > FRAMES {
            self.frames.pop_front();
        }
    }

    fn frame(&mut self) {
        for (s, (x, w)) in self
            .scratch
            .iter_mut()
            .zip(self.samples[..N_FFT].iter().zip(&self.hann))
        {
            *s = x * w;
        }
        if self.fft.process(&mut self.scratch, &mut self.spectrum).is_err() {
            return;
        }

        let mut mel = [0.0f32; N_MELS];
        for (m, out) in mel.iter_mut().enumerate() {
            let row = &self.filters[m * BINS..(m + 1) * BINS];
            // `power=1`: magnitude, not squared. Squaring here is the single easiest
            // way to hand the model something it has never seen.
            *out = row
                .iter()
                .zip(&self.spectrum)
                .map(|(f, c)| f * c.norm())
                .sum();
        }
        self.frames.push_back(mel);
    }

    /// True once a full window exists. The model has nothing to say before this,
    /// which is 11.89s after audio starts.
    pub fn ready(&self) -> bool {
        self.frames.len() >= FRAMES
    }

    /// The newest window, mel-major, normalised the way `cnn` expects: divided by
    /// its own maximum.
    ///
    /// That normalisation is *not* in the ONNX graph — it happens in
    /// `tempocnn/classifier.py` before the model is called — so it has to happen
    /// here. `cnn` uses `max_normalizer`; the deeptemp and shallowtemp families use
    /// zero-mean-unit-variance instead, which would matter if the model changed.
    pub fn window(&self) -> Option<Window> {
        if !self.ready() {
            return None;
        }
        let start = self.frames.len() - FRAMES;
        let mut out = vec![0.0f32; N_MELS * FRAMES];
        let mut max = 0.0f32;
        for (t, frame) in self.frames.iter().skip(start).enumerate() {
            for (m, &v) in frame.iter().enumerate() {
                out[m * FRAMES + t] = v;
                max = max.max(v);
            }
        }
        if max > 0.0 {
            for v in &mut out {
                *v /= max;
            }
        }
        Some(Window(out))
    }

    /// Un-normalised mel frames, for checking against librosa. `window()`
    /// divides by the max, which would hide a scale error in the filterbank.
    #[cfg(test)]
    pub fn raw_frames(&self) -> Vec<[f32; N_MELS]> {
        self.frames.iter().copied().collect()
    }
}

/// A tone pair both this and librosa can generate identically, so a parity check
/// has no file to agree about.
#[cfg(test)]
pub fn probe_signal(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let t = i as f32 / MODEL_RATE as f32;
            0.5 * (std::f32::consts::TAU * 440.0 * t).sin()
                + 0.2 * (std::f32::consts::TAU * 1000.0 * t).sin()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frozen from librosa via `tools/mel_parity.py`, on `probe_signal`: the first
    /// eight bands of frame 0. Checked once against the real thing and then
    /// embedded, so the check keeps working without Python in the loop.
    ///
    /// These are librosa's `center=False` numbers. Its default, and TempoCNN's, is
    /// `center=True`, which pads half a window of zeros in front — that shifts only
    /// the first frame of a stream and is meaningless to a window that slides, so
    /// the streaming path does not pad and frame 0 here is librosa's frame 1 there.
    const LIBROSA_FRAME0: [f32; 8] = [
        0.000_036_30,
        0.000_071_82,
        0.000_157_28,
        0.000_433_29,
        0.002_150_26,
        0.938_672_42,
        3.291_694_64,
        0.014_256_78,
    ];

    #[test]
    fn the_spectrogram_matches_librosa() {
        let mut mel = MelBands::new(MODEL_RATE as u32).unwrap();
        mel.push_at_model_rate(&probe_signal(N_FFT + HOP * 4));
        let frames = mel.raw_frames();
        assert!(!frames.is_empty(), "produced no frames at all");

        let got = &frames[0][..LIBROSA_FRAME0.len()];
        for (i, (&a, &b)) in got.iter().zip(&LIBROSA_FRAME0).enumerate() {
            // Relative, because the values span four orders of magnitude and an
            // absolute tolerance would only really test the loud bands.
            let scale = b.abs().max(1e-6);
            let rel = (a - b).abs() / scale;
            assert!(
                rel < 0.02,
                "band {i}: got {a:.8}, librosa {b:.8}, relative error {rel:.4}"
            );
        }
    }

    /// The window the model sees, not the frames: mel-major and max-normalised.
    #[test]
    fn a_window_is_mel_major_and_peaks_at_one() {
        let mut mel = MelBands::new(MODEL_RATE as u32).unwrap();
        assert!(mel.window().is_none(), "claimed a window before one existed");

        mel.push_at_model_rate(&probe_signal(N_FFT + HOP * (FRAMES + 4)));
        assert!(mel.ready(), "no window after feeding more than 256 frames");
        let w = mel.window().unwrap().0;
        assert_eq!(w.len(), N_MELS * FRAMES);

        let max = w.iter().copied().fold(0.0f32, f32::max);
        assert!((max - 1.0).abs() < 1e-6, "normalised peak is {max}, not 1.0");

        // Mel-major: a steady tone makes one band loud across every frame, so the
        // loud values must be contiguous. Laid out frame-major they would be strided.
        let loud = w.iter().position(|&v| v > 0.5).unwrap();
        assert!(
            w[loud + 1] > 0.5,
            "the value after the peak is quiet, so the layout is frame-major"
        );
    }

    /// Resampling has to land a tone in the same band it would at the model's own
    /// rate, or every band is shifted and nothing says so.
    #[test]
    fn resampling_preserves_which_band_a_tone_lands_in() {
        let native = {
            let mut m = MelBands::new(MODEL_RATE as u32).unwrap();
            m.push_at_model_rate(&probe_signal(N_FFT + HOP * 40));
            m.raw_frames()[20]
        };

        // The same two tones, generated at 48kHz and resampled down.
        let mut m = MelBands::new(48_000).unwrap();
        let n = 48_000 * 3;
        let at_48k: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / 48_000.0;
                0.5 * (std::f32::consts::TAU * 440.0 * t).sin()
                    + 0.2 * (std::f32::consts::TAU * 1000.0 * t).sin()
            })
            .collect();
        m.push(&at_48k).unwrap();
        let frames = m.raw_frames();
        assert!(frames.len() > 30, "only {} frames from 3s", frames.len());
        let resampled = frames[20];

        let peak = |f: &[f32; N_MELS]| {
            f.iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .map(|(i, _)| i)
                .unwrap()
        };
        assert_eq!(
            peak(&native),
            peak(&resampled),
            "tone peaks in band {} natively but {} after resampling",
            peak(&native),
            peak(&resampled)
        );
    }
}
