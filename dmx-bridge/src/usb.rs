//! Pretending to be an FTDI device, so rekordbox's ENTTEC DMX-IF probe finds us.
//!
//! Both Enttec interfaces rekordbox supports are FTDI-based and use FTDI's stock
//! VID/PID, and rekordbox exposes a single "ENTTEC DMX-IF" toggle rather than
//! per-model entries — so it almost certainly opens any FTDI device and then
//! probes to work out which Enttec it is. This layer answers the USB half of
//! that; the widget protocol on top is stage 2.
//!
//! Stage 1 goal: log every control request and every bulk byte verbatim, so the
//! protocol is read off the wire rather than guessed from specs.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use esp_idf_svc::sys::{
    esp, tinyusb_config_t, tinyusb_driver_install, tud_control_status, tud_control_xfer,
    tud_vendor_n_available, tud_vendor_n_read, tud_vendor_n_write, tud_vendor_n_write_flush,
    tusb_control_request_t, tusb_desc_device_t, EspError,
};

/// FTDI's own VID, and the PID every FT232R/FT245R ships with. Not ours to use
/// for anything distributable — fine for a device on one desk.
const VID: u16 = 0x0403;
const PID: u16 = 0x6001;

/// FTDI vendor requests (values from Linux `ftdi_sio.h`).
mod sio {
    pub const RESET: u8 = 0;
    pub const SET_MODEM_CTRL: u8 = 1;
    pub const SET_FLOW_CTRL: u8 = 2;
    pub const SET_BAUDRATE: u8 = 3;
    pub const SET_DATA: u8 = 4;
    pub const GET_MODEM_STATUS: u8 = 5;
    pub const SET_EVENT_CHAR: u8 = 6;
    pub const SET_ERROR_CHAR: u8 = 7;
    pub const SET_LATENCY_TIMER: u8 = 9;
    pub const GET_LATENCY_TIMER: u8 = 0x0a;
    pub const SET_BITMODE: u8 = 0x0b;
    pub const READ_PINS: u8 = 0x0c;
    pub const READ_EEPROM: u8 = 0x90;
}

/// FTDI SET_DATA packs the line settings into wValue; bit 14 is the break bit.
/// rekordbox drives us as an Enttec Open DMX USB, which means raw DMX512 on the
/// serial link: 8N2 framing, and each frame delimited by asserting then clearing
/// BREAK. The break-clear edge is therefore the start-of-frame marker.
const SET_DATA_BREAK: u16 = 1 << 14;

/// Counts break-clear edges. The control callback runs on the USB task and the
/// reader runs on the main loop, so this is the whole synchronisation: a counter
/// the reader compares against, rather than a shared buffer needing a lock.
static BREAK_EDGES: AtomicU32 = AtomicU32::new(0);
static BREAK_ASSERTED: AtomicBool = AtomicBool::new(false);

/// A DMX frame is a start code plus up to 512 channels.
pub const DMX_FRAME_MAX: usize = 513;
/// Standard DMX512 dimmer data. The only byte in a frame whose value is known
/// in advance, so the only way to verify frame phase.
const DMX_NULL_START_CODE: u8 = 0x00;

/// Reassembles the bulk byte stream into DMX frames.
///
/// Bytes accumulate until a break-clear edge arrives, which means the frame just
/// finished and a new one is starting.
pub struct FrameAsm {
    buf: Vec<u8>,
    /// Newest complete frame, not a queue — a stale frame is never worth
    /// replaying when a fresher one exists.
    ready: Option<Vec<u8>>,
    last_edge: u32,
    pub frames: u64,
    pub resyncs: u64,
    /// Observed frame lengths. The wire format is supposed to be a fixed 513
    /// bytes; these exist to prove that rather than assume it.
    pub min_len: usize,
    pub max_len: usize,
    /// Frames dropped because index 0 was not the null start code, i.e. the
    /// phase was wrong.
    pub bad_start_code: u64,
    synced: bool,
    edges: u64,
}

impl FrameAsm {
    pub fn new() -> Self {
        FrameAsm {
            buf: Vec::with_capacity(DMX_FRAME_MAX),
            ready: None,
            last_edge: 0,
            frames: 0,
            resyncs: 0,
            min_len: usize::MAX,
            max_len: 0,
            bad_start_code: 0,
            synced: false,
            edges: 0,
        }
    }

