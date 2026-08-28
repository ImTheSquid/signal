//! The UDP datagram the traffic light listens for.
//!
//! It rides the light's existing UDP path — the same socket and the same parser
//! in `firmware/src/dmx.rs` — which is why adding it needed no reflash.
//!
//! The beat block rides in the **channel bytes**, not in an extended header.
//! The header is extensible — `firmware/src/dmx.rs` skips whatever it does not
//! recognise — but everything before `header_len` is discarded before the Rhai
//! script sees it: `dmx_recv` exposes only `{ok, base, seq, ch}`. Beat fields
//! in the header would therefore need a reflash, and the light needs physical
//! access. In the channel bytes they arrive as `p.ch[0..16]` with no firmware
//! change at all.
//!
//! `base` is the discriminator: the parser's channel bases are 1..512, so
//! 0xFFFE cannot collide with one and a script can tell senders apart without
//! being modified.

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::bands::Levels;
use crate::tempo::Grid;

const MAGIC: [u8; 2] = [0x54, 0x4C]; // "TL"
const VERSION: u8 = 3;
const HEADER_LEN: usize = 10;

/// Not a DMX base channel, and so unambiguously "this is a beat block".
pub const AUDIO_BASE: u16 = 0xFFFE;

/// Beat block format, versioned independently of the wire version so the block
/// can grow without touching the header the light parses.
const FMT_BEAT_V1: u8 = 0x01;

pub const BLOCK_LEN: usize = 16;

pub mod flags {
    pub const AUDIO_PRESENT: u8 = 1 << 0;
    pub const TRACKING: u8 = 1 << 1;
    pub const COASTING: u8 = 1 << 2;
    pub const BASS_MUTED: u8 = 1 << 3;
    /// Reserved: set only once a real downbeat estimator exists. Until then
    /// nothing may read `beat_index % 4 == 0` as a downbeat. Declared rather
    /// than left as a bare bit so the reservation is visible to whoever adds
    /// the estimator, and so a test can hold it down.
    #[allow(dead_code)]
    pub const BAR_VALID: u8 = 1 << 4;
    pub const CLIPPING: u8 = 1 << 5;
}

/// Baseline cadence. Matches the light's own ceiling — `min_lamp_dwell_ms` is
/// 100 — and is enough for the beat fields *because they are predictions*: a
/// 90ms-old packet still names the correct beat instant.
const TICK: Duration = Duration::from_millis(100);

/// Floor on the gap between datagrams, bounding the light's drain-and-coalesce
/// loop when events cluster.
const MIN_GAP: Duration = Duration::from_millis(20);

/// The light rejects a sequence number that has gone backwards until it has
/// seen 2000ms of silence (`SEQ_RESET_GAP` in `firmware/src/dmx.rs`). A restart
/// would otherwise wedge the lamps for exactly that long. Waiting it out costs
/// nothing: the tracker needs several seconds to lock anyway.
const STARTUP_HOLD: Duration = Duration::from_millis(2200);

/// Everything the light is told in one datagram.
#[derive(Clone, Copy, Debug)]
pub struct Beat {
    pub grid: Grid,
    pub levels: Levels,
    pub audio_present: bool,
    pub clipping: bool,
}

fn unit_to_u8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Encode the 16-byte beat block.
pub fn encode_block(beat: &Beat) -> [u8; BLOCK_LEN] {
    let mut b = [0u8; BLOCK_LEN];
    b[0] = FMT_BEAT_V1;

    let mut f = 0u8;
    if beat.audio_present {
        f |= flags::AUDIO_PRESENT;
    }
    if beat.grid.tracking {
        f |= flags::TRACKING;
    }
    if beat.grid.coasting {
        f |= flags::COASTING;
    }
    if beat.levels.bass_muted {
        f |= flags::BASS_MUTED;
    }
    if beat.clipping {
        f |= flags::CLIPPING;
    }
    b[1] = f;

    // Milliseconds rather than a phase fraction: directly usable as
    // `millis() + ms_to_next_beat`, and still meaningful when there is no
    // period to divide by.
    let next = beat
        .grid
        .ms_to_next_beat
        .map(|ms| ms.clamp(0.0, 65_534.0) as u16)
        .unwrap_or(0xFFFF);
    b[2..4].copy_from_slice(&next.to_be_bytes());

    let period = beat
        .grid
        .period_ms
        .map(|ms| ms.clamp(0.0, 65_535.0) as u16)
        .unwrap_or(0);
    b[4..6].copy_from_slice(&period.to_be_bytes());

    b[6] = beat.grid.beat_index % 16;
    b[7] = unit_to_u8(beat.grid.confidence);
    b[8] = unit_to_u8(beat.levels.energy);
    b[9] = unit_to_u8(beat.levels.low);
    b[10] = unit_to_u8(beat.levels.mid);
    b[11] = unit_to_u8(beat.levels.high);
    b[12] = unit_to_u8(beat.levels.flux);
    // Quarter-millisecond resolution reaches ~1s, past which "how long ago"
    // stops being useful. 255 means no beat has been seen.
    b[13] = beat
        .grid
        .ms_since_beat
        .map(|ms| (ms / 4.0).clamp(0.0, 254.0) as u8)
        .unwrap_or(255);
    b[14] = unit_to_u8(beat.grid.onset_strength);
    // 128 is flat, so a script can read this as a signed lean without needing
    // to know the scale.
    b[15] = ((beat.levels.build.clamp(-1.0, 1.0) * 127.0) + 128.0).round() as u8;

    b
}

