//! The CDJs' own beat grid, read off the Pro DJ Link network.
//!
//! A playing CDJ broadcasts a packet on every beat carrying its tempo, pitch and
//! place in the bar, all from rekordbox's analysis — ground truth the audio
//! tracker can only estimate. While a deck is audibly playing, its grid replaces
//! the tracker's. The tracker keeps running underneath, so handing back when the
//! decks stop, or the cable is pulled, costs nothing.
//!
//! Detection is stateless: a deck counts for as long as its beat packets keep
//! arriving, so there is no connected/disconnected state to get stuck in.
//!
//! Layouts are from Deep Symmetry's analysis (djl-analysis.deepsymmetry.org),
//! and the keep-alive is their beat-link's template.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use socket2::{Domain, Protocol, Socket, Type};

use crate::tempo::Grid;

const MAGIC: [u8; 10] = *b"Qspt1WmJOL";

const ANNOUNCE_PORT: u16 = 50000;
const BEAT_PORT: u16 = 50001;
const STATUS_PORT: u16 = 50002;

const TYPE_ON_AIR: u8 = 0x03;
const TYPE_KEEP_ALIVE: u8 = 0x06;
const TYPE_STATUS: u8 = 0x0a;
const TYPE_BEAT: u8 = 0x28;

const BEAT_LEN: usize = 0x60;
const KEEP_ALIVE_LEN: usize = 0x36;
const STATUS_FLAGS: usize = 0x89;
const STATUS_PLAYING: u8 = 0x40;
const STATUS_MASTER: u8 = 0x20;
const STATUS_ON_AIR: u8 = 0x08;

/// Pitch is a fraction of this; 0x100000 is the track at its own tempo.
const NOMINAL_PITCH: f64 = 1_048_576.0;

/// Players are 1-6, five and six since the CDJ-3000.
const PLAYERS: usize = 6;
/// The mixer's beat packets are a backup metronome, not a deck — but they do say
/// a mixer is there to trust for on-air flags.
const MIXER: u8 = 0x21;

/// Anything outside this is a corrupt packet rather than a tempo.
const PLAUSIBLE_BPM: std::ops::RangeInclusive<f64> = 20.0..=400.0;

/// A deck is playing while its beats keep arriving: one lost packet is
/// tolerated, and the slack covers jitter on the next.
const STALE_PERIODS: f64 = 2.0;
const STALE_SLACK_MS: f64 = 150.0;

/// On-air flags are believed while the mixer's metronome is.
const MIXER_FRESH: Duration = Duration::from_secs(2);
/// Players send status several times a second to an announced device.
const STATUS_FRESH: Duration = Duration::from_secs(2);
/// Devices heard within this count toward the keep-alive's peer total.
const PEER_FRESH: Duration = Duration::from_secs(5);

const ANNOUNCE_EVERY: Duration = Duration::from_millis(1500);
/// How long a receive blocks, which bounds how late a keep-alive can go out.
const RECV_TIMEOUT: Duration = Duration::from_millis(50);
const REBIND_AFTER: Duration = Duration::from_secs(1);

const NAME: &[u8] = b"audio-bridge";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Off,
    /// Only listens. Never transmits on the DJs' network.
    Passive,
    /// Announces as this device number, which is what makes players send status —
    /// and so the tempo-master flag.
    Announce(u8),
}

/// What the followed deck says, as of a poll.
#[derive(Clone, Copy, Debug)]
pub struct Link {
    pub player: u8,
    pub grid: Grid,
    /// A beat packet arrived, or the deck was just picked up, since the last poll.
    pub beat: bool,
}

/// A loaded track, as named to the dbserver of the player it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Track {
    /// Player whose media holds it: the deck itself, or another over Link.
    pub source: u8,
    pub slot: u8,
    /// 1 rekordbox-analysed, 2 unanalysed, 5 audio CD.
    pub kind: u8,
    pub id: u32,
}

/// A player's own account of itself. Only arrives when announcing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    pub device: u8,
    pub track: Option<Track>,
    pub playing: bool,
    /// The player's belief, from the mixer, that it can be heard.
    pub on_air: bool,
    pub master: bool,
}

/// A device on the network, as of its last keep-alive.
#[derive(Clone, Debug)]
pub struct Player {
    pub number: u8,
    pub name: String,
    pub ip: Ipv4Addr,
    /// None unless fresh.
    pub status: Option<Status>,
}