    /// A DMX frame is a fixed 513 bytes, so the byte count is the delimiter.
    ///
    /// The BREAK edge is only for phase: it aligns the first frame and detects
    /// drift. It cannot delimit every frame, because it is recorded on the USB
    /// task while bytes are drained on the main loop — an edge seen mid-drain
    /// truncated 38% of frames, lengths scattered from 64 to 513.
    ///
    /// Ordering matters and is the whole reason [`check_phase`] runs before the
    /// drain. The endpoint FIFO is ordered, so once an edge is seen every byte
    /// still in it belongs to the *new* frame. Draining first instead attributes
    /// those bytes to the old frame and starts counting a packet late, which is
    /// a constant 64-byte rotation of every frame — invisible in the length and
    /// resync counters, and it put channel 3 at index 452.
    fn feed(&mut self, data: &[u8]) {
        if !self.synced {
            return; // Discard the partial frame we started mid-stream.
        }
        for &b in data {
            self.buf.push(b);
            if self.buf.len() == DMX_FRAME_MAX {
                self.complete();
            }
        }
    }

    fn complete(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        // The null start code is the one byte whose value we know a priori, so
        // it is the only available proof that the phase is right. A rotated
        // frame otherwise passes every other check.
        if self.buf[0] != DMX_NULL_START_CODE {
            self.bad_start_code += 1;
            self.synced = false;
            self.buf.clear();
            return;
        }
        self.frames += 1;
        let len = self.buf.len();
        self.min_len = self.min_len.min(len);
        self.max_len = self.max_len.max(len);
        self.ready = Some(std::mem::take(&mut self.buf));
        self.buf.reserve(DMX_FRAME_MAX);
    }

    /// Act on any BREAK edge. **Must run before draining the endpoint**, so that
    /// bytes still in the FIFO are attributed to the frame the edge starts.
    fn check_phase(&mut self) {
        let edge = BREAK_EDGES.load(Ordering::Relaxed);
        if edge == self.last_edge {
            return;
        }
        let missed = edge.wrapping_sub(self.last_edge);
        self.last_edge = edge;

        if !self.synced {
            // A frame starts here; everything still queued belongs to it.
            self.synced = true;
            self.buf.clear();
            return;
        }
        self.edges = self.edges.wrapping_add(missed as u64);
        // One frame per edge. Sustained divergence means the byte count slipped,
        // so re-align on the next edge rather than emitting rotated channels.
        let drift = self.edges as i64 - self.frames as i64;
        if drift.abs() > 2 {
            self.resyncs += 1;
            self.synced = false;
            self.buf.clear();
            self.edges = self.frames;
        }
    }

    /// The newest complete frame, if any. Index 0 is the start code; channel N
    /// (1-based) is at index N.
    fn take_ready(&mut self) -> Option<Vec<u8>> {
        self.ready.take()
    }

    /// Read a channel from a completed frame, 1-based as DMX numbers them.
    pub fn channel(frame: &[u8], n: usize) -> u8 {
        frame.get(n).copied().unwrap_or(0)
    }
}

impl Default for FrameAsm {
    fn default() -> Self {
        Self::new()
    }
}

/// Line status with both transmitter-empty bits set: nothing pending, no errors.
/// This is the second of the two bytes FTDI prefixes to every bulk IN.
pub const LINE_STATUS_IDLE: u8 = 0x60;
/// Modem status with CTS asserted, which is what a widget with no flow control
/// looks like.
pub const MODEM_STATUS_IDLE: u8 = 0x01;

static DEVICE_DESC: tusb_desc_device_t = tusb_desc_device_t {
    bLength: core::mem::size_of::<tusb_desc_device_t>() as u8,
    bDescriptorType: 0x01, // DEVICE
    bcdUSB: 0x0200,
    // FTDI reports a vendor-specific *interface*, not a vendor-specific device.
    bDeviceClass: 0x00,
    bDeviceSubClass: 0x00,
    bDeviceProtocol: 0x00,
    bMaxPacketSize0: 64,
    idVendor: VID,
    idProduct: PID,
    bcdDevice: 0x0600, // FT232R
    iManufacturer: 0x01,
    iProduct: 0x02,
    iSerialNumber: 0x03,
    bNumConfigurations: 0x01,
};

/// Configuration, one vendor-class interface, two bulk endpoints. Endpoint
/// numbers match a real FT232R: IN on 1, OUT on 2.
const EP_IN: u8 = 0x81;
const EP_OUT: u8 = 0x02;
const EP_SIZE: u16 = 64;

