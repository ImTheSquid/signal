//! Which metrical level aubio landed on.
//!
//! aubio reports *a* periodicity, not necessarily the beat. Measured over 25
//! tracks from the library, 7 of the 12 wrong tempi were exact rational ratios of
//! the truth — five at 2/3 and two at 1/2 — and every one lands within ~1% of
//! rekordbox after multiplying back. So the period is right and the *level* is
//! wrong, which is a question with a cheap answer.
//!
//! The test is a comb over the onset novelty already computed for the bands:
//! score a candidate period by how much stronger its grid points are than the
//! midpoints between them.
//!
//! - The true beat: grid points on beats, midpoints on offbeats. Large gap.
//! - Half the tempo: grid points on beats and midpoints on the *other* beats, so
//!   both are strong and the gap collapses.
//! - Twice the tempo: grid points alternate beat and gap, so both terms dilute.
//!
//! Penalising both directions is the whole point — a plain "how much energy is on
//! the grid" score rises as the period grows, and that is what an octave error
//! looks like from the inside.
//!
//! No tempo prior. A prior centred on dance music would fix these tracks and lie
//! about anything else, and the library spans 77 to 220 BPM.

use std::collections::VecDeque;

use crate::tempo::HOP;

/// Novelty kept for the comb. Long enough for several bars at the slowest tempo
/// worth tracking, short enough to follow a transition.
const WINDOW_S: f32 = 8.0;

/// How often the level is re-decided. It changes on track boundaries, not within
/// a bar, and the comb is the only real work in this file.
const RECHECK_S: f32 = 0.5;

/// Multipliers on aubio's BPM. **Speed-ups only**, and that is the load-bearing
/// decision in this file.
///
/// Two reasons. Measured over 25 library tracks, every one of aubio's wrong tempi
/// was an *under*-estimate — five at 2/3, two at 1/2, none too fast — so a
/// slow-down candidate can only do harm here. And the contrast score below is
/// biased toward slow: at half tempo the grid lands on the accented beats and the
/// midpoints on the weak ones, which real music separates strongly, so `on - mid`
/// is *larger* at half tempo than at the true level. With slow-downs allowed that
/// bias halved fourteen of the 25 tracks and took correct tempi from 13 to 7.
/// 4/3 is deliberately absent. It rescued nothing and cost three tracks — two
/// correct tempi pushed up by a third, and one near-miss pushed further — which
/// is what a candidate that matches no real metrical relation does. aubio confuses
/// the dotted-quarter (3/2) and the half (2), and those are the ones worth
/// offering it.
const RATIOS: [f32; 3] = [1.0, 1.5, 2.0];

/// Below this the comb has no opinion and aubio's level stands.
const MIN_MARGIN: f32 = 1.05;

pub struct Metrical {
    novelty: VecDeque<f32>,
    capacity: usize,
    hops_per_recheck: u32,
    since_recheck: u32,
    /// The multiplier currently applied to aubio's estimate.
    ratio: f32,
    hop_ms: f32,
}

impl Metrical {
    pub fn new(sample_rate: u32) -> Self {
        let hop_ms = HOP as f32 * 1000.0 / sample_rate as f32;
        let hops = |s: f32| (s * 1000.0 / hop_ms).round() as usize;
        Metrical {
            novelty: VecDeque::with_capacity(hops(WINDOW_S)),
            capacity: hops(WINDOW_S),
            hops_per_recheck: hops(RECHECK_S) as u32,
            since_recheck: u32::MAX,
            ratio: 1.0,
            hop_ms,
        }
    }

    /// One hop of onset novelty, and the BPM aubio currently believes. Returns
    /// the BPM at the level the novelty actually supports.
    ///
    /// `audible` is the same energy gate the confidence uses, and it is not
    /// optional: a comb run over an inaudible passage is exactly the failure that
    /// gate exists for. A quiet pad with a 1Hz wobble scores beautifully at 64
    /// BPM, and without this it halved a 128 BPM grid during a breakdown.
    pub fn resolve(&mut self, flux: f32, bpm: f32, audible: bool) -> f32 {
        if !audible {
            return bpm * self.ratio;
        }
        if self.novelty.len() == self.capacity {
            self.novelty.pop_front();
        }
        self.novelty.push_back(flux);

        self.since_recheck = self.since_recheck.saturating_add(1);
        if self.since_recheck >= self.hops_per_recheck {
            self.since_recheck = 0;
            self.ratio = self.best_ratio(bpm);
        }
        bpm * self.ratio
    }

    /// The applied multiplier, for the probe.
    pub fn ratio(&self) -> f32 {
        self.ratio
    }

    fn best_ratio(&self, bpm: f32) -> f32 {
        // A quarter of the window is not enough grid points to mean anything.
        if bpm <= 0.0 || self.novelty.len() < self.capacity / 2 {
            return self.ratio;
        }

        let mut best = (1.0f32, f32::MIN);
        let mut unity = f32::MIN;
        for r in RATIOS {
            let candidate = bpm * r;
            if !(crate::tempo::MIN_BPM..=crate::tempo::MAX_BPM).contains(&candidate) {
                continue;
            }
            let period_hops = 60_000.0 / candidate / self.hop_ms;
            let s = self.contrast(period_hops);
            if r == 1.0 {
                unity = s;
            }
            if s > best.1 {
                best = (r, s);
            }
        }

        // Only move off aubio's own level for a clear win. Every one of these
        // tracks is a track the current code already gets right, and swapping a
        // correct tempo for a marginally better comb score is a bad trade.
        match unity > f32::MIN && best.1 > unity * MIN_MARGIN {
            true => best.0,
            false => 1.0,
        }
    }

    /// Mean novelty on the grid minus mean novelty at the midpoints, at whichever
    /// phase maximises it.
    fn contrast(&self, period_hops: f32) -> f32 {
        let n = self.novelty.len() as f32;
        if period_hops < 2.0 || period_hops * 3.0 > n {
            return f32::MIN;
        }
        let at = |i: f32| -> f32 {
            let idx = i.round() as usize;
            self.novelty.get(idx).copied().unwrap_or(0.0)
        };

        let mut best = f32::MIN;
        // One phase per hop across a single period; finer would be below the
        // resolution the novelty itself has.
        let steps = period_hops.round() as usize;
        for p in 0..steps {
            let phase = p as f32;
            let (mut on, mut mid, mut count) = (0.0f32, 0.0f32, 0.0f32);
            let mut k = 0.0f32;
            loop {
                let t = phase + k * period_hops;
                if t + period_hops / 2.0 >= n {
                    break;
                }
                on += at(t);
                mid += at(t + period_hops / 2.0);
                count += 1.0;
                k += 1.0;
            }
            if count < 3.0 {
                continue;
            }
            let score = (on - mid) / count;
            if score > best {
                best = score;
            }
        }
        best
    }
}