#[derive(Clone, Copy, Debug, Default)]
struct Deck {
    /// Arrival of the latest beat packet. A player sends it on the beat, so this
    /// is the beat instant to within the switch's latency.
    beat_at: Option<Instant>,
    period_ms: f64,
    /// Beats counted, aligned so that `% 4` is the place in the bar.
    count: u64,
    packets: u64,
    /// When the current run of beats began. Two decks playing at once go to the
    /// one that was there first.
    playing_since: Option<Instant>,
    status: Option<(Instant, Status)>,
}

impl Deck {
    fn playing(&self, now: Instant) -> bool {
        self.beat_at.is_some_and(|at| {
            ms(now.saturating_duration_since(at)) <= STALE_PERIODS * self.period_ms + STALE_SLACK_MS
        })
    }

    fn status(&self, now: Instant) -> Option<Status> {
        self.status
            .filter(|(at, _)| now.saturating_duration_since(*at) <= STATUS_FRESH)
            .map(|(_, s)| s)
    }

    fn master(&self, now: Instant) -> bool {
        self.status(now).is_some_and(|s| s.master)
    }

    fn beat(&mut self, period_ms: f64, beat_in_bar: u8, now: Instant) {
        match self.beat_at.filter(|_| self.playing(now)) {
            // Whole beats since the last packet, so a lost one still counts.
            Some(prev) => {
                let n = (ms(now.saturating_duration_since(prev)) / self.period_ms).round();
                self.count += n.max(1.0) as u64;
            }
            None => self.playing_since = Some(now),
        }
        // Onto the bar the CDJ reports, the short way round: steady play never
        // moves it, and a hot cue or loop jump lands on the new bar.
        let want = u64::from(beat_in_bar - 1);
        let step = (want + 4 - self.count % 4) % 4;
        self.count = match step {
            3 if self.count > 0 => self.count - 1,
            _ => self.count + step,
        };
        self.period_ms = period_ms;
        self.beat_at = Some(now);
        self.packets += 1;
    }

    fn grid(&self, now: Instant) -> Grid {
        let since = self
            .beat_at
            .map_or(0.0, |at| ms(now.saturating_duration_since(at)));
        // Extrapolated, so a lost packet costs nothing.
        let past = (since / self.period_ms).floor();
        let into = since - past * self.period_ms;
        Grid {
            period_ms: Some(self.period_ms as f32),
            ms_to_next_beat: Some((self.period_ms - into) as f32),
            beat_index: ((self.count + past as u64) % 16) as u8,
            bar_valid: true,
            confidence: 1.0,
            tracking: true,
            coasting: false,
            ms_since_beat: Some(into as f32),
            ..Grid::default()
        }
    }
}

#[derive(Default)]
struct Shared {
    decks: [Deck; PLAYERS],
    /// Bit n is channel n+1, as the mixer last reported it. Channel n carries
    /// player n.
    on_air: Option<u8>,
    mixer_at: Option<Instant>,
    /// Any player's address, which names the subnet to announce on.
    player_ip: Option<Ipv4Addr>,
    /// Everyone's latest keep-alive. Only heard when announcing.
    devices: BTreeMap<u8, Heard>,
    notes: Vec<String>,
}

struct Heard {
    name: String,
    ip: Ipv4Addr,
    at: Instant,
}

impl Shared {
    /// Something arrived on the beat port.
    fn heard(&mut self, packet: &[u8], from: IpAddr, now: Instant) {
        if let Some(mask) = parse_on_air(packet) {
            self.on_air = Some(mask);
            return;
        }
        let Some(beat) = parse_beat(packet) else {
            return;
        };
        if beat.device == MIXER {
            self.mixer_at = Some(now);
            return;
        }
        let Some(period_ms) = beat.period_ms else {
            return;
        };
        if !(1..=4).contains(&beat.beat_in_bar) {
            return;
        }
        let Some(deck) = player_slot(beat.device).map(|i| &mut self.decks[i]) else {
            return;
        };
        deck.beat(period_ms, beat.beat_in_bar, now);
        if let IpAddr::V4(ip) = from {
            self.player_ip = Some(ip);
        }
    }