#[rustfmt::skip]
static CONFIG_DESC: [u8; 32] = [
    // Configuration: 32 bytes total, 1 interface, config #1, bus-powered, 100mA
    0x09, 0x02, 32, 0x00, 0x01, 0x01, 0x00, 0x80, 0x32,
    // Interface 0: class/subclass/protocol all 0xFF, 2 endpoints
    0x09, 0x04, 0x00, 0x00, 0x02, 0xFF, 0xFF, 0xFF, 0x00,
    // Bulk OUT
    0x07, 0x05, EP_OUT, 0x02, (EP_SIZE & 0xFF) as u8, (EP_SIZE >> 8) as u8, 0x00,
    // Bulk IN
    0x07, 0x05, EP_IN, 0x02, (EP_SIZE & 0xFF) as u8, (EP_SIZE >> 8) as u8, 0x00,
];

/// Index 0 is the language id (0x0409, US English) rather than a real string.
/// The rest mimic an FT232R; the serial is arbitrary but must look plausible,
/// since FTDI's own tooling can filter on it.
const STR_LANGID: &[u8] = b"\x09\x04\0";
const STR_MANUFACTURER: &[u8] = b"FTDI\0";
const STR_PRODUCT: &[u8] = b"FT232R USB UART\0";
const STR_SERIAL: &[u8] = b"A50285BI\0";

/// esp_tinyusb stores this array by pointer and reads it on every string
/// descriptor request (`descriptors_control.c`: `pstr_desc = config->string_descriptor`).
/// It must therefore outlive `install()` — a local array dangles and enumeration
/// fails with the device never appearing on the bus at all.
struct StringTable([*const core::ffi::c_char; 4]);
// Raw pointers aren't Sync, but these only ever point at immutable statics and
// are only read from the USB task.
unsafe impl Sync for StringTable {}

static STRINGS: StringTable = StringTable([
    STR_LANGID.as_ptr() as *const core::ffi::c_char,
    STR_MANUFACTURER.as_ptr() as *const core::ffi::c_char,
    STR_PRODUCT.as_ptr() as *const core::ffi::c_char,
    STR_SERIAL.as_ptr() as *const core::ffi::c_char,
]);

/// Install the USB device stack. After this the USB-Serial-JTAG console on this
/// port is gone — see `netlog`.
pub fn install() -> Result<(), EspError> {
    let mut cfg: tinyusb_config_t = unsafe { core::mem::zeroed() };
    cfg.__bindgen_anon_1.device_descriptor = &DEVICE_DESC;
    cfg.__bindgen_anon_2.__bindgen_anon_1.configuration_descriptor = CONFIG_DESC.as_ptr();
    cfg.string_descriptor = STRINGS.0.as_ptr() as *mut *const core::ffi::c_char;
    cfg.string_descriptor_count = STRINGS.0.len() as core::ffi::c_int;
    cfg.external_phy = false;
    cfg.self_powered = false;

    log::info!(
        "installing USB device: {VID:04x}:{PID:04x} \"FT232R USB UART\" serial A50285BI"
    );
    esp!(unsafe { tinyusb_driver_install(&cfg) })
}

/// Drain the host's bulk writes into DMX frames.
///
/// `status_due` gates the FTDI idle status bytes on the IN endpoint. Left off by
/// default: rekordbox drives this device output-only and never reads, so the
/// bytes only pile up in an endpoint nobody drains. It is also unsolicited
/// traffic to a host that has been crashing, which makes it the first thing
/// worth removing rather than the last.
pub fn pump(asm: &mut FrameAsm, status_due: &mut bool) -> Option<Vec<u8>> {
    // Before the drain, not after — see FrameAsm::feed.
    asm.check_phase();

    let mut buf = [0u8; 64];
    loop {
        if unsafe { tud_vendor_n_available(0) } == 0 {
            break;
        }
        let n =
            unsafe { tud_vendor_n_read(0, buf.as_mut_ptr() as *mut c_void, buf.len() as u32) };
        if n == 0 {
            break;
        }
        asm.feed(&buf[..n as usize]);
    }
    let frame = asm.take_ready();

    if *status_due {
        *status_due = false;
        let status = [MODEM_STATUS_IDLE, LINE_STATUS_IDLE];
        unsafe {
            tud_vendor_n_write(0, status.as_ptr() as *const c_void, status.len() as u32);
            tud_vendor_n_write_flush(0);
        }
    }

    frame
}

