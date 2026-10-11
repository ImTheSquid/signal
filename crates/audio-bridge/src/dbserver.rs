//! Track titles from the database server of the player holding the media.
//!
//! Every player answers metadata queries over TCP for the media in its slots,
//! which is what makes a guest's USB readable without their rekordbox library.
//! Framing is from Deep Symmetry's analysis (djl-analysis.deepsymmetry.org,
//! "Track Metadata"), checked against beat-link's dbserver package.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::link::{Player, Track};

/// Answers with the port the database server itself is on.
const QUERY_PORT: u16 = 12523;
const QUERY: &[u8] = b"\x00\x00\x00\x0fRemoteDBServer\x00";
const TIMEOUT: Duration = Duration::from_secs(3);

const MESSAGE_START: u32 = 0x8723_49ae;
/// Transaction id of the setup and teardown messages.
const CONTROL_TX: u32 = 0xffff_fffe;

const SETUP: u16 = 0x0000;
const TEARDOWN: u16 = 0x0100;
const METADATA: u16 = 0x2002;
const UNANALYZED_METADATA: u16 = 0x2202;
const RENDER: u16 = 0x3000;
const MENU_AVAILABLE: u16 = 0x4000;
const MENU_HEADER: u16 = 0x4001;
const UNAVAILABLE: u16 = 0x4003;
const MENU_ITEM: u16 = 0x4101;
const MENU_FOOTER: u16 = 0x4201;

const MAIN_MENU: u32 = 1;
const UNANALYZED: u8 = 2;
const NO_RESULTS: u32 = 0xffff_ffff;
/// Far more than a track's metadata menu ever has.
const MAX_ITEMS: u32 = 64;

const ITEM_TITLE: u32 = 0x04;
const ITEM_ARTIST: u32 = 0x07;
/// CDJ-3000s use the high half of the item type for something else.
const ITEM_TYPE_MASK: u32 = 0xffff;

/// Players that answer a query made under their own number. Others answer only
/// a real player 1-4 — per beat-link, which keys this on the announced name.
const FLEXIBLE: [&str; 2] = ["CDJ-3000", "XDJ-AZ"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    pub title: String,
    pub artist: String,
}

/// The player number to ask `target` as. A non-flexible player must be asked by
/// a present player 1-4 other than itself, and not one that has loaded a track
/// from it, whose own session with it would clash.
pub fn asking_number(target: &Player, players: &[Player]) -> Option<u8> {
    if target.number >= 7 || FLEXIBLE.contains(&target.name.as_str()) {
        return Some(target.number);
    }
    players
        .iter()
        .find(|p| {
            (1..=4).contains(&p.number)
                && p.number != target.number
                && p
                    .status
                    .is_some_and(|s| s.track.is_none_or(|t| t.source != target.number))
        })
        .map(|p| p.number)
}

/// Title and artist of `track`, asked of its source player at `ip`. None when
/// the player has no metadata for it.
pub fn lookup(ip: Ipv4Addr, track: Track, asking: u8) -> Result<Option<Meta>> {
    let port = server_port(ip).context("asking for the dbserver port")?;
    let mut s = connect(SocketAddr::from((ip, port)))?;

    Field::num(1).write(&mut s)?;
    if Field::read(&mut s)? != Field::num(1) {
        bail!("no greeting");
    }

    let setup = exchange(&mut s, CONTROL_TX, SETUP, vec![Field::num(u32::from(asking))])?;
    if setup.kind != MENU_AVAILABLE {
        bail!("setup as player {asking} answered {:#06x}", setup.kind);
    }
    if setup.num(1) != Some(u32::from(track.source)) {
        bail!("expected player {}, got {:?}", track.source, setup.num(1));
    }

    let meta = query(&mut s, track, asking);
    // Polite only: the server closes its end on reading it.
    let _ = Message::new(CONTROL_TX, TEARDOWN, vec![]).write(&mut s);
    meta
}