    /// Which deck drives the light: a master if the players have said, else the
    /// one already followed, else whichever has played longest. Only decks
    /// playing and on air count — a deck cued with its fader down never does.
    fn choose(&self, current: Option<usize>, now: Instant) -> Option<usize> {
        let mixer = self
            .mixer_at
            .is_some_and(|at| now.saturating_duration_since(at) <= MIXER_FRESH);
        let on_air = self.on_air.filter(|_| mixer);
        let eligible = |i: &usize| {
            self.decks[*i].playing(now) && on_air.is_none_or(|mask| mask & (1 << *i) != 0)
        };
        (0..PLAYERS)
            .filter(eligible)
            .find(|&i| self.decks[i].master(now))
            .or(current.filter(eligible))
            .or_else(|| {
                (0..PLAYERS)
                    .filter(eligible)
                    .min_by_key(|&i| self.decks[i].playing_since)
            })
    }
}

/// Watches the network from a thread of its own; the main loop polls it.
pub struct Follower {
    shared: Arc<Mutex<Shared>>,
    following: Option<usize>,
    seen: u64,
}

impl Follower {
    /// None when the mode is off.
    pub fn start(mode: Mode) -> Result<Option<Self>> {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let mode = match mode {
            Mode::Off => return Ok(None),
            Mode::Announce(_) if !cfg!(target_os = "linux") => {
                lock(&shared).notes.push(
                    "announcing needs the interface's MAC, read from /sys on Linux only; \
                     listening only"
                        .into(),
                );
                Mode::Passive
            }
            m => m,
        };
        let listener = shared.clone();
        std::thread::Builder::new()
            .name("prodjlink".into())
            .spawn(move || listen(&listener, mode))
            .context("starting the Pro DJ Link listener")?;
        Ok(Some(Follower {
            shared,
            following: None,
            seen: 0,
        }))
    }

    /// Anything the listener had to say since the last call.
    pub fn notes(&self) -> Vec<String> {
        std::mem::take(&mut lock(&self.shared).notes)
    }

    pub fn poll(&mut self, now: Instant) -> Option<Link> {
        let shared = lock(&self.shared);
        let chosen = shared.choose(self.following, now);
        let picked_up = chosen != self.following;
        self.following = chosen;
        let deck = shared.decks[chosen?];
        let beat = picked_up || deck.packets != self.seen;
        self.seen = deck.packets;
        Some(Link {
            player: chosen? as u8 + 1,
            grid: deck.grid(now),
            beat,
        })
    }

    /// Devices heard recently, in number order. Empty unless announcing.
    pub fn players(&self, now: Instant) -> Vec<Player> {
        let shared = lock(&self.shared);
        shared
            .devices
            .iter()
            .filter(|(_, h)| now.saturating_duration_since(h.at) <= PEER_FRESH)
            .map(|(&number, h)| Player {
                number,
                name: h.name.clone(),
                ip: h.ip,
                status: player_slot(number).and_then(|i| shared.decks[i].status(now)),
            })
            .collect()
    }
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    // A panic mid-update leaves at worst one stale deck, which goes stale on its
    // own; refusing every later poll would lose the grid for good.
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

fn note(shared: &Mutex<Shared>, msg: String) {
    lock(shared).notes.push(msg);
}

/// Never returns. A failure is reported once and retried, so a port that is
/// taken now, or an interface that is down, recovers without a restart.
fn listen(shared: &Mutex<Shared>, mode: Mode) {
    let mut said = None;
    loop {
        let Err(e) = serve(shared, mode);
        let msg = format!("Pro DJ Link: {e}; retrying");
        if said.as_ref() != Some(&msg) {
            note(shared, msg.clone());
            said = Some(msg);
        }
        std::thread::sleep(REBIND_AFTER);
    }
}

fn serve(shared: &Mutex<Shared>, mode: Mode) -> io::Result<Infallible> {
    // Wildcard, so the socket outlives the interface: a pulled cable needs no
    // rebind, the packets simply stop and resume.
    let beats = bind(BEAT_PORT)?;
    beats.set_read_timeout(Some(RECV_TIMEOUT))?;
    // Fails and retries on its own: losing the master flag must not cost the
    // beats.
    let mut announcer = None;
    let mut announce_tried: Option<Instant> = None;
    let mut said = None;
    let mut buf = [0u8; 1500];
    loop {
        match beats.recv_from(&mut buf) {
            Ok((n, from)) => lock(shared).heard(&buf[..n], from.ip(), Instant::now()),
            Err(e) if timed_out(&e) => {}
            Err(e) => return Err(e),
        }

        let Mode::Announce(number) = mode else {
            continue;
        };
        if announcer.is_none() && announce_tried.is_none_or(|t| t.elapsed() >= REBIND_AFTER) {
            announce_tried = Some(Instant::now());
            match Announcer::new(number) {
                Ok(a) => announcer = Some(a),
                Err(e) => {
                    let msg = format!("Pro DJ Link announce: {e}; listening only, retrying");
                    if said.as_ref() != Some(&msg) {
                        note(shared, msg.clone());
                        said = Some(msg);
                    }
                }
            }
        }
        if let Some(a) = &mut announcer {
            if let Err(e) = a.tick(shared, &mut buf) {
                note(shared, format!("Pro DJ Link announce: {e}; reopening"));
                announcer = None;
            }
        }
    }
}

/// Shared rather than exclusive: the ports are fixed by the protocol, and
/// anything else on this machine watching the CDJs needs them too.
fn bind(port: u16) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.set_reuse_port(true)?;
    socket.set_broadcast(true)?;
    socket
        .bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())
        .map_err(|e| io::Error::new(e.kind(), format!("binding UDP {port}: {e}")))?;
    Ok(socket.into())
}

