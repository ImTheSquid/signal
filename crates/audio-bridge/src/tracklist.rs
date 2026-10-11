//! Reports played tracks to greenlight-stream, which keeps the tracklist.
//!
//! Detection polls the link listener on a thread of its own, and titles and
//! posting happen on another, so a slow player or a dead uplink never delays
//! it. Each play is also logged: the copy that outlasts the in-memory queue.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};

use crate::dbserver::{self, Meta};
use crate::link::{Player, Track, Watch};
use crate::played::{Detector, Play};

const POLL: Duration = Duration::from_millis(250);
const HEARTBEAT_EVERY: Duration = Duration::from_secs(30);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// Hours of a set; past this the oldest go, and only the log has them.
const QUEUE_MAX: usize = 500;

pub struct Config {
    /// greenlight-stream's `/api/tracks`.
    pub url: String,
    pub token: String,
    pub min_play: Duration,
}

pub fn start(config: Config, watch: Watch) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let w = watch.clone();
    let min_play = config.min_play;
    std::thread::Builder::new()
        .name("tracklist".into())
        .spawn(move || detect(&w, min_play, &tx))
        .context("starting tracklist detection")?;
    std::thread::Builder::new()
        .name("tracklist-send".into())
        .spawn(move || Poster::new(config, watch).run(&rx))
        .context("starting the tracklist sender")?;
    Ok(())
}

fn detect(watch: &Watch, min_play: Duration, tx: &Sender<Play>) {
    let mut detector = Detector::new(min_play);
    loop {
        std::thread::sleep(POLL);
        let now = Instant::now();
        let statuses = watch.players(now).into_iter().filter_map(|p| p.status);
        for play in detector.update(statuses, now, SystemTime::now()) {
            if tx.send(play).is_err() {
                return;
            }
        }
    }
}

enum Sent {
    Done,
    /// Never going to be accepted.
    Refused(u16),
    Retry(String),
}

struct Poster {
    config: Config,
    watch: Watch,
    agent: ureq::Agent,
    queue: VecDeque<String>,
    titles: HashMap<Track, Meta>,
    retry_at: Instant,
    backoff: Duration,
    heartbeat_at: Instant,
    /// The last failure, said once rather than every retry.
    said: Option<String>,
}

impl Poster {
    fn new(config: Config, watch: Watch) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(HTTP_TIMEOUT))
            .http_status_as_error(false)
            .build()
            .into();
        let now = Instant::now();
        Poster {
            config,
            watch,
            agent,
            queue: VecDeque::new(),
            titles: HashMap::new(),
            retry_at: now,
            backoff: BACKOFF_MIN,
            heartbeat_at: now,
            said: None,
        }
    }

    fn run(mut self, rx: &Receiver<Play>) {
        loop {
            match rx.recv_timeout(BACKOFF_MIN) {
                Ok(play) => self.played(play),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            let now = Instant::now();
            if now >= self.heartbeat_at {
                self.heartbeat_at = now + HEARTBEAT_EVERY;
                // Superseded by the next, so never queued.
                let body = heartbeat_json(&self.watch.players(now), self.watch.mixer(now));
                match self.post(&body) {
                    Sent::Done => self.recovered(),
                    Sent::Refused(code) => self.fail(format!("greenlight refused a heartbeat ({code})")),
                    Sent::Retry(why) => self.fail(why),
                }
            }
            self.flush(now);
        }
    }

    fn played(&mut self, play: Play) {
        let meta = match self.titles.get(&play.track) {
            Some(m) => Some(m.clone()),
            None => self.look_up(&play),
        };
        if let Some(m) = &meta {
            self.titles.insert(play.track, m.clone());
        }
        self.watch.note(format!("tracklist: {}", describe(&play, meta.as_ref())));
        if self.queue.len() >= QUEUE_MAX {
            self.queue.pop_front();
            self.watch.note("tracklist: queue full; dropped the oldest play".into());
        }
        self.queue.push_back(play_json(&play, meta.as_ref()));
    }

    fn look_up(&self, play: &Play) -> Option<Meta> {
        let players = self.watch.players(Instant::now());
        let found = title(play, &players);
        if let Err(e) = &found {
            self.watch.note(format!("tracklist: no title for player {}: {e:#}", play.player));
        }
        found.ok().flatten()
    }

    fn flush(&mut self, now: Instant) {
        while now >= self.retry_at {
            let Some(body) = self.queue.front() else {
                return;
            };
            match self.post(body) {
                Sent::Done => {
                    self.queue.pop_front();
                    self.backoff = BACKOFF_MIN;
                    self.recovered();
                }
                Sent::Refused(code) => {
                    self.queue.pop_front();
                    self.watch
                        .note(format!("tracklist: greenlight refused a play ({code}); dropped"));
                }
                Sent::Retry(why) => {
                    self.fail(why);
                    self.retry_at = now + self.backoff;
                    self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
                }
            }
        }
    }

    fn recovered(&mut self) {
        if self.said.take().is_some() {
            self.watch.note("tracklist: reaching greenlight again".into());
        }
    }

    fn fail(&mut self, why: String) {
        let msg = format!("tracklist: {why}; retrying");
        if self.said.as_ref() != Some(&msg) {
            self.watch.note(msg.clone());
            self.said = Some(msg);
        }
    }

    fn post(&self, body: &str) -> Sent {
        let sent = self
            .agent
            .post(&self.config.url)
            .header("Authorization", format!("Bearer {}", self.config.token))
            .content_type("application/json")
            .send(body);
        match sent.map(|r| r.status().as_u16()) {
            Ok(200..=299) => Sent::Done,
            // Malformed: sending it again changes nothing.
            Ok(400) => Sent::Refused(400),
            Ok(code) => Sent::Retry(format!("greenlight answered {code}")),
            Err(e) => Sent::Retry(format!("posting to greenlight: {e}")),
        }
    }
}