fn query(s: &mut TcpStream, track: Track, asking: u8) -> Result<Option<Meta>> {
    let request = match track.kind {
        UNANALYZED => UNANALYZED_METADATA,
        _ => METADATA,
    };
    let rmst = Field::num(
        u32::from(asking) << 24 | MAIN_MENU << 16 | u32::from(track.slot) << 8 | u32::from(track.kind),
    );
    let available = exchange(s, 1, request, vec![rmst.clone(), Field::num(track.id)])?;
    match available.kind {
        UNAVAILABLE => return Ok(None),
        MENU_AVAILABLE => {}
        other => bail!("metadata request answered {other:#06x}"),
    }
    let count = match available.num(1) {
        None | Some(0) | Some(NO_RESULTS) => return Ok(None),
        Some(n) => n.min(MAX_ITEMS),
    };

    let n = Field::num;
    let args = vec![rmst, n(0), n(count), n(0), n(count), n(0)];
    if exchange(s, 2, RENDER, args)?.kind != MENU_HEADER {
        bail!("render did not start with a menu header");
    }
    let mut meta = Meta {
        title: String::new(),
        artist: String::new(),
    };
    loop {
        let item = Message::read(s)?;
        match item.kind {
            MENU_FOOTER => break,
            MENU_ITEM => {}
            other => bail!("unexpected {other:#06x} in a menu"),
        }
        let Some(Field::Str(label)) = item.args.get(3) else {
            continue;
        };
        match item.num(6).map(|t| t & ITEM_TYPE_MASK) {
            Some(ITEM_TITLE) => meta.title = label.clone(),
            Some(ITEM_ARTIST) => meta.artist = label.clone(),
            _ => {}
        }
    }
    Ok(Some(meta))
}

fn server_port(ip: Ipv4Addr) -> Result<u16> {
    let mut s = connect(SocketAddr::from((ip, QUERY_PORT)))?;
    s.write_all(QUERY)?;
    let mut port = [0u8; 2];
    s.read_exact(&mut port)?;
    match u16::from_be_bytes(port) {
        // Still booting.
        0xffff => bail!("not ready"),
        p => Ok(p),
    }
}

fn connect(addr: SocketAddr) -> Result<TcpStream> {
    let s = TcpStream::connect_timeout(&addr, TIMEOUT).with_context(|| format!("connecting to {addr}"))?;
    s.set_read_timeout(Some(TIMEOUT))?;
    s.set_write_timeout(Some(TIMEOUT))?;
    Ok(s)
}

fn exchange(s: &mut TcpStream, tx: u32, kind: u16, args: Vec<Field>) -> Result<Message> {
    Message::new(tx, kind, args).write(s)?;
    let reply = Message::read(s)?;
    if reply.tx != tx {
        bail!("reply to transaction {tx} came as {}", reply.tx);
    }
    Ok(reply)
}

#[derive(Clone, Debug, PartialEq)]
enum Field {
    /// Width in bytes, 1, 2 or 4.
    Num(u8, u32),
    Bin(Vec<u8>),
    Str(String),
}

impl Field {
    fn num(v: u32) -> Field {
        Field::Num(4, v)
    }

