//! A one-line live display, so "working" and "silent" stop looking alike.
//!
//! Everything here goes to stderr. stdout carries the `--probe` TSV, and a
//! redrawing status line would corrupt it.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use crate::bands::Levels;
use crate::tempo::Grid;

/// Redraw rate. Fast enough to read as a meter, slow enough not to flood a
/// terminal or a pipe.
const REDRAW: Duration = Duration::from_millis(66);

/// Without a terminal there is nothing to redraw over, so print a line
/// occasionally instead and keep the log readable.
const LOG_EVERY: Duration = Duration::from_secs(10);

/// How long a detected beat stays lit on the display.
const BEAT_FLASH: Duration = Duration::from_millis(120);

/// Peak hold, long enough to catch a transient by eye.
const PEAK_HOLD: Duration = Duration::from_millis(1500);

/// Fallback of the bar, in dB per second.
///
/// Instantaneous level is unreadable on percussive material: a kick is 50ms of
/// sound in a 469ms beat, so sampling the envelope at any given moment almost
/// always finds silence and the bar sits empty through a loud track. Real
/// meters solve this with fast attack and slow release; so does this one.
const RELEASE_DB_PER_S: f32 = 20.0;

/// Silence tolerated before the display starts suggesting why.
const HINT_AFTER: Duration = Duration::from_secs(3);

const METER_WIDTH: usize = 24;
const FLOOR_DB: f32 = -60.0;

