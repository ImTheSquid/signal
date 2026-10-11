//! Which tracks were played. A track counts once its player has had it playing
//! and on air long enough to have been heard rather than cued, and once per load:
//! pausing and resuming it is still the one play.

use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime};

use crate::link::{Status, Track};

/// An audible run survives a gap this short — a fader cut, a brief stop — and
/// keeps the time it started.
const GAP: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Play {
    pub player: u8,
    pub track: Track,
    /// When it became audible.
    pub started_at: SystemTime,
}

#[derive(Clone, Copy)]
struct Run {
    track: Track,
    since: Instant,
    wall: SystemTime,
    heard_at: Instant,
}

#[derive(Default)]
struct Deck {
    run: Option<Run>,
    /// Already reported, and still loaded.
    counted: Option<Track>,
}

pub struct Detector {
    min: Duration,
    decks: BTreeMap<u8, Deck>,
}

impl Detector {
    pub fn new(min: Duration) -> Self {
        Detector {
            min,
            decks: BTreeMap::new(),
        }
    }

    /// One round of the players' statuses. Returns the plays that just counted.
    pub fn update(
        &mut self,
        statuses: impl IntoIterator<Item = Status>,
        now: Instant,
        wall: SystemTime,
    ) -> Vec<Play> {
        let mut plays = Vec::new();
        for s in statuses {
            let deck = self.decks.entry(s.device).or_default();
            if deck.counted != s.track {
                deck.counted = None;
            }
            let Some(track) = s.track.filter(|_| s.playing && s.on_air) else {
                continue;
            };
            let run = match deck.run {
                Some(r) if r.track == track && now.saturating_duration_since(r.heard_at) <= GAP => {
                    Run { heard_at: now, ..r }
                }
                _ => Run {
                    track,
                    since: now,
                    wall,
                    heard_at: now,
                },
            };
            deck.run = Some(run);
            if deck.counted.is_none() && now.saturating_duration_since(run.since) >= self.min {
                deck.counted = Some(track);
                plays.push(Play {
                    player: s.device,
                    track,
                    started_at: run.wall,
                });
            }
        }
        plays
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: Duration = Duration::from_secs(30);

    fn track(id: u32) -> Track {
        Track {
            source: 1,
            slot: 3,
            kind: 1,
            id,
        }
    }

    fn status(device: u8, id: Option<u32>, audible: bool) -> Status {
        Status {
            device,
            track: id.map(track),
            playing: audible,
            on_air: audible,
            master: false,
        }
    }

    /// Feeds `statuses` every quarter second from `from` to `to`, in seconds.
    fn feed(d: &mut Detector, t0: Instant, from: f64, to: f64, statuses: &[Status]) -> Vec<(u8, u32, f64)> {
        let wall0 = SystemTime::UNIX_EPOCH;
        let mut plays = Vec::new();
        let mut s = from;
        while s <= to {
            let dt = Duration::from_secs_f64(s);
            for p in d.update(statuses.iter().copied(), t0 + dt, wall0 + dt) {
                let started = p.started_at.duration_since(wall0).unwrap().as_secs_f64();
                plays.push((p.player, p.track.id, started));
            }
            s += 0.25;
        }
        plays
    }

    #[test]
    fn a_track_counts_once_heard_long_enough_from_when_it_started() {
        let mut d = Detector::new(MIN);
        let t = Instant::now();
        assert!(feed(&mut d, t, 0.0, 10.0, &[status(1, Some(5), false)]).is_empty());
        let plays = feed(&mut d, t, 10.0, 90.0, &[status(1, Some(5), true)]);
        assert_eq!(plays, [(1, 5, 10.0)]);
    }

    #[test]
    fn a_cue_in_the_headphones_never_counts() {
        let mut d = Detector::new(MIN);
        let t = Instant::now();
        let cued = Status {
            on_air: false,
            ..status(2, Some(5), true)
        };
        assert!(feed(&mut d, t, 0.0, 120.0, &[cued]).is_empty());
        // Nor a quick preview on air.
        assert!(feed(&mut d, t, 120.0, 140.0, &[status(2, Some(5), true)]).is_empty());
    }

    #[test]
    fn a_fader_cut_keeps_the_start_and_a_pause_does_not_count_twice() {
        let mut d = Detector::new(MIN);
        let t = Instant::now();
        let on = [status(1, Some(5), true)];
        let off = [status(1, Some(5), false)];
        assert!(feed(&mut d, t, 0.0, 20.0, &on).is_empty());
        assert!(feed(&mut d, t, 20.25, 22.0, &off).is_empty());
        assert_eq!(feed(&mut d, t, 22.25, 40.0, &on), [(1, 5, 0.0)]);

        // A long pause and a resume is the same play.
        assert!(feed(&mut d, t, 40.25, 100.0, &off).is_empty());
        assert!(feed(&mut d, t, 100.25, 200.0, &on).is_empty());
    }

    #[test]
    fn both_decks_in_a_long_blend_count() {
        let mut d = Detector::new(MIN);
        let t = Instant::now();
        let mut plays = feed(&mut d, t, 0.0, 60.0, &[status(1, Some(5), true)]);
        plays.extend(feed(
            &mut d,
            t,
            60.25,
            120.0,
            &[status(1, Some(5), true), status(2, Some(6), true)],
        ));
        assert_eq!(plays, [(1, 5, 0.0), (2, 6, 60.25)]);
    }

    #[test]
    fn the_same_track_counts_again_after_another() {
        let mut d = Detector::new(MIN);
        let t = Instant::now();
        let mut plays = feed(&mut d, t, 0.0, 40.0, &[status(1, Some(5), true)]);
        plays.extend(feed(&mut d, t, 40.25, 80.0, &[status(1, Some(6), true)]));
        plays.extend(feed(&mut d, t, 80.25, 120.0, &[status(1, Some(5), true)]));
        assert_eq!(plays, [(1, 5, 0.0), (1, 6, 40.25), (1, 5, 80.25)]);
    }
}
