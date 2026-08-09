//! Band levels, and the musical flags derived from them.
//!
//! Deliberately independent of the tempo tracker. Levels answer "which lamp,
//! how bright"; the tracker answers "when". `scripts/README.md` records what
//! conflating those two cost on the DMX side — half the tempo lock — and the
//! same separation is why swapping the tracker later costs nothing here.

use std::f32::consts::PI;

/// Band edges, in Hz. Low is where the kick lives, high is where hats and air
/// live, mid is everything that carries the tune.
const LOW_HP: f32 = 30.0;
const LOW_LP: f32 = 160.0;
const MID_LP: f32 = 2_000.0;
const HIGH_LP: f32 = 12_000.0;

/// Envelope smoother. Fast enough to keep a kick transient's shape, slow enough
/// that the per-hop sample is not chasing individual cycles.
const ENVELOPE_TAU_S: f32 = 0.010;

/// Half-life of the per-band automatic gain reference. Long enough to survive a
/// breakdown, short enough to follow a genuinely quieter track.
const AGC_HALF_LIFE_S: f32 = 15.0;

/// A band's gain reference is never allowed below this fraction of the
/// broadband reference. Without it every band normalises its own noise floor to
/// full scale, and three lamps read hot on a bass-only drop. This is the same
/// idea as `follow.rhai`'s `floor 20.0` on a 0-255 channel — a floor that means
/// "quiet relative to the mix", not "quiet relative to nothing".
const BAND_FLOOR_RATIO: f32 = 0.05;

/// Absolute floor, guarding the division when there is no signal at all.
const SILENCE_FLOOR: f32 = 1.0e-6;

/// The reference `bass_muted` is judged against.
const BASS_REFERENCE_S: f32 = 60.0;
/// 20dB below reference. A bass EQ kill is far deeper than any musical dip.
const BASS_MUTED_RATIO: f32 = 0.1;
/// Sustained this long before the flag latches, so a single beat gap in the
/// kick pattern does not read as a kill.
const BASS_MUTED_HOLD_S: f32 = 0.3;

/// The two horizons whose difference stands in for the energy slope.
const BUILD_FAST_S: f32 = 1.0;
const BUILD_SLOW_S: f32 = 4.0;

/// Short horizons for "something just happened", which is what a script needs
/// to accent between beats — and the only such cue it has while coasting.
const FLUX_FAST_S: f32 = 0.03;
const FLUX_SLOW_S: f32 = 0.20;

/// Section Qs for a 6th-order Butterworth, `1 / (2 cos((2k+1)pi/12))`.
///
/// Three sections rather than one because 60Hz sits only 1.4 octaves below the
/// 160Hz crossover: at 12dB/oct the kick bleeds into the mid band at -34dB,
/// which is enough to light the wrong lamp. At 36dB/oct it is -51dB.
///
/// These specific Qs rather than three identical ones: cascading identical
/// sections puts -9dB at the nominal corner and drags the passband inward, so
/// the bands would no longer meet. A true Butterworth keeps -3dB at the corner.
const BUTTERWORTH_Q6: [f32; 3] = [0.517_638, 0.707_107, 1.931_852];

