use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Mutex;

use esp_idf_svc::hal::gpio::{AnyOutputPin, Output, PinDriver};

const R: u8 = 1 << 0;
const Y: u8 = 1 << 1;
const G: u8 = 1 << 2;

/// Relay driver for the three lamps. Shared between the script thread
/// (writes) and the main loop (reads state for heartbeats).
pub struct Lights {
    pins: Mutex<[PinDriver<'static, Output>; 3]>,
    state: AtomicU8,
    dirty: AtomicBool,
    active_low: bool,
}

impl Lights {
    pub fn new(
        red: AnyOutputPin<'static>,
        yellow: AnyOutputPin<'static>,
        green: AnyOutputPin<'static>,
        active_low: bool,
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
        };
        lights.set(false, false, false);
        Ok(lights)
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
        let bits = (r as u8 * R) | (y as u8 * Y) | (g as u8 * G);
        if self.state.swap(bits, Ordering::SeqCst) != bits {
            self.dirty.store(true, Ordering::SeqCst);
        }
    }

    pub fn get(&self) -> (bool, bool, bool) {
        let bits = self.state.load(Ordering::SeqCst);
        (bits & R != 0, bits & Y != 0, bits & G != 0)
    }

    /// True once per change; the main loop uses this to push state promptly.
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::SeqCst)
    }
}