fn timed_out(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn player_slot(device: u8) -> Option<usize> {
    (1..=PLAYERS as u8)
        .contains(&device)
        .then(|| usize::from(device) - 1)
}

/// Poses as a player just enough for the others to send it status.
struct Announcer {
    number: u8,
    /// Ours go out on it, and everyone's keep-alives come in on it.
    keep_alives: UdpSocket,
    status: UdpSocket,
    last_sent: Option<Instant>,
    /// Our address on the DJ Link subnet, to tell our own broadcasts from a
    /// rival's.
    own_ip: Option<Ipv4Addr>,
    peers: BTreeMap<u8, Instant>,
    /// Someone else has our number. Their claim stands; we only listen.
    yielded: bool,
    said: Option<String>,
}

impl Announcer {
    fn new(number: u8) -> io::Result<Self> {
        let keep_alives = bind(ANNOUNCE_PORT)?;
        keep_alives.set_nonblocking(true)?;
        let status = bind(STATUS_PORT)?;
        status.set_nonblocking(true)?;
        Ok(Announcer {
            number,
            keep_alives,
            status,
            last_sent: None,
            own_ip: None,
            peers: BTreeMap::new(),
            yielded: false,
            said: None,
        })
    }

    /// Reports a situation once, rather than every cycle it persists.
    fn say(&mut self, shared: &Mutex<Shared>, msg: String) {
        if self.said.as_ref() != Some(&msg) {
            note(shared, msg.clone());
            self.said = Some(msg);
        }
    }

    fn tick(&mut self, shared: &Mutex<Shared>, buf: &mut [u8]) -> io::Result<()> {
        let now = Instant::now();
        loop {
            let (n, from) = match self.keep_alives.recv_from(buf) {
                Ok(got) => got,
                Err(e) if timed_out(&e) => break,
                Err(e) => return Err(e),
            };
            let (Some(k), IpAddr::V4(ip)) = (parse_keep_alive(&buf[..n]), from.ip()) else {
                continue;
            };
            if k.device == self.number && Some(ip) != self.own_ip && !self.yielded {
                self.yielded = true;
                self.say(
                    shared,
                    format!(
                        "device {} is already {:?} at {ip}; listening only",
                        self.number, k.name
                    ),
                );
            }
            self.peers.insert(k.device, now);
            let mut shared = lock(shared);
            if player_slot(k.device).is_some() {
                shared.player_ip = Some(ip);
            }
            if Some(ip) != self.own_ip {
                let heard = Heard {
                    name: k.name,
                    ip,
                    at: now,
                };
                shared.devices.insert(k.device, heard);
            }
        }
        loop {
            let n = match self.status.recv_from(buf) {
                Ok((n, _)) => n,
                Err(e) if timed_out(&e) => break,
                Err(e) => return Err(e),
            };
            if let Some(s) = parse_status(&buf[..n]) {
                if let Some(i) = player_slot(s.device) {
                    lock(shared).decks[i].status = Some((now, s));
                }
            }
        }

        if self.yielded || self.last_sent.is_some_and(|t| now - t < ANNOUNCE_EVERY) {
            return Ok(());
        }
        self.last_sent = Some(now);
        let Some(player) = lock(shared).player_ip else {
            return Ok(());
        };
        // Resolved every cycle, so a replug or a new lease is simply followed.
        let Some(iface) = interface_for(player) else {
            self.say(shared, format!("no interface on {player}'s subnet to announce from"));
            return Ok(());
        };
        self.say(
            shared,
            format!("announcing as device {} on {} ({})", self.number, iface.name, iface.ip),
        );
        self.own_ip = Some(iface.ip);
        self.peers.retain(|_, at| now - *at <= PEER_FRESH);
        let peers = (self.peers.len() + 1).min(255) as u8;
        let packet = keep_alive(self.number, iface.mac, iface.ip, peers);
        // A failure here is an interface mid-flap; the next cycle resolves afresh.
        let _ = self
            .keep_alives
            .send_to(&packet, (iface.broadcast, ANNOUNCE_PORT));
        Ok(())
    }
}

struct Iface {
    name: String,
    ip: Ipv4Addr,
    broadcast: Ipv4Addr,
    mac: [u8; 6],
}

/// The interface sharing a subnet with the players. Not the default route's:
/// that is the wifi the light is on.
fn interface_for(player: Ipv4Addr) -> Option<Iface> {
    if_addrs::get_if_addrs().ok()?.into_iter().find_map(|i| {
        let if_addrs::IfAddr::V4(v4) = &i.addr else {
            return None;
        };
        let mask = u32::from(v4.netmask);
        if i.is_loopback() || u32::from(v4.ip) & mask != u32::from(player) & mask {
            return None;
        }
        Some(Iface {
            ip: v4.ip,
            broadcast: v4
                .broadcast
                .unwrap_or_else(|| Ipv4Addr::from(u32::from(v4.ip) | !mask)),
            mac: mac_of(&i.name)?,
            name: i.name,
        })
    })
}

#[cfg(target_os = "linux")]
fn mac_of(name: &str) -> Option<[u8; 6]> {
    let text = std::fs::read_to_string(format!("/sys/class/net/{name}/address")).ok()?;
    let mut parts = text.trim().split(':');
    let mut mac = [0u8; 6];
    for byte in &mut mac {
        *byte = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    Some(mac)
}

#[cfg(not(target_os = "linux"))]
fn mac_of(_name: &str) -> Option<[u8; 6]> {
    None
}

fn framed(p: &[u8], kind: u8, min_len: usize) -> bool {
    p.len() >= min_len && p[..10] == MAGIC && p[0x0a] == kind
}

#[derive(Debug, PartialEq)]
struct BeatPacket {
    device: u8,
    /// Effective, so a pitch ride is followed. None for an implausible tempo.
    period_ms: Option<f64>,
    beat_in_bar: u8,
}

fn parse_beat(p: &[u8]) -> Option<BeatPacket> {
    if !framed(p, TYPE_BEAT, BEAT_LEN) {
        return None;
    }
    let pitch = f64::from(u32::from_be_bytes([p[0x54], p[0x55], p[0x56], p[0x57]])) / NOMINAL_PITCH;
    let bpm = f64::from(u16::from_be_bytes([p[0x5a], p[0x5b]])) / 100.0 * pitch;
    Some(BeatPacket {
        device: p[0x21],
        period_ms: PLAUSIBLE_BPM.contains(&bpm).then(|| 60_000.0 / bpm),
        beat_in_bar: p[0x5c],
    })
}

/// Bit n set when channel n+1 is audible. Four-channel mixers send four flags;
/// the six-channel layout, from the DJM-V10, puts five and six further on.
fn parse_on_air(p: &[u8]) -> Option<u8> {
    if !framed(p, TYPE_ON_AIR, 0x24) {
        return None;
    }
    let flags: &[usize] = match u16::from_be_bytes([p[0x22], p[0x23]]) {
        0x09 => &[0x24, 0x25, 0x26, 0x27],
        0x11 => &[0x24, 0x25, 0x26, 0x27, 0x2d, 0x2e],
        _ => return None,
    };
    let mut mask = 0u8;
    for (ch, &at) in flags.iter().enumerate() {
        if *p.get(at)? != 0 {
            mask |= 1 << ch;
        }
    }
    Some(mask)
}

struct KeepAlive {
    device: u8,
    name: String,
}

fn parse_keep_alive(p: &[u8]) -> Option<KeepAlive> {
    if !framed(p, TYPE_KEEP_ALIVE, KEEP_ALIVE_LEN) {
        return None;
    }
    let name = &p[0x0c..0x20];
    let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
    Some(KeepAlive {
        device: p[0x24],
        name: String::from_utf8_lossy(&name[..end]).into_owned(),
    })
}

/// Every field read sits in the prefix the 0xd4-byte nexus layout shares with
/// the CDJ-3000's longer one.
fn parse_status(p: &[u8]) -> Option<Status> {
    if !framed(p, TYPE_STATUS, STATUS_FLAGS + 1) {
        return None;
    }
    let flags = p[STATUS_FLAGS];
    // A zero source player means nothing is loaded.
    let track = (p[0x28] != 0).then(|| Track {
        source: p[0x28],
        slot: p[0x29],
        kind: p[0x2a],
        id: u32::from_be_bytes([p[0x2c], p[0x2d], p[0x2e], p[0x2f]]),
    });
    Some(Status {
        device: p[0x21],
        track,
        playing: flags & STATUS_PLAYING != 0,
        on_air: flags & STATUS_ON_AIR != 0,
        master: flags & STATUS_MASTER != 0,
    })
}

/// beat-link's keep-alive, which players accept from a virtual CDJ.
fn keep_alive(number: u8, mac: [u8; 6], ip: Ipv4Addr, peers: u8) -> [u8; KEEP_ALIVE_LEN] {
    let mut p = [0u8; KEEP_ALIVE_LEN];
    p[..10].copy_from_slice(&MAGIC);
    p[0x0a] = TYPE_KEEP_ALIVE;
    p[0x0c..0x0c + NAME.len()].copy_from_slice(NAME);
    p[0x20] = 0x01;
    p[0x21] = 0x02;
    p[0x22..0x24].copy_from_slice(&(KEEP_ALIVE_LEN as u16).to_be_bytes());
    p[0x24] = number;
    p[0x25] = 0x01;
    p[0x26..0x2c].copy_from_slice(&mac);
    p[0x2c..0x30].copy_from_slice(&ip.octets());
    p[0x30] = peers;
    // A player rather than a mixer.
    p[0x34] = 0x01;
    // CDJ-3000 compatible. A wrong value here can make CDJ-3000s set to player
    // five or six drop off the network.
    p[0x35] = 0x64;
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANY: IpAddr = IpAddr::V4(Ipv4Addr::new(169, 254, 1, 2));

    fn beat_packet(device: u8, bpm_x100: u16, pitch: u32, beat_in_bar: u8) -> Vec<u8> {
        let mut p = vec![0u8; BEAT_LEN];
        p[..10].copy_from_slice(&MAGIC);
        p[0x0a] = TYPE_BEAT;
        p[0x21] = device;
        p[0x22..0x24].copy_from_slice(&0x3cu16.to_be_bytes());
        p[0x54..0x58].copy_from_slice(&pitch.to_be_bytes());
        p[0x5a..0x5c].copy_from_slice(&bpm_x100.to_be_bytes());
        p[0x5c] = beat_in_bar;
        p[0x5f] = device;
        p
    }

    fn on_air_packet(flags: &[u8]) -> Vec<u8> {
        let six = flags.len() == 6;
        let mut p = vec![0u8; if six { 0x35 } else { 0x2d }];
        p[..10].copy_from_slice(&MAGIC);
        p[0x0a] = TYPE_ON_AIR;
        p[0x20] = if six { 0x03 } else { 0x00 };
        p[0x21] = MIXER;
        p[0x22..0x24].copy_from_slice(&(if six { 0x11u16 } else { 0x09 }).to_be_bytes());
        for (i, &f) in flags.iter().enumerate() {
            p[[0x24, 0x25, 0x26, 0x27, 0x2d, 0x2e][i]] = f;
        }
        p
    }

    const NOMINAL: u32 = 0x10_0000;

    /// A deck playing at 120 BPM (500ms) from `start`, one packet per beat.
    fn play(shared: &mut Shared, device: u8, start: Instant, beats: u32, first_in_bar: u8) {
        for k in 0..beats {
            let bib = (first_in_bar - 1 + k as u8) % 4 + 1;
            let at = start + Duration::from_millis(500 * u64::from(k));
            shared.heard(&beat_packet(device, 12_000, NOMINAL, bib), ANY, at);
        }
    }

    fn follower() -> Follower {
        Follower {
            shared: Arc::new(Mutex::new(Shared::default())),
            following: None,
            seen: 0,
        }
    }

    #[test]
    fn a_beat_packet_reads_as_its_effective_tempo() {
        let p = parse_beat(&beat_packet(2, 12_800, NOMINAL, 3)).unwrap();
        assert_eq!(p.device, 2);
        assert_eq!(p.beat_in_bar, 3);
        assert!((p.period_ms.unwrap() - 468.75).abs() < 1e-9);

        // +8% on the fader is a faster beat, not the track's own.
        let fast = parse_beat(&beat_packet(2, 12_800, NOMINAL + NOMINAL / 100 * 8, 1)).unwrap();
        let bpm = 60_000.0 / fast.period_ms.unwrap();
        assert!((bpm - 138.24).abs() < 0.01, "got {bpm}");
    }

    #[test]
    fn garbage_is_not_a_beat() {
        let mut bad = beat_packet(1, 12_000, NOMINAL, 1);
        bad[0] = b'X';
        assert!(parse_beat(&bad).is_none(), "wrong magic");
        assert!(parse_beat(&beat_packet(1, 12_000, NOMINAL, 1)[..0x50]).is_none(), "short");
        // No track loaded reports 0xffff.
        assert_eq!(parse_beat(&beat_packet(1, 0xffff, NOMINAL, 1)).unwrap().period_ms, None);
    }

    #[test]
    fn the_mixer_metronome_is_not_a_deck() {
        let mut shared = Shared::default();
        let t = Instant::now();
        play(&mut shared, MIXER, t, 4, 1);
        assert_eq!(shared.choose(None, t + Duration::from_millis(1600)), None);
    }

    #[test]
    fn on_air_flags_read_for_four_and_six_channels() {
        assert_eq!(parse_on_air(&on_air_packet(&[1, 0, 1, 0])), Some(0b0101));
        assert_eq!(parse_on_air(&on_air_packet(&[0, 0, 0, 0, 1, 1])), Some(0b11_0000));
    }

    #[test]
    fn the_count_lands_on_the_bar() {
        let mut shared = Shared::default();
        let t = Instant::now();
        // Picked up on the third beat of a bar.
        play(&mut shared, 1, t, 1, 3);
        let g = shared.decks[0].grid(t);
        assert_eq!(g.beat_index % 4, 2);
        assert!(g.bar_valid);

        // A hot cue to a downbeat moves the count with it.
        shared.heard(&beat_packet(1, 12_000, NOMINAL, 1), ANY, t + Duration::from_millis(500));
        assert_eq!(shared.decks[0].grid(t + Duration::from_millis(500)).beat_index % 4, 0);
    }

    #[test]
    fn a_lost_packet_is_extrapolated() {
        let mut shared = Shared::default();
        let t = Instant::now();
        play(&mut shared, 1, t, 4, 1);
        let last = t + Duration::from_millis(1500);
        let before = shared.decks[0].grid(last).beat_index;

        // 750ms after the last packet: the next beat came and went unreported.
        let g = shared.decks[0].grid(last + Duration::from_millis(750));
        assert!((g.ms_to_next_beat.unwrap() - 250.0).abs() < 0.5);
        assert_eq!(g.beat_index, (before + 1) % 16);
        assert_eq!(g.beat_index % 4, 0, "the fifth beat is a downbeat");

        // The packet after it counts two beats, not one.
        shared.heard(&beat_packet(1, 12_000, NOMINAL, 2), ANY, last + Duration::from_millis(1000));
        assert_eq!(shared.decks[0].grid(last + Duration::from_millis(1000)).beat_index, (before + 2) % 16);
    }

    #[test]
    fn a_fader_coming_up_does_not_take_the_light() {
        let mut f = follower();
        let t = Instant::now();
        {
            // Deck one has played longest, but cued in the headphones.
            let mut s = lock(&f.shared);
            play(&mut s, 1, t, 16, 1);
            play(&mut s, 2, t + Duration::from_millis(1000), 14, 1);
            play(&mut s, MIXER, t, 16, 1);
            s.heard(&on_air_packet(&[0, 1, 0, 0]), ANY, t);
        }
        assert_eq!(f.poll(t + Duration::from_millis(3600)).unwrap().player, 2);

        // The blend: both on air, and the light stays where it was.
        lock(&f.shared).heard(&on_air_packet(&[1, 1, 0, 0]), ANY, t + Duration::from_millis(4000));
        assert_eq!(f.poll(t + Duration::from_millis(7600)).unwrap().player, 2);
    }

    #[test]
    fn a_deck_off_air_is_ignored() {
        let mut f = follower();
        let t = Instant::now();
        {
            let mut s = lock(&f.shared);
            play(&mut s, 1, t, 8, 1);
            play(&mut s, 2, t + Duration::from_millis(1000), 8, 1);
            play(&mut s, MIXER, t, 8, 1);
            s.heard(&on_air_packet(&[0, 1, 0, 0]), ANY, t);
        }
        assert_eq!(f.poll(t + Duration::from_millis(3600)).unwrap().player, 2);
    }

    #[test]
    fn on_air_flags_lapse_with_the_mixer() {
        let mut f = follower();
        let t = Instant::now();
        {
            let mut s = lock(&f.shared);
            s.heard(&on_air_packet(&[0, 0, 0, 0]), ANY, t);
            play(&mut s, MIXER, t, 1, 1);
            play(&mut s, 1, t, 20, 1);
        }
        // With the mixer gone, its last word on the faders is no longer believed.
        assert_eq!(f.poll(t + Duration::from_millis(9600)).unwrap().player, 1);
    }

    #[test]
    fn stopping_hands_over_then_lets_go() {
        let mut f = follower();
        let t = Instant::now();
        play(&mut lock(&f.shared), 1, t, 8, 1);
        play(&mut lock(&f.shared), 2, t + Duration::from_millis(1000), 12, 1);
        assert_eq!(f.poll(t + Duration::from_millis(3600)).unwrap().player, 1);

        // Deck one's last beat was at 3500ms; it is stale 1150ms later.
        assert_eq!(f.poll(t + Duration::from_millis(4600)).unwrap().player, 1);
        assert_eq!(f.poll(t + Duration::from_millis(4700)).unwrap().player, 2);

        // Deck two's last beat was at 6500ms.
        assert!(f.poll(t + Duration::from_millis(7700)).is_none());
    }

    #[test]
    fn the_master_overrides_the_incumbent() {
        let mut f = follower();
        let t = Instant::now();
        play(&mut lock(&f.shared), 1, t, 8, 1);
        play(&mut lock(&f.shared), 2, t, 8, 1);
        let at = t + Duration::from_millis(3600);
        assert_eq!(f.poll(at).unwrap().player, 1);

        let master = parse_status(&status_packet(2, 0xd4, None, STATUS_PLAYING | STATUS_MASTER));
        lock(&f.shared).decks[1].status = Some((at, master.unwrap()));
        assert_eq!(f.poll(at).unwrap().player, 2);
    }

    fn status_packet(device: u8, len: usize, track: Option<Track>, flags: u8) -> Vec<u8> {
        let mut p = vec![0u8; len];
        p[..10].copy_from_slice(&MAGIC);
        p[0x0a] = TYPE_STATUS;
        p[0x21] = device;
        if let Some(t) = track {
            p[0x28] = t.source;
            p[0x29] = t.slot;
            p[0x2a] = t.kind;
            p[0x2c..0x30].copy_from_slice(&t.id.to_be_bytes());
        }
        p[STATUS_FLAGS] = flags;
        p
    }

    #[test]
    fn a_status_packet_names_its_track_on_both_generations() {
        // Player 3 playing on air, from player 1's USB over Link.
        let track = Track {
            source: 1,
            slot: 3,
            kind: 1,
            id: 0x0001_2345,
        };
        for len in [0xd4, 0x200] {
            let s = parse_status(&status_packet(3, len, Some(track), 0x40 | 0x08)).unwrap();
            assert_eq!(s.device, 3);
            assert_eq!(s.track, Some(track));
            assert!(s.playing && s.on_air && !s.master);
        }
        let empty = parse_status(&status_packet(2, 0xd4, None, 0)).unwrap();
        assert_eq!(empty.track, None);
        assert!(!empty.playing && !empty.on_air);
        assert!(parse_status(&status_packet(2, 0xd4, None, 0)[..0x80]).is_none(), "short");
    }

    #[test]
    fn each_beat_is_reported_once() {
        let mut f = follower();
        let t = Instant::now();
        play(&mut lock(&f.shared), 1, t, 1, 1);
        assert!(f.poll(t).unwrap().beat, "picking a deck up is news");
        assert!(!f.poll(t + Duration::from_millis(100)).unwrap().beat);

        play(&mut lock(&f.shared), 1, t + Duration::from_millis(500), 1, 2);
        assert!(f.poll(t + Duration::from_millis(501)).unwrap().beat);
        assert!(!f.poll(t + Duration::from_millis(502)).unwrap().beat);
    }

    #[test]
    fn our_keep_alive_reads_back_as_us() {
        let ip = Ipv4Addr::new(169, 254, 7, 9);
        let p = keep_alive(7, [2, 0, 0, 0, 0, 1], ip, 3);
        let k = parse_keep_alive(&p).unwrap();
        assert_eq!(k.device, 7);
        assert_eq!(k.name, "audio-bridge");
        assert_eq!(&p[0x2c..0x30], &ip.octets());
        assert_eq!(u16::from_be_bytes([p[0x22], p[0x23]]) as usize, p.len());
    }
}