    /// How a message's tag list names this field.
    fn arg_tag(&self) -> u8 {
        match self {
            Field::Num(..) => 0x06,
            Field::Bin(_) => 0x03,
            Field::Str(_) => 0x02,
        }
    }

    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Field::Num(1, v) => out.extend([0x0f, *v as u8]),
            Field::Num(2, v) => {
                out.push(0x10);
                out.extend((*v as u16).to_be_bytes());
            }
            Field::Num(_, v) => {
                out.push(0x11);
                out.extend(v.to_be_bytes());
            }
            // An empty blob goes as nothing at all, not as a zero length.
            Field::Bin(b) if b.is_empty() => {}
            Field::Bin(b) => {
                out.push(0x14);
                out.extend((b.len() as u32).to_be_bytes());
                out.extend(b);
            }
            // UTF-16BE with a NUL, its length counted in code units.
            Field::Str(text) => {
                let units: Vec<u16> = text.encode_utf16().chain([0]).collect();
                out.push(0x26);
                out.extend((units.len() as u32).to_be_bytes());
                out.extend(units.iter().flat_map(|u| u.to_be_bytes()));
            }
        }
    }

    fn write(&self, w: &mut impl Write) -> io::Result<()> {
        let mut out = Vec::new();
        self.encode(&mut out);
        w.write_all(&out)
    }

    fn read(r: &mut impl Read) -> io::Result<Field> {
        let tag = read_array::<1>(r)?[0];
        Ok(match tag {
            0x0f => Field::Num(1, u32::from(read_array::<1>(r)?[0])),
            0x10 => Field::Num(2, u32::from(u16::from_be_bytes(read_array(r)?))),
            0x11 => Field::Num(4, u32::from_be_bytes(read_array(r)?)),
            0x14 => {
                let len = u32::from_be_bytes(read_array(r)?);
                Field::Bin(read_vec(r, len as usize)?)
            }
            0x26 => {
                let units = u32::from_be_bytes(read_array(r)?) as usize;
                let bytes = read_vec(r, units * 2)?;
                let utf16: Vec<u16> = bytes
                    .chunks_exact(2)
                    .map(|c| u16::from_be_bytes([c[0], c[1]]))
                    .take_while(|&u| u != 0)
                    .collect();
                Field::Str(String::from_utf16_lossy(&utf16))
            }
            other => return Err(bad(format!("unknown field tag {other:#04x}"))),
        })
    }
}

#[derive(Debug, PartialEq)]
struct Message {
    tx: u32,
    kind: u16,
    args: Vec<Field>,
}

impl Message {
    fn new(tx: u32, kind: u16, args: Vec<Field>) -> Message {
        Message { tx, kind, args }
    }

    fn num(&self, i: usize) -> Option<u32> {
        match self.args.get(i) {
            Some(Field::Num(_, v)) => Some(*v),
            _ => None,
        }
    }

    fn encode(&self) -> Vec<u8> {
        // Always twelve tags, the unused ones zero.
        let mut tags = vec![0u8; 12];
        for (tag, arg) in tags.iter_mut().zip(&self.args) {
            *tag = arg.arg_tag();
        }
        let mut out = Vec::new();
        for f in [
            Field::num(MESSAGE_START),
            Field::num(self.tx),
            Field::Num(2, u32::from(self.kind)),
            Field::Num(1, self.args.len() as u32),
            Field::Bin(tags),
        ]
        .iter()
        .chain(&self.args)
        {
            f.encode(&mut out);
        }
        out
    }

    fn write(&self, w: &mut impl Write) -> io::Result<()> {
        w.write_all(&self.encode())
    }

    fn read(r: &mut impl Read) -> io::Result<Message> {
        if Field::read(r)? != Field::num(MESSAGE_START) {
            return Err(bad("message did not start with the marker".into()));
        }
        let (Field::Num(4, tx), Field::Num(2, kind), Field::Num(1, count), Field::Bin(tags)) =
            (Field::read(r)?, Field::read(r)?, Field::read(r)?, Field::read(r)?)
        else {
            return Err(bad("malformed message header".into()));
        };
        let count = count as usize;
        if count > 12 || tags.len() < count {
            return Err(bad(format!("{count} arguments")));
        }
        let mut args: Vec<Field> = Vec::with_capacity(count);
        for &tag in &tags[..count] {
            // A blob whose preceding length is zero was sent as nothing.
            let arg = match args.last() {
                Some(Field::Num(_, 0)) if tag == 0x03 => Field::Bin(Vec::new()),
                _ => Field::read(r)?,
            };
            if arg.arg_tag() != tag {
                return Err(bad(format!("argument tagged {tag:#04x} read as {arg:?}")));
            }
            args.push(arg);
        }
        Ok(Message {
            tx,
            kind: kind as u16,
            args,
        })
    }
}

fn read_array<const N: usize>(r: &mut impl Read) -> io::Result<[u8; N]> {
    let mut b = [0u8; N];
    r.read_exact(&mut b)?;
    Ok(b)
}

