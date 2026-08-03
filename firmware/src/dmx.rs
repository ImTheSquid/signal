//! Lamp states pushed over LAN UDP by the rekordbox DMX bridge.
//!
//! Unauthenticated by design: the socket only exists while a script is calling
//! `dmx_recv()`, and that script only runs while its submitter holds the lock.

use std::io;
use std::net::UdpSocket;
use std::time::{Duration, Instant};

/// Filters accidents (stray broadcasts, other protocols), not attackers.
const MAGIC: [u8; 2] = [0x54, 0x4C]; // "TL"
const VERSION: u8 = 3;

/// Wire layout:
///
/// ```text
/// 0  2  magic "TL"
/// 2  1  version
/// 3  1  header_len
/// 4  4  seq, u32 BE
/// 8  2  base channel, u16 BE
/// 10 .. channel values, one byte each, to the end of the datagram
/// ```
///
/// `header_len` is what makes this extensible without reflashing the light,
/// which needs physical access: a later bridge can append header fields and
/// this firmware skips whatever it does not recognise. The channel count is
/// implied by the datagram length rather than carried separately, so the two
/// can never disagree.
const HEADER_MIN: usize = 10;
/// Bounds the datagram and the Rhai array this turns into. Three covers the
/// fixture; the headroom is for a script reacting to the rest of the show.
const MAX_CHANNELS: usize = 64;

/// Silence longer than this means the next packet belongs to a new sender
/// session, so its sequence number is accepted without comparison. Without
/// this a bridge reboot (seq back to 0) would be rejected for 2^31 frames.
const SEQ_RESET_GAP: Duration = Duration::from_millis(2000);

/// One received frame. Raw channel values, not a lamp decision — the script
/// owns thresholding and the channel-to-lamp mapping.
pub struct Frame {
    pub seq: u32,
    /// DMX channel that `channels[0]` corresponds to. Carried on the wire so
    /// the bridge can be repointed without the script guessing, and without
    /// reflashing this device.
    pub base: u16,
    pub channels: Vec<u8>,
}

fn parse(buf: &[u8]) -> Option<Frame> {
    if buf.len() < HEADER_MIN || buf[0..2] != MAGIC || buf[2] != VERSION {
        return None;
    }
    let header_len = buf[3] as usize;
    // Accept longer headers from a newer bridge; skip what we don't know.
    if header_len < HEADER_MIN || buf.len() < header_len {
        return None;
    }
    let channels = &buf[header_len..];
    if channels.len() > MAX_CHANNELS {
        return None;
    }
    Some(Frame {
        seq: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
        base: u16::from_be_bytes([buf[8], buf[9]]),
        channels: channels.to_vec(),
    })
}

pub struct DmxSocket {
    socket: UdpSocket,
    last_seq: Option<u32>,
    last_rx: Option<Instant>,
}

impl DmxSocket {
    pub fn bind(port: u16) -> io::Result<Self> {
        let socket = UdpSocket::bind(("0.0.0.0", port))?;
        socket.set_nonblocking(true)?;
        Ok(DmxSocket {
            socket,
            last_seq: None,
            last_rx: None,
        })
    }

    /// The newest frame to arrive within `timeout`, or None on timeout or stop.
    ///
    /// Waits in `chunk`-sized steps so `should_stop` (abort, deadline) is
    /// honored with the same latency bound as `sleep`. Each pass drains the
    /// socket completely and keeps only the last valid frame, so a burst can
    /// never queue up and replay stale states.
    pub fn recv(
        &mut self,
        timeout: Duration,
        chunk: Duration,
        should_stop: &dyn Fn() -> bool,
    ) -> Option<Frame> {
        let until = Instant::now() + timeout;
        // Room for a longer header from a future bridge.
        let mut buf = [0u8; 128 + MAX_CHANNELS];
        loop {
            let mut newest = None;
            loop {
                match self.socket.recv_from(&mut buf) {
                    Ok((n, _)) => {
                        if let Some(frame) = parse(&buf[..n]) {
                            if self.accept_seq(frame.seq) {
                                newest = Some(frame);
                            }
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        log::warn!("dmx recv failed: {e}");
                        break;
                    }
                }
            }
            if newest.is_some() {
                return newest;
            }
            if should_stop() {
                return None;
            }
            let now = Instant::now();
            if now >= until {
                return None;
            }
            std::thread::sleep(chunk.min(until - now));
        }
    }

    /// Reject replays and reordered frames, so a late packet cannot undo a
    /// newer one.
    fn accept_seq(&mut self, seq: u32) -> bool {
        let now = Instant::now();
        let new_session = self
            .last_rx
            .is_none_or(|at| now.duration_since(at) > SEQ_RESET_GAP);
        let accept = new_session
            || self
                .last_seq
                .is_none_or(|last| seq.wrapping_sub(last) as i32 > 0);
        if accept {
            self.last_seq = Some(seq);
            self.last_rx = Some(now);
        }
        accept
    }
}