const BLOCKS: [char; 9] = ['·', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

pub struct Meter {
    tty: bool,
    last_draw: Instant,
    started: Instant,
    /// Ballistic bar level, in dB.
    display_db: f32,
    /// Same treatment for the per-band indicators, in linear 0..=1.
    bands: [f32; 3],
    peak: f32,
    peak_at: Instant,
    beat_at: Option<Instant>,
    audio_since: Option<Instant>,
    dirty: bool,
}

impl Meter {
    pub fn new() -> Self {
        let now = Instant::now();
        Meter {
            tty: std::io::stderr().is_terminal(),
            last_draw: now - REDRAW,
            started: now,
            display_db: f32::NEG_INFINITY,
            bands: [0.0; 3],
            peak: 0.0,
            peak_at: now,
            beat_at: None,
            audio_since: None,
            dirty: false,
        }
    }

    /// Print a message without leaving it tangled in the status line.
    pub fn note(&mut self, msg: &str) {
        let mut err = std::io::stderr().lock();
        if self.dirty {
            let _ = write!(err, "\r\x1b[2K");
            self.dirty = false;
        }
        let _ = writeln!(err, "{msg}");
        let _ = err.flush();
    }

    pub fn update(&mut self, levels: &Levels, grid: &Grid, beat: bool, present: bool) {
        let now = Instant::now();
        if beat {
            self.beat_at = Some(now);
        }

        // Attack instantly, release gradually. Sampled per hop rather than per
        // redraw so a transient between two frames is not missed entirely.
        let instant_db = to_db(levels.raw_energy);
        let hop_s = crate::tempo::HOP as f32 / 48_000.0;
        self.display_db = if instant_db > self.display_db {
            instant_db
        } else {
            self.display_db - RELEASE_DB_PER_S * hop_s
        }
        .max(FLOOR_DB);

        let release = (-hop_s / 0.25).exp();
        for (display, &level) in self
            .bands
            .iter_mut()
            .zip([levels.low, levels.mid, levels.high].iter())
        {
            *display = level.max(*display * release);
        }
        if levels.raw_energy > self.peak || now.duration_since(self.peak_at) > PEAK_HOLD {
            self.peak = levels.raw_energy;
            self.peak_at = now;
        }
        match present {
            true => self.audio_since = Some(now),
            false => {}
        }

        let due = if self.tty { REDRAW } else { LOG_EVERY };
        if now.duration_since(self.last_draw) < due {
            return;
        }
        self.last_draw = now;

        let line = self.render(grid, present, now);
        let mut err = std::io::stderr().lock();
        if self.tty {
            let _ = write!(err, "\r\x1b[2K{line}");
            self.dirty = true;
        } else {
            let _ = writeln!(err, "{line}");
        }
        let _ = err.flush();
    }

    /// Reads only the ballistic state, never the raw levels: the instant a hop
    /// happens to be drawn on is almost never representative.
    fn render(&self, grid: &Grid, present: bool, now: Instant) -> String {
        let db = self.display_db;
        let peak_db = to_db(self.peak);

        let bar = if self.tty {
            format!(
                "{}{}\x1b[0m",
                zone_colour(db),
                bar(db_fraction(db), METER_WIDTH)
            )
        } else {
            bar(db_fraction(db), METER_WIDTH)
        };

        let tempo = match grid.period_ms {
            Some(p) => format!("{:6.2} BPM", 60_000.0 / p),
            None => "    -- BPM".to_string(),
        };

        let flash = self
            .beat_at
            .is_some_and(|at| now.duration_since(at) < BEAT_FLASH);
        let beat = match (flash, self.tty) {
            (true, true) => "\x1b[1;33m●\x1b[0m",
            (true, false) => "*",
            (false, _) => " ",
        };

        format!(
            "▐{bar}▐ {:>6} pk {:>6}  L{} M{} H{}  {tempo} c{:.2} {beat} {}",
            fmt_db(db),
            fmt_db(peak_db),
            BLOCKS[level_index(self.bands[0])],
            BLOCKS[level_index(self.bands[1])],
            BLOCKS[level_index(self.bands[2])],
            grid.confidence,
            self.state(grid, present, now),
        )
    }

    fn state(&self, grid: &Grid, present: bool, now: Instant) -> String {
        if !present {
            let quiet_for = self
                .audio_since
                .map(|at| now.duration_since(at))
                .unwrap_or_else(|| now.duration_since(self.started));
            // The failure this exists to catch: BlackHole selected, nothing
            // routed into it. Silent capture and broken capture look identical
            // otherwise, and the fix is never in this program.
            return if quiet_for > HINT_AFTER {
                "no audio — is anything routed into BlackHole?".into()
            } else {
                "no audio".into()
            };
        }
        if grid.tracking {
            return "tracking".into();
        }
        if grid.coasting {
            return "coasting".into();
        }
        "listening".into()
    }

    /// Leave the cursor somewhere sane on the way out.
    pub fn finish(&mut self) {
        if self.dirty {
            let mut err = std::io::stderr().lock();
            let _ = writeln!(err);
            let _ = err.flush();
            self.dirty = false;
        }
    }
}

fn to_db(amplitude: f32) -> f32 {
    if amplitude <= 0.0 {
        f32::NEG_INFINITY
    } else {
        20.0 * amplitude.log10()
    }
}

/// At or under the meter's own floor there is nothing to report a number for,
/// so say so rather than printing a precise-looking -60.0.
fn fmt_db(db: f32) -> String {
    if db.is_finite() && db > FLOOR_DB {
        format!("{db:.1}dB")
    } else {
        "-inf".to_string()
    }
}

fn db_fraction(db: f32) -> f32 {
    if !db.is_finite() {
        return 0.0;
    }
    ((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0)
}

fn zone_colour(db: f32) -> &'static str {
    match db {
        d if d >= -3.0 => "\x1b[31m",  // clipping territory
        d if d >= -12.0 => "\x1b[33m", // hot
        _ => "\x1b[32m",
    }
}

fn bar(fraction: f32, width: usize) -> String {
    let filled = (fraction * width as f32).round() as usize;
    (0..width)
        .map(|i| if i < filled { '█' } else { '░' })
        .collect()
}

fn level_index(level: f32) -> usize {
    ((level.clamp(0.0, 1.0) * 8.0).round() as usize).min(8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_reads_as_minus_infinity_not_zero() {
        assert_eq!(fmt_db(to_db(0.0)), "-inf");
        assert_eq!(db_fraction(to_db(0.0)), 0.0);
        // Under the floor there is no number worth printing.
        assert_eq!(fmt_db(FLOOR_DB - 1.0), "-inf");
    }

    #[test]
    fn full_scale_fills_the_bar() {
        assert_eq!(db_fraction(to_db(1.0)), 1.0);
        assert!(bar(1.0, 4).chars().all(|c| c == '█'));
        assert!(bar(0.0, 4).chars().all(|c| c == '░'));
    }

    /// The meter has to distinguish quiet-but-present from absent, since that
    /// is the whole reason it exists.
    #[test]
    fn a_quiet_signal_still_registers() {
        let quiet = to_db(0.003); // about -50dB
        assert!(quiet.is_finite());
        assert!(
            db_fraction(quiet) > 0.0,
            "a -50dB signal must not render as empty"
        );
    }
}