pub struct Sender {
    socket: UdpSocket,
    dest: SocketAddr,
    seq: u32,
    buf: Vec<u8>,
    started: Instant,
    last_sent: Option<Instant>,
    last_block: Option<[u8; BLOCK_LEN]>,
}

impl Sender {
    pub fn new(host: &str, port: u16) -> Result<Self> {
        let dest = (host, port)
            .to_socket_addrs()
            .with_context(|| format!("resolving {host}:{port}"))?
            .next()
            .with_context(|| format!("{host}:{port} resolved to nothing"))?;
        let socket = UdpSocket::bind("0.0.0.0:0").context("binding the sending socket")?;
        socket.set_broadcast(true).ok();
        // A datagram must never stall the analysis loop; drop it instead.
        socket
            .set_write_timeout(Some(Duration::from_millis(20)))
            .context("setting write timeout")?;
        Ok(Sender {
            socket,
            dest,
            seq: 0,
            buf: Vec::with_capacity(HEADER_LEN + BLOCK_LEN),
            started: Instant::now(),
            last_sent: None,
            last_block: None,
        })
    }

    pub fn dest(&self) -> SocketAddr {
        self.dest
    }

    /// Monotonic within a run and across restarts.
    ///
    /// Derived from wall clock rather than counted from zero so a restart
    /// cannot land below the light's `last_seq` and be filtered as a replay.
    /// Ticks at the 50Hz send ceiling, so the two can never disagree.
    fn next_seq(&mut self) -> u32 {
        let from_clock = (crate::capture::unix_millis() / 20) as u32;
        self.seq = self.seq.wrapping_add(1).max(from_clock);
        self.seq
    }

    /// Send if the cadence calls for it. `event` marks something worth not
    /// waiting for — a detected beat, in practice.
    ///
    /// Returns whether a datagram went out.
    pub fn maybe_send(&mut self, beat: &Beat, event: bool, now: Instant) -> Result<bool> {
        if now.duration_since(self.started) < STARTUP_HOLD {
            return Ok(false);
        }

        let block = encode_block(beat);
        let since = self.last_sent.map(|t| now.duration_since(t));

        if since.is_some_and(|d| d < MIN_GAP) {
            return Ok(false);
        }
        // Flags and period are state, not samples: a change in either should
        // reach the script now rather than up to a tick later.
        let state_changed = self
            .last_block
            .is_none_or(|prev| prev[1] != block[1] || prev[4..6] != block[4..6]);
        let due = since.is_none_or(|d| d >= TICK);
        if !(due || event || state_changed) {
            return Ok(false);
        }

        let seq = self.next_seq();
        self.buf.clear();
        self.buf.extend_from_slice(&MAGIC);
        self.buf.push(VERSION);
        self.buf.push(HEADER_LEN as u8);
        self.buf.extend_from_slice(&seq.to_be_bytes());
        self.buf.extend_from_slice(&AUDIO_BASE.to_be_bytes());
        self.buf.extend_from_slice(&block);

        self.socket
            .send_to(&self.buf, self.dest)
            .context("sending beat datagram")?;
        self.last_sent = Some(now);
        self.last_block = Some(block);
        Ok(true)
    }
}

/// The light's view of a datagram.
#[cfg(test)]
#[derive(Debug, PartialEq)]
pub struct Parsed {
    pub seq: u32,
    pub base: u16,
    pub channels: Vec<u8>,
}