/// Vendor control requests. TinyUSB routes every `TUSB_REQ_TYPE_VENDOR` setup
/// packet here, which is exactly the FTDI SIO surface.
///
/// Stage 1 answers plausibly rather than correctly: the point is to keep the
/// host talking so the conversation shows up in the log.
#[no_mangle]
pub extern "C" fn tud_vendor_control_xfer_cb(
    rhport: u8,
    stage: u8,
    request: *const tusb_control_request_t,
) -> bool {
    // Only the setup stage carries a new request; data/ack stages just complete.
    const CONTROL_STAGE_SETUP: u8 = 1;
    if stage != CONTROL_STAGE_SETUP {
        return true;
    }
    let Some(req) = (unsafe { request.as_ref() }) else {
        return false;
    };
    // Copies, not references: the struct is `repr(packed)`, so borrowing a
    // field (which every format macro does) is E0793.
    let req_type = unsafe { req.__bindgen_anon_1.bmRequestType };
    let b_request = req.bRequest;
    let w_value = req.wValue;
    let w_index = req.wIndex;
    let w_length = req.wLength;

    // SET_DATA arrives twice per DMX frame at ~41Hz. Track the break edge and
    // stay silent; logging it would bury everything else.
    if b_request == sio::SET_DATA {
        let asserted = w_value & SET_DATA_BREAK != 0;
        let was = BREAK_ASSERTED.swap(asserted, Ordering::Relaxed);
        if was && !asserted {
            BREAK_EDGES.fetch_add(1, Ordering::Relaxed);
        }
        return unsafe { tud_control_status(rhport, request) };
    }

    let name = match b_request {
        sio::RESET => "SIO_RESET",
        sio::SET_MODEM_CTRL => "SET_MODEM_CTRL",
        sio::SET_FLOW_CTRL => "SET_FLOW_CTRL",
        sio::SET_BAUDRATE => "SET_BAUDRATE",
        sio::SET_DATA => "SET_DATA",
        sio::GET_MODEM_STATUS => "GET_MODEM_STATUS",
        sio::SET_EVENT_CHAR => "SET_EVENT_CHAR",
        sio::SET_ERROR_CHAR => "SET_ERROR_CHAR",
        sio::SET_LATENCY_TIMER => "SET_LATENCY_TIMER",
        sio::GET_LATENCY_TIMER => "GET_LATENCY_TIMER",
        sio::SET_BITMODE => "SET_BITMODE",
        sio::READ_PINS => "READ_PINS",
        sio::READ_EEPROM => "READ_EEPROM",
        // Stall rather than fake an ACK. Acknowledging a request we did not
        // actually service desynchronises the host's state machine, and a
        // confidently wrong answer is worse than an honest "unsupported".
        _ => {
            log::warn!(
                "ctrl UNSUPPORTED req=0x{b_request:02x} type=0x{req_type:02x} \
                 value=0x{w_value:04x} index=0x{w_index:04x} len={w_length} — stalling"
            );
            return false;
        }
    };
    log::info!(
        "ctrl {name} req=0x{:02x} type=0x{:02x} value=0x{:04x} index=0x{:04x} len={}",
        b_request, req_type, w_value, w_index, w_length
    );

    // Requests that return data need a body; the rest are just acknowledged.
    // These are constant per request, so they live as immutable statics — the
    // transfer is device-to-host, so TinyUSB only reads through the pointer.
    static MODEM: [u8; 2] = [MODEM_STATUS_IDLE, LINE_STATUS_IDLE];
    static LATENCY: [u8; 1] = [16];
    static PINS: [u8; 1] = [0];
    static EEPROM: [u8; 2] = [0, 0];

    let reply: Option<&'static [u8]> = match b_request {
        sio::GET_MODEM_STATUS => Some(&MODEM),
        sio::GET_LATENCY_TIMER => Some(&LATENCY),
        sio::READ_PINS => Some(&PINS),
        sio::READ_EEPROM => Some(&EEPROM),
        _ => None,
    };

    match reply {
        Some(bytes) => unsafe {
            tud_control_xfer(
                rhport,
                request,
                bytes.as_ptr() as *mut c_void,
                bytes.len() as u16,
            )
        },
        None => unsafe { tud_control_status(rhport, request) },
    }
}
