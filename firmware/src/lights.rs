use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use esp_idf_svc::hal::gpio::{AnyOutputPin, Output, PinDriver};

const R: u8 = 1 << 0;
const Y: u8 = 1 << 1;
const G: u8 = 1 << 2;
const MASKS: [u8; 3] = [R, Y, G];

/// Relay driver for the three lamps. Shared between the script thread
/// (writes) and the main loop (reads state for heartbeats).
pub struct Lights {
    pins: Mutex<[PinDriver<'static, Output>; 3]>,
    state: AtomicU8,
    dirty: AtomicBool,
    active_low: bool,
    /// Per-lamp time of the last physical transition, paired with `min_dwell`
    /// to keep scripts from chattering the contacts.
    last_change: Mutex<[Option<Instant>; 3]>,
    min_dwell: Duration,
    /// Per-lamp count of physical transitions since boot. Mechanical relays are
    /// rated in operations, so this is the only way to tell whether a lighting
    /// pattern is affordable rather than guessing from datasheet estimates.
    ops: [AtomicU32; 3],
}

impl Lights {
    pub fn new(
        red: AnyOutputPin<'static>,
        yellow: AnyOutputPin<'static>,
        green: AnyOutputPin<'static>,
        active_low: bool,
        min_dwell: Duration,
    ) -> anyhow::Result<Self> {
        let lights = Lights {
            pins: Mutex::new([
                PinDriver::output(red)?,
                PinDriver::output(yellow)?,
                PinDriver::output(green)?,
            ]),
            state: AtomicU8::new(0),
            dirty: AtomicBool::new(false),
            active_low,
            last_change: Mutex::new([None; 3]),
            min_dwell,
            ops: [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)],
        };
        lights.set(false, false, false);
        Ok(lights)
    }

    /// When the relays may next move to `(r, y, g)`, or `None` if now.
    /// Callers block on this instead of dropping writes, so the lamps always
    /// reach the state the script asked for — just no faster than the
    /// hardware can follow.
    pub fn ready_at(&self, r: bool, y: bool, g: bool) -> Option<Instant> {
        let changing = pack(r, y, g) ^ self.state.load(Ordering::SeqCst);
        if changing == 0 {
            return None;
        }
        let now = Instant::now();
        let last = self.last_change.lock().unwrap();
        MASKS
            .iter()
            .zip(last.iter())
            .filter(|(mask, _)| changing & **mask != 0)
            .filter_map(|(_, at)| at.map(|at| at + self.min_dwell))
            .filter(|at| *at > now)
            .max()
    }

    pub fn set(&self, r: bool, y: bool, g: bool) {
        let mut pins = self.pins.lock().unwrap();
        for (pin, on) in pins.iter_mut().zip([r, y, g]) {
            let high = on != self.active_low;
            let result = if high { pin.set_high() } else { pin.set_low() };
            if let Err(e) = result {
                log::warn!("gpio write failed: {e}");
            }
        }
        let bits = pack(r, y, g);
        let prev = self.state.swap(bits, Ordering::SeqCst);
        if prev != bits {
            self.dirty.store(true, Ordering::SeqCst);
            let now = Instant::now();
            let mut last = self.last_change.lock().unwrap();
            for ((at, mask), ops) in last.iter_mut().zip(MASKS).zip(self.ops.iter()) {
                if (prev ^ bits) & mask != 0 {
                    *at = Some(now);
                    ops.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    pub fn get(&self) -> (bool, bool, bool) {
        let bits = self.state.load(Ordering::SeqCst);
        (bits & R != 0, bits & Y != 0, bits & G != 0)
    }

    /// Physical transitions per lamp since boot, for wear accounting.
    pub fn ops(&self) -> (u32, u32, u32) {
        (
            self.ops[0].load(Ordering::Relaxed),
            self.ops[1].load(Ordering::Relaxed),
            self.ops[2].load(Ordering::Relaxed),
        )
    }

    /// True once per change; the main loop uses this to push state promptly.
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::SeqCst)
    }
}

fn pack(r: bool, y: bool, g: bool) -> u8 {
    (r as u8 * R) | (y as u8 * Y) | (g as u8 * G)
}
