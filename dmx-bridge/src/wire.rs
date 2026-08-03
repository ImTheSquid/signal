//! The UDP datagram the traffic light listens for.
//!
//! Carries raw DMX channel values, not a lamp decision. The Rhai script on the
//! light does the thresholding and the channel-to-lamp mapping, so both can
//! change without reflashing either device.

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};

/// Filters accidents (stray broadcasts, other protocols), not attackers.
const MAGIC: [u8; 2] = [0x54, 0x4C]; // "TL"
/// v1 was thresholded booleans, v2 raw channels, v3 adds `header_len` and the
/// base channel. The light rejects unknown versions, so the bump is what stops
/// a mismatched pair from misreading each other.
const VERSION: u8 = 3;
/// magic(2) version(1) header_len(1) seq(4) base(2). The light reads
/// `header_len` and skips anything beyond it, so fields can be appended here
/// without reflashing the light — which needs physical access.
const HEADER_LEN: usize = 10;
/// Must match `firmware/src/dmx.rs`.
pub const MAX_CHANNELS: usize = 64;

pub struct Sender {
    socket: UdpSocket,
    dest: SocketAddr,
    seq: u32,
    buf: Vec<u8>,
}

impl Sender {
    pub fn new(host: &str, port: u16) -> std::io::Result<Self> {
        let dest = (host, port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| std::io::Error::other(format!("{host}:{port} resolved to nothing")))?;
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        socket.set_broadcast(true)?;
        // A frame must never stall the USB pump; drop it instead.
        socket.set_write_timeout(Some(std::time::Duration::from_millis(20)))?;
        Ok(Sender {
            socket,
            dest,
            seq: 0,
            buf: Vec::with_capacity(HEADER_LEN + MAX_CHANNELS),
        })
    }

    pub fn dest(&self) -> SocketAddr {
        self.dest
    }

    /// Send one frame. `channels` is truncated to [`MAX_CHANNELS`]; `base` is the
    /// DMX channel number `channels[0]` holds.
    pub fn send(&mut self, base: u16, channels: &[u8]) -> std::io::Result<()> {
        let channels = &channels[..channels.len().min(MAX_CHANNELS)];
        self.seq = self.seq.wrapping_add(1);

        self.buf.clear();
        self.buf.extend_from_slice(&MAGIC);
        self.buf.push(VERSION);
        self.buf.push(HEADER_LEN as u8);
        self.buf.extend_from_slice(&self.seq.to_be_bytes());
        self.buf.extend_from_slice(&base.to_be_bytes());
        self.buf.extend_from_slice(channels);

        self.socket.send_to(&self.buf, self.dest).map(|_| ())
    }
}