fn read_vec(r: &mut impl Read, len: usize) -> io::Result<Vec<u8>> {
    // Bounded, so a corrupt length cannot ask for gigabytes.
    if len > 1 << 20 {
        return Err(bad(format!("{len}-byte field")));
    }
    let mut b = vec![0u8; len];
    r.read_exact(&mut b)?;
    Ok(b)
}

fn bad(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::Status;

    #[test]
    fn the_greeting_is_a_four_byte_one() {
        let mut out = Vec::new();
        Field::num(1).encode(&mut out);
        assert_eq!(out, [0x11, 0, 0, 0, 1]);
    }

    #[test]
    fn a_setup_message_is_laid_out_as_beat_link_sends_it() {
        let bytes = Message::new(CONTROL_TX, SETUP, vec![Field::num(1)]).encode();
        let want: &[u8] = &[
            0x11, 0x87, 0x23, 0x49, 0xae, // start
            0x11, 0xff, 0xff, 0xff, 0xfe, // transaction
            0x10, 0x00, 0x00, // type
            0x0f, 0x01, // one argument
            0x14, 0x00, 0x00, 0x00, 0x0c, 0x06, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // tags
            0x11, 0x00, 0x00, 0x00, 0x01, // player 1
        ];
        assert_eq!(bytes, want);
    }

    #[test]
    fn a_menu_item_reads_back() {
        let item = Message::new(
            7,
            MENU_ITEM,
            vec![
                Field::num(0),
                Field::num(42),
                Field::num(12),
                Field::Str("Strobe — Edit".into()),
                Field::num(0),
                Field::Str(String::new()),
                Field::num(0x0001_0004),
                Field::num(0),
                Field::num(0),
                Field::num(0),
                Field::num(0),
                Field::num(0),
            ],
        );
        let bytes = item.encode();
        assert_eq!(Message::read(&mut bytes.as_slice()).unwrap(), item);
        assert_eq!(item.num(6).map(|t| t & ITEM_TYPE_MASK), Some(ITEM_TITLE));
    }

    #[test]
    fn a_blob_after_a_zero_length_is_not_read() {
        let msg = Message::new(3, 0x4402, vec![Field::num(0), Field::Bin(Vec::new())]);
        let mut bytes = msg.encode();
        // Anything after belongs to the next message.
        bytes.extend([0xde, 0xad]);
        let mut r = bytes.as_slice();
        assert_eq!(Message::read(&mut r).unwrap(), msg);
        assert_eq!(r, [0xde, 0xad]);
    }

    fn player(number: u8, name: &str, source: Option<u8>) -> Player {
        Player {
            number,
            name: name.into(),
            ip: Ipv4Addr::new(169, 254, 0, number),
            status: Some(Status {
                device: number,
                track: source.map(|source| Track {
                    source,
                    slot: 3,
                    kind: 1,
                    id: 1,
                }),
                playing: false,
                on_air: false,
                master: false,
            }),
        }
    }

    #[test]
    fn a_cdj_3000_is_asked_as_itself() {
        let booth = [player(1, "CDJ-3000", Some(1)), player(3, "CDJ-2000NXS2", Some(1))];
        assert_eq!(asking_number(&booth[0], &booth), Some(1));
    }

    #[test]
    fn a_nexus_is_asked_as_a_player_not_linked_to_it() {
        let booth = [
            player(1, "CDJ-3000", Some(3)),
            player(2, "CDJ-3000", Some(2)),
            player(3, "CDJ-2000NXS2", Some(3)),
            player(4, "CDJ-2000NXS2", None),
        ];
        // Player 1 has loaded from 3, so 2 is the first that may ask.
        assert_eq!(asking_number(&booth[2], &booth), Some(2));
    }

    #[test]
    fn a_nexus_everyone_links_to_cannot_be_asked() {
        let booth = [
            player(1, "CDJ-3000", Some(3)),
            player(3, "CDJ-2000NXS2", Some(3)),
        ];
        assert_eq!(asking_number(&booth[1], &booth), None);
    }
}