/// Asked of the player whose media holds the track.
fn title(play: &Play, players: &[Player]) -> Result<Option<Meta>> {
    let source = players
        .iter()
        .find(|p| p.number == play.track.source)
        .with_context(|| format!("source player {} not heard", play.track.source))?;
    let asking = dbserver::asking_number(source, players)
        .with_context(|| format!("no player number may ask player {}", source.number))?;
    dbserver::lookup(source.ip, play.track, asking)
}

fn describe(play: &Play, meta: Option<&Meta>) -> String {
    let what = match meta {
        Some(m) => format!("{} — {}", m.artist, m.title),
        None => format!("track {}", play.track.id),
    };
    format!(
        "player {} played {what} (player {} slot {})",
        play.player, play.track.source, play.track.slot
    )
}

fn epoch_ms(t: SystemTime) -> u128 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

fn play_json(play: &Play, meta: Option<&Meta>) -> String {
    let started = epoch_ms(play.started_at);
    let text = |f: fn(&Meta) -> &str| match meta.map(f).filter(|s| !s.is_empty()) {
        Some(s) => json_str(s),
        None => "null".into(),
    };
    let t = play.track;
    format!(
        concat!(
            r#"{{"type":"play","id":"{}-{}-{}","startedAt":{},"player":{},"#,
            r#""source":{},"slot":{},"kind":{},"rekordboxId":{},"title":{},"artist":{}}}"#
        ),
        play.player,
        started,
        t.id,
        started,
        play.player,
        t.source,
        t.slot,
        t.kind,
        t.id,
        text(|m| &m.title),
        text(|m| &m.artist),
    )
}

fn heartbeat_json(players: &[Player], mixer: bool) -> String {
    let players: Vec<String> = players
        .iter()
        .filter_map(|p| {
            let s = p.status?;
            Some(format!(
                r#"{{"number":{},"name":{},"playing":{},"onAir":{}}}"#,
                p.number,
                json_str(&p.name),
                s.playing,
                s.on_air
            ))
        })
        .collect();
    format!(
        r#"{{"type":"heartbeat","players":[{}],"mixer":{mixer}}}"#,
        players.join(",")
    )
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn play() -> Play {
        Play {
            player: 3,
            track: Track {
                source: 1,
                slot: 3,
                kind: 1,
                id: 812,
            },
            started_at: SystemTime::UNIX_EPOCH + Duration::from_millis(1_760_000_000_123),
        }
    }

    #[test]
    fn a_play_is_the_agreed_shape() {
        let meta = Meta {
            title: "Say \"When\"".into(),
            artist: String::new(),
        };
        assert_eq!(
            play_json(&play(), Some(&meta)),
            concat!(
                r#"{"type":"play","id":"3-1760000000123-812","startedAt":1760000000123,"player":3,"#,
                r#""source":1,"slot":3,"kind":1,"rekordboxId":812,"title":"Say \"When\"","artist":null}"#
            )
        );
        assert!(play_json(&play(), None).ends_with(r#""title":null,"artist":null}"#));
    }

    #[test]
    fn control_characters_are_escaped() {
        assert_eq!(json_str("a\\b\n—"), r#""a\\b\u000a—""#);
    }
}
