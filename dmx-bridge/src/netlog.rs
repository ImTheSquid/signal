//! Logging over UDP, mirrored to the console.
//!
//! TinyUSB and USB-Serial-JTAG share one PHY on the S3, so a board whose only
//! console is that port goes silent the moment it starts pretending to be an
//! FTDI device. This one has a separate UART bridge and
//! `CONFIG_ESP_CONSOLE_SECONDARY_NONE`, so the console survives — but UDP stays
//! the sink that works regardless of how the board is wired.

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use log::{LevelFilter, Metadata, Record};

struct Sink {
    socket: UdpSocket,
    addr: SocketAddr,
}

/// Set once, after wifi is up.
static SINK: OnceLock<Sink> = OnceLock::new();

/// Lines logged before the sink existed — wifi bring-up, and anything that
/// panics during it. Without this they would only ever reach the console, which
/// is exactly what stops existing once TinyUSB claims the port.
static BACKLOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Enough for boot; a bridge that produces more than this before wifi is up has
/// a worse problem than truncated logs.
const BACKLOG_MAX: usize = 64;

/// Set when TinyUSB takes the USB PHY. Writing to the console after that point
/// blocks forever: stdout is USB-Serial-JTAG, the peripheral is gone, and
/// nothing is draining the FIFO. That deadlocks the first log call after USB
/// init, which looks exactly like a silent hang.
static CONSOLE_OFF: AtomicBool = AtomicBool::new(false);

/// Stop writing to stdout. Needed only if the console shares the USB PHY that
/// TinyUSB is about to claim; see the module docs.
#[allow(dead_code)]
pub fn disable_console() {
    CONSOLE_OFF.store(true, Ordering::SeqCst);
}

struct NetLogger;

impl log::Log for NetLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        let line = format!(
            "[{:<5}] {}: {}",
            record.level(),
            record.target(),
            record.args()
        );
        // Console first: it is the only sink that exists before wifi, and the
        // only one left if the UDP send fails. Skipped once USB is claimed.
        if !CONSOLE_OFF.load(Ordering::Relaxed) {
            println!("{line}");
        }
        match SINK.get() {
            Some(sink) => {
                let _ = sink.socket.send_to(line.as_bytes(), sink.addr);
            }
            None => {
                if let Ok(mut backlog) = BACKLOG.lock() {
                    if backlog.len() < BACKLOG_MAX {
                        backlog.push(line);
                    }
                }
            }
        }
    }

    fn flush(&self) {}
}

/// Install as the global logger. Console-only until [`attach`] runs.
///
/// Deliberately not `EspLogger::initialize_default()` — that claims the global
/// logger and would leave no way to tee to the network. ESP-IDF's own C-side
/// logging is unaffected and still reaches the console.
pub fn init(level: LevelFilter) {
    if log::set_boxed_logger(Box::new(NetLogger)).is_ok() {
        log::set_max_level(level);
    }
}

/// Start mirroring to the network. Resolving is done here rather than at
/// compile time so a hostname works, but the default is a broadcast address
/// and needs no DNS.
pub fn attach(host: &str, port: u16) -> anyhow::Result<SocketAddr> {
    let addr = (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow::anyhow!("log_host {host}:{port} resolved to nothing"))?;

    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.set_broadcast(true)?;
    // A log line must never stall the caller; drop it instead.
    socket.set_write_timeout(Some(std::time::Duration::from_millis(50)))?;

    if SINK.set(Sink { socket, addr }).is_err() {
        anyhow::bail!("log sink already attached");
    }

    // Replay boot, then release the memory — the backlog is never used again.
    if let (Some(sink), Ok(mut backlog)) = (SINK.get(), BACKLOG.lock()) {
        for line in backlog.drain(..) {
            let _ = sink.socket.send_to(line.as_bytes(), sink.addr);
        }
        backlog.shrink_to_fit();
    }
    Ok(addr)
}
