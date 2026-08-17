//! Score the phrase estimate against rekordbox's own beat grid.
//!
//! `crates/grid-truth` dumps a TSV of every beat of a track with its position in
//! the bar, read out of the ANLZ analysis rekordbox has already done. This reads
//! that back and asks how often the estimator agrees.
//!
//! Two numbers, reported separately, because they rest on different things:
//!
//! - **Bar phase** is compared against `beat_in_bar`, which rekordbox states
//!   outright. Direct evidence.
//! - **Phrase phase** is compared against a grid counted from the track's first
//!   downbeat, which assumes the track *starts* on a phrase boundary. True of
//!   most dance music and not a guarantee, so a phrase disagreement may be the
//!   assumption failing rather than the estimator.
//!
//! Collapsing them into one figure would hide which had happened.

use std::fs;

use anyhow::{bail, Context, Result};

use crate::analysis::HopResult;

/// Where a lock is believed. Sits in the gap measured on the synthetic fixtures:
/// a flat click train reads 1.88 sigma, which is just the max of sixteen noisy
/// bins, and a pattern with a real phrase in it reads 12.87.
pub const LOCK_SIGMA: f32 = 4.0;

pub struct Truth {
    /// `(time_ms, bar phase 0..4, phrase phase 0..16)`, ascending by time.
    beats: Vec<(f64, u8, u8)>,
    pub label: String,
}

impl Truth {
    pub fn read(path: &str) -> Result<Self> {
        let text = fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
        let mut beats = Vec::new();
        let mut label = String::new();

        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("# title\t") {
                label = rest.to_string();
                continue;
            }
            if line.starts_with('#') || line.starts_with("time_ms") || line.is_empty() {
                continue;
            }
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 4 {
                bail!("{path}: expected 4 tab-separated fields, got {:?}", line);
            }
            let time_ms: f64 = f[0].parse().with_context(|| format!("time in {line:?}"))?;
            let beat_in_bar: u8 = f[1].parse().with_context(|| format!("bar in {line:?}"))?;
            let phrase: u8 = f[3].parse().with_context(|| format!("phase in {line:?}"))?;
            if !(1..=4).contains(&beat_in_bar) {
                bail!("{path}: beat_in_bar {beat_in_bar} outside 1..4");
            }
            beats.push((time_ms, beat_in_bar - 1, phrase));
        }

        if beats.is_empty() {
            bail!("{path} has no beats in it");
        }
        Ok(Truth { beats, label })
    }

    /// The truth beat closest in time — nearest rather than preceding, because
    /// the estimator's grid can sit slightly either side of rekordbox's.
    fn nearest(&self, t_ms: f64) -> (f64, u8, u8) {
        let i = self
            .beats
            .partition_point(|&(bt, _, _)| bt < t_ms)
            .min(self.beats.len() - 1);
        let cand = match i {
            0 => self.beats[0],
            _ => {
                let (prev, here) = (self.beats[i - 1], self.beats[i]);
                match (t_ms - prev.0).abs() <= (here.0 - t_ms).abs() {
                    true => prev,
                    false => here,
                }
            }
        };
        cand
    }
}

pub struct Score {
    truth: Truth,
    last_index: Option<u8>,
    first_lock_s: Option<f64>,
    beats: u32,
    locked: u32,
    hits_bar: u32,
    hits_phrase: u32,
    /// How often each `(estimated - truth) mod 16` offset came up.
    ///
    /// The absolute hit rates above punish a constant offset as hard as random
    /// noise, and those are different faults: the tempo grid sitting half a beat
    /// off rekordbox's shifts every comparison the same way, which is the tracker
    /// mis-placing beats rather than this mis-placing the phrase. A sharp peak
    /// here means the phrase is being found and merely mis-labelled; a flat
    /// histogram means it is not being found.
    offset_hist: [u32; 16],
    align_err_ms: Vec<f64>,
    /// Beats of evidence the clock held at the last scored beat. Distinguishes a
    /// low score from a clock that had barely started, and drops on a reset.
    evidence: u32,
}

impl Score {
    pub fn new(truth: Truth) -> Self {
        Score {
            truth,
            last_index: None,
            first_lock_s: None,
            beats: 0,
            locked: 0,
            hits_bar: 0,
            hits_phrase: 0,
            offset_hist: [0; 16],
            align_err_ms: Vec::new(),
            evidence: 0,
        }
    }

    /// One hop. Only the hops where the grid's beat rolls over are scored: the
    /// estimate is per beat, so sampling every hop would just weight each beat by
    /// how many hops it happened to span.
    pub fn observe(&mut self, t_s: f64, r: &HopResult) {
        if self.last_index == Some(r.grid.beat_index) {
            return;
        }
        let first = self.last_index.is_none();
        self.last_index = Some(r.grid.beat_index);
        if first {
            return;
        }

        self.beats += 1;
        if r.phrase.excess_sigma < LOCK_SIGMA {
            return;
        }
        self.locked += 1;
        if self.first_lock_s.is_none() {
            self.first_lock_s = Some(t_s);
        }

        let (bt, bar, phrase) = self.truth.nearest(t_s * 1000.0);
        self.align_err_ms.push(t_s * 1000.0 - bt);
        let est = r.phrase.phase_of(r.grid.beat_index);
        self.hits_bar += u32::from(r.phrase.bar_phase_of(r.grid.beat_index) == bar);
        self.hits_phrase += u32::from(est == phrase);
        self.offset_hist[((est + 16 - phrase) % 16) as usize] += 1;
        self.evidence = r.phrase.beats;
    }

    pub fn report(&mut self) {
        let pct = |n: u32, d: u32| match d {
            0 => "-".to_string(),
            _ => format!("{:.0}%", 100.0 * n as f64 / d as f64),
        };

        eprintln!("\nphrase estimate vs rekordbox — {}", self.truth.label);
        eprintln!(
            "  locked          {} of {} beats at >= {LOCK_SIGMA} sigma, \
             {} beats of evidence at the end",
            self.locked, self.beats, self.evidence
        );
        eprintln!(
            "  first lock      {}",
            self.first_lock_s
                .map(|t| format!("{t:.1}s"))
                .unwrap_or_else(|| "never".into())
        );
        eprintln!(
            "  bar phase       {}   (vs beat_in_bar, stated by rekordbox)",
            pct(self.hits_bar, self.locked)
        );
        eprintln!(
            "  phrase phase    {}   (vs 16 from the first downbeat — assumes the \
             track starts on a phrase)",
            pct(self.hits_phrase, self.locked)
        );

        // The one that separates "found the phrase and mislabelled it" from "did
        // not find the phrase". 1/16 is 6% — chance.
        let total: u32 = self.offset_hist.iter().sum();
        let best = self
            .offset_hist
            .iter()
            .enumerate()
            .max_by_key(|(_, &n)| n)
            .map(|(i, &n)| (i, n))
            .unwrap_or((0, 0));
        eprintln!(
            "  consistency     {}   at a constant offset of {} beats (chance 6%)",
            pct(best.1, total),
            best.0
        );

        if self.align_err_ms.is_empty() {
            eprintln!("  grid alignment  no locked beats to measure");
            return;
        }
        // Independent of the phase question: if this is large the tempo grid is
        // off and the phase numbers above are measuring the wrong beats.
        self.align_err_ms.sort_by(f64::total_cmp);
        let n = self.align_err_ms.len();
        eprintln!(
            "  grid alignment  median {:+.0}ms, 10th {:+.0}ms, 90th {:+.0}ms",
            self.align_err_ms[n / 2],
            self.align_err_ms[n / 10],
            self.align_err_ms[n * 9 / 10],
        );
    }
}