/// Mirrors `parse` in `firmware/src/dmx.rs`, so the tests check what the light
/// will actually see rather than what we meant to send.
///
#[cfg(test)]
pub fn parse(buf: &[u8]) -> Option<Parsed> {
    if buf.len() < HEADER_LEN || buf[0..2] != MAGIC || buf[2] != VERSION {
        return None;
    }
    let header_len = buf[3] as usize;
    if header_len < HEADER_LEN || buf.len() < header_len {
        return None;
    }
    let channels = &buf[header_len..];
    if channels.len() > 64 {
        return None;
    }
    Some(Parsed {
        seq: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
        base: u16::from_be_bytes([buf[8], buf[9]]),
        channels: channels.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_beat() -> Beat {
        Beat {
            grid: Grid {
                period_ms: Some(468.75),
                ms_to_next_beat: Some(123.0),
                beat_index: 21,
                confidence: 0.75,
                tracking: true,
                coasting: false,
                ms_since_beat: Some(40.0),
                onset_strength: 0.5,
                // Diagnostics only; nothing in the block encodes it.
                model_bpm: Some(128.0),
            },
            levels: Levels {
                low: 1.0,
                mid: 0.5,
                high: 0.0,
                energy: 0.8,
                flux: 0.25,
                build: 0.0,
                bass_muted: true,
                ..Levels::default()
            },
            audio_present: true,
            clipping: false,
        }
    }

    #[test]
    fn block_layout_is_what_the_script_will_read() {
        let b = encode_block(&sample_beat());
        assert_eq!(b.len(), BLOCK_LEN);
        assert_eq!(b[0], FMT_BEAT_V1);
        assert_eq!(
            b[1],
            flags::AUDIO_PRESENT | flags::TRACKING | flags::BASS_MUTED
        );
        assert_eq!(u16::from_be_bytes([b[2], b[3]]), 123);
        assert_eq!(u16::from_be_bytes([b[4], b[5]]), 468);
        assert_eq!(b[6], 5, "beat index is reported mod 16");
        assert_eq!(b[7], 191);
        assert_eq!(b[9], 255, "low is full scale");
        assert_eq!(b[11], 0, "high is dark");
        assert_eq!(b[13], 10, "40ms since the beat, in quarter-ms units");
        assert_eq!(b[15], 128, "a flat build reads as the midpoint");
    }

    #[test]
    fn bar_valid_is_never_claimed() {
        let b = encode_block(&sample_beat());
        assert_eq!(
            b[1] & flags::BAR_VALID,
            0,
            "there is no downbeat estimator, so this bit must stay clear"
        );
    }

    #[test]
    fn unknown_tempo_encodes_as_unknown_not_as_zero() {
        let mut beat = sample_beat();
        beat.grid.period_ms = None;
        beat.grid.ms_to_next_beat = None;
        beat.grid.ms_since_beat = None;
        let b = encode_block(&beat);
        assert_eq!(u16::from_be_bytes([b[2], b[3]]), 0xFFFF);
        assert_eq!(u16::from_be_bytes([b[4], b[5]]), 0);
        assert_eq!(b[13], 255);
    }

    /// The whole no-reflash argument rests on this: the light's own parser must
    /// accept the datagram and hand the block through as channel bytes.
    #[test]
    fn the_light_parser_accepts_it_and_sees_the_block() {
        let block = encode_block(&sample_beat());
        let mut datagram = Vec::new();
        datagram.extend_from_slice(&MAGIC);
        datagram.push(VERSION);
        datagram.push(HEADER_LEN as u8);
        datagram.extend_from_slice(&7u32.to_be_bytes());
        datagram.extend_from_slice(&AUDIO_BASE.to_be_bytes());
        datagram.extend_from_slice(&block);

        assert_eq!(datagram.len(), 26);
        let parsed = parse(&datagram).expect("the light must accept this");
        assert_eq!(parsed.seq, 7);
        assert_eq!(parsed.base, AUDIO_BASE);
        assert_eq!(parsed.channels, block.to_vec());
    }

    /// A DMX base is 1..512, so a script can tell the two senders apart without
    /// either of them being modified.
    #[test]
    fn the_audio_base_cannot_collide_with_a_dmx_base() {
        assert!(AUDIO_BASE > 512);
    }

    #[test]
    fn sequence_survives_a_restart() {
        let mut a = Sender::new("127.0.0.1", 49500).unwrap();
        let first = a.next_seq();
        let second = a.next_seq();
        assert!(second > first, "monotonic within a run");

        // A fresh process starts its counter at zero.
        let mut b = Sender::new("127.0.0.1", 49500).unwrap();
        let after_restart = b.next_seq();
        assert!(
            after_restart >= first,
            "a restart must not go backwards ({after_restart} < {first}), or the \
             light filters every frame as a replay for 2 seconds"
        );
    }
}