/// Direct Form II transposed.
#[derive(Clone, Copy)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    fn lowpass(fs: f32, f0: f32, q: f32) -> Self {
        let (cos_w0, alpha) = Self::terms(fs, f0, q);
        let a0 = 1.0 + alpha;
        Biquad {
            b0: (1.0 - cos_w0) / 2.0 / a0,
            b1: (1.0 - cos_w0) / a0,
            b2: (1.0 - cos_w0) / 2.0 / a0,
            a1: -2.0 * cos_w0 / a0,
            a2: (1.0 - alpha) / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn highpass(fs: f32, f0: f32, q: f32) -> Self {
        let (cos_w0, alpha) = Self::terms(fs, f0, q);
        let a0 = 1.0 + alpha;
        Biquad {
            b0: (1.0 + cos_w0) / 2.0 / a0,
            b1: -(1.0 + cos_w0) / a0,
            b2: (1.0 + cos_w0) / 2.0 / a0,
            a1: -2.0 * cos_w0 / a0,
            a2: (1.0 - alpha) / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    /// Returns `(cos w0, alpha)`.
    fn terms(fs: f32, f0: f32, q: f32) -> (f32, f32) {
        // Keep the corner clear of Nyquist: a 12kHz edge is meaningless at
        // 22.05k, and the response degenerates as it approaches.
        let f0 = f0.min(fs * 0.45);
        let w0 = 2.0 * PI * f0 / fs;
        (w0.cos(), w0.sin() / (2.0 * q))
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

/// A 6th-order Butterworth section stack.
#[derive(Clone, Copy)]
struct Cascade([Biquad; 3]);

impl Cascade {
    fn lowpass(fs: f32, f0: f32) -> Self {
        Cascade(BUTTERWORTH_Q6.map(|q| Biquad::lowpass(fs, f0, q)))
    }

    fn highpass(fs: f32, f0: f32) -> Self {
        Cascade(BUTTERWORTH_Q6.map(|q| Biquad::highpass(fs, f0, q)))
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        self.0.iter_mut().fold(x, |acc, s| s.process(acc))
    }
}

/// One-pole smoother over a rectified signal.
#[derive(Clone, Copy)]
struct Envelope {
    coeff: f32,
    value: f32,
}

impl Envelope {
    fn new(fs: f32, tau_s: f32) -> Self {
        Envelope {
            coeff: (-1.0 / (tau_s * fs)).exp(),
            value: 0.0,
        }
    }

    #[inline]
    fn process(&mut self, x: f32) {
        let x = x.abs();
        self.value = x + self.coeff * (self.value - x);
        // Denormals here cost more than the arithmetic they carry.
        if self.value < 1.0e-20 {
            self.value = 0.0;
        }
    }
}

/// Slow-decay peak reference, mirroring `follow.rhai`'s per-channel AGC: hold
/// the peak, let it sag, normalise against it. A fixed threshold cannot work
/// when one band's natural level is a fraction of another's.
#[derive(Clone, Copy)]
struct Agc {
    decay: f32,
    peak: f32,
}

impl Agc {
    fn new(hop_s: f32, half_life_s: f32) -> Self {
        Agc {
            decay: 0.5f32.powf(hop_s / half_life_s),
            peak: SILENCE_FLOOR,
        }
    }

    /// Normalise into 0..=1. `floor` keeps a near-empty band from amplifying
    /// its own leakage to full scale.
    fn normalise(&mut self, value: f32, floor: f32) -> f32 {
        self.peak = (self.peak * self.decay).max(floor).max(value);
        if self.peak <= SILENCE_FLOOR {
            return 0.0;
        }
        (value / self.peak).clamp(0.0, 1.0)
    }
}

/// Exponential moving average over a time constant, sampled per hop.
#[derive(Clone, Copy)]
pub struct Ema {
    coeff: f32,
    value: f32,
    primed: bool,
}

impl Ema {
    pub fn new(hop_s: f32, tau_s: f32) -> Self {
        Ema {
            coeff: (-hop_s / tau_s).exp(),
            value: 0.0,
            primed: false,
        }
    }

    pub fn update(&mut self, x: f32) -> f32 {
        if self.primed {
            self.value = x + self.coeff * (self.value - x);
        } else {
            self.value = x;
            self.primed = true;
        }
        self.value
    }
}

/// One band's filter chain: bandpass, rectify, smooth.
struct Band {
    highpass: Option<Cascade>,
    lowpass: Cascade,
    envelope: Envelope,
    agc: Agc,
}

impl Band {
    fn new(fs: f32, hop_s: f32, hp: Option<f32>, lp: f32) -> Self {
        Band {
            highpass: hp.map(|f| Cascade::highpass(fs, f)),
            lowpass: Cascade::lowpass(fs, lp),
            envelope: Envelope::new(fs, ENVELOPE_TAU_S),
            agc: Agc::new(hop_s, AGC_HALF_LIFE_S),
        }
    }

    #[inline]
    fn push(&mut self, x: f32) {
        let mut y = x;
        if let Some(hp) = &mut self.highpass {
            y = hp.process(y);
        }
        self.envelope.process(self.lowpass.process(y));
    }

    fn level(&self) -> f32 {
        self.envelope.value
    }
}

/// What one hop of audio says about the music, independent of tempo.
#[derive(Clone, Copy, Debug, Default)]
pub struct Levels {
    /// Normalised 0..=1, for ranking which lamp is hottest.
    pub low: f32,
    pub mid: f32,
    pub high: f32,
    pub energy: f32,
    /// Pre-normalisation envelopes. The tracker's silence gate and any absolute
    /// judgement need these, because the normalised values are relative by
    /// construction and never report "quiet".
    pub raw_low: f32,
    pub raw_mid: f32,
    pub raw_high: f32,
    pub raw_energy: f32,
    /// Low band sustained far below its own long-run reference. A DJ killing
    /// the bass is one of the most reliable build cues in a set, so this ships
    /// to the script as information rather than being treated as damage.
    pub bass_muted: bool,
    /// Energy slope: negative falling, 0 flat, positive rising. Roughly -1..=1.
    pub build: f32,
    /// Rectified short-horizon energy rise, 0..=1. Independent of the beat
    /// grid, so it still says something when the tracker is coasting.
    pub flux: f32,
}

pub struct Bands {
    low: Band,
    mid: Band,
    high: Band,
    broadband: Envelope,
    broadband_agc: Agc,
    bass_reference: Ema,
    bass_muted_hops: u32,
    bass_muted_hold: u32,
    build_fast: Ema,
    build_slow: Ema,
    flux_fast: Ema,
    flux_slow: Ema,
}

impl Bands {
    pub fn new(sample_rate: u32, hop: usize) -> Self {
        let fs = sample_rate as f32;
        let hop_s = hop as f32 / fs;
        Bands {
            low: Band::new(fs, hop_s, Some(LOW_HP), LOW_LP),
            mid: Band::new(fs, hop_s, Some(LOW_LP), MID_LP),
            high: Band::new(fs, hop_s, Some(MID_LP), HIGH_LP),
            broadband: Envelope::new(fs, ENVELOPE_TAU_S),
            broadband_agc: Agc::new(hop_s, AGC_HALF_LIFE_S),
            bass_reference: Ema::new(hop_s, BASS_REFERENCE_S),
            bass_muted_hops: 0,
            bass_muted_hold: (BASS_MUTED_HOLD_S / hop_s).ceil() as u32,
            build_fast: Ema::new(hop_s, BUILD_FAST_S),
            build_slow: Ema::new(hop_s, BUILD_SLOW_S),
            flux_fast: Ema::new(hop_s, FLUX_FAST_S),
            flux_slow: Ema::new(hop_s, FLUX_SLOW_S),
        }
    }

    /// Feed one sample. Called at audio rate, so it stays branch-light.
    #[inline]
    pub fn push(&mut self, x: f32) {
        self.low.push(x);
        self.mid.push(x);
        self.high.push(x);
        self.broadband.process(x);
    }

    /// Sample the envelopes at a hop boundary and update the derived flags.
    pub fn sample(&mut self) -> Levels {
        let (raw_low, raw_mid, raw_high) = (self.low.level(), self.mid.level(), self.high.level());
        let raw_energy = self.broadband.value;

        // Broadband first: it sets the floor the per-band references cannot
        // sink below.
        let energy = self.broadband_agc.normalise(raw_energy, SILENCE_FLOOR);
        let floor = self.broadband_agc.peak * BAND_FLOOR_RATIO;

        // The reference tracks the level the low band normally runs at. A kill
        // drops well below it; a quiet passage drags the reference down too,
        // which is why this is judged as a ratio rather than an absolute.
        let reference = self.bass_reference.update(raw_low);
        let muted_now = reference > SILENCE_FLOOR && raw_low < reference * BASS_MUTED_RATIO;
        self.bass_muted_hops = if muted_now {
            self.bass_muted_hops.saturating_add(1)
        } else {
            0
        };

        let flux_fast = self.flux_fast.update(raw_energy);
        let flux_slow = self.flux_slow.update(raw_energy);
        let flux = if flux_slow > SILENCE_FLOOR {
            ((flux_fast - flux_slow) / flux_slow).clamp(0.0, 1.0)
        } else {
            0.0
        };

        let fast = self.build_fast.update(raw_energy);
        let slow = self.build_slow.update(raw_energy);
        // Normalised against the slower horizon so the slope means the same
        // thing at any absolute level.
        let build = if slow > SILENCE_FLOOR {
            ((fast - slow) / slow).clamp(-1.0, 1.0)
        } else {
            0.0
        };

        Levels {
            low: self.low.agc.normalise(raw_low, floor),
            mid: self.mid.agc.normalise(raw_mid, floor),
            high: self.high.agc.normalise(raw_high, floor),
            energy,
            raw_low,
            raw_mid,
            raw_high,
            raw_energy,
            bass_muted: self.bass_muted_hops >= self.bass_muted_hold,
            build,
            flux,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: u32 = 48_000;
    const HOP: usize = 256;

    /// Feed a generated signal and return the levels at the last hop boundary.
    fn drive(seconds: f32, mut sample: impl FnMut(f32) -> f32) -> Levels {
        let mut bands = Bands::new(FS, HOP);
        let mut levels = Levels::default();
        for i in 0..(FS as f32 * seconds) as usize {
            bands.push(sample(i as f32 / FS as f32));
            if i % HOP == HOP - 1 {
                levels = bands.sample();
            }
        }
        levels
    }

    fn tone(freq: f32, seconds: f32) -> Levels {
        drive(seconds, |t| (2.0 * PI * freq * t).sin())
    }

    /// The separation the whole design leans on: a tone must light its own band
    /// and leave the others dark, even though each band normalises itself.
    #[test]
    fn each_band_claims_its_own_range() {
        let bass = tone(60.0, 3.0);
        assert!(
            bass.low > 0.9 && bass.mid < 0.2 && bass.high < 0.2,
            "60Hz should land in low only: {bass:?}"
        );

        let mid = tone(700.0, 3.0);
        assert!(
            mid.mid > 0.9 && mid.low < 0.2 && mid.high < 0.2,
            "700Hz should land in mid only: {mid:?}"
        );

        let high = tone(6_000.0, 3.0);
        assert!(
            high.high > 0.9 && high.low < 0.2 && high.mid < 0.2,
            "6kHz should land in high only: {high:?}"
        );
    }

    /// Band edges must meet: a tone just inside a band still belongs to it.
    #[test]
    fn crossovers_do_not_leave_a_hole() {
        let just_below = tone(150.0, 3.0);
        assert!(
            just_below.low > 0.5,
            "150Hz should still be low: {just_below:?}"
        );
        let just_above = tone(180.0, 3.0);
        assert!(
            just_above.mid > 0.5,
            "180Hz should already be mid: {just_above:?}"
        );
    }

    #[test]
    fn loud_midrange_does_not_hold_the_low_band_up() {
        let levels = drive(3.0, |t| 4.0 * (2.0 * PI * 700.0 * t).sin());
        assert!(
            levels.low < 0.1,
            "loud midrange leaked into the low band: {levels:?}"
        );
    }

    #[test]
    fn bass_muted_latches_only_after_the_hold() {
        let mut bands = Bands::new(FS, HOP);
        let mut levels = Levels::default();

        // Establish a reference with the kick present.
        for i in 0..FS as usize * 20 {
            let t = i as f32 / FS as f32;
            bands.push((2.0 * PI * 60.0 * t).sin());
            if i % HOP == HOP - 1 {
                levels = bands.sample();
            }
        }
        assert!(!levels.bass_muted, "bass present should not read as muted");

        // Kill it, keeping the midrange going as an EQ kill would.
        let mut flagged_at = None;
        for i in 0..FS as usize * 2 {
            let t = i as f32 / FS as f32;
            bands.push((2.0 * PI * 700.0 * t).sin());
            if i % HOP == HOP - 1 {
                levels = bands.sample();
                if levels.bass_muted && flagged_at.is_none() {
                    flagged_at = Some(i as f32 / FS as f32);
                }
            }
        }
        let at = flagged_at.expect("a sustained bass kill should flag");
        assert!(
            (0.25..0.9).contains(&at),
            "flag should trail the kill by roughly the hold, got {at}s"
        );
    }

    /// A single missing kick is not a bass kill.
    #[test]
    fn one_silent_beat_does_not_flag() {
        let mut bands = Bands::new(FS, HOP);
        let mut levels = Levels::default();
        for i in 0..FS as usize * 20 {
            let t = i as f32 / FS as f32;
            bands.push((2.0 * PI * 60.0 * t).sin());
            if i % HOP == HOP - 1 {
                levels = bands.sample();
            }
        }
        // 150ms gap, half the hold.
        let mut flagged = false;
        for i in 0..(FS as f32 * 0.15) as usize {
            bands.push(0.0);
            if i % HOP == HOP - 1 {
                levels = bands.sample();
                flagged |= levels.bass_muted;
            }
        }
        assert!(!flagged, "a 150ms gap should not latch: {levels:?}");
    }

    #[test]
    fn build_tracks_the_direction_of_energy() {
        let rising = drive(8.0, |t| (t / 8.0) * (2.0 * PI * 700.0 * t).sin());
        assert!(rising.build > 0.0, "rising energy should build: {rising:?}");

        let falling = drive(8.0, |t| (1.0 - t / 8.0) * (2.0 * PI * 700.0 * t).sin());
        assert!(
            falling.build < 0.0,
            "falling energy should not build: {falling:?}"
        );
    }
}
