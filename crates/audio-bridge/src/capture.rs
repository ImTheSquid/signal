//! CoreAudio input, and the handoff to the analysis thread.
//!
//! The callback runs on a realtime thread: it must not allocate, lock, or log.
//! All it does is downmix to mono, push into a lock-free ring, and stamp a
//! timestamp the supervisor uses to notice the stream has died.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// Preferred in this order. Nothing resamples, so whichever is accepted becomes
/// the time base for every downstream constant.
const PREFERRED_RATES: [u32; 2] = [48_000, 44_100];

/// 5.33ms at 48kHz. Small enough that the input buffer is a rounding error in
/// the latency budget, large enough not to drown the callback in overhead.
const FRAMES_PER_BUFFER: u32 = 256;

/// Below this, the two legs are fighting rather than agreeing and summing them
/// cancels the centred content — which is exactly where the kick lives.
const CORRELATION_FLOOR: f32 = -0.3;

/// Samples accumulated before the correlation verdict is trusted — roughly
/// five seconds, long enough that a single centred-bass passage cannot swing it.
const CORRELATION_WINDOW: usize = 240_000;

pub struct DeviceInfo {
    pub name: String,
    pub channels: u16,
    pub default_rate: u32,
    pub supported_rates: Vec<u32>,
}

/// Every input device CoreAudio is currently offering.
///
/// Worth having as a subcommand rather than a log line: a driver that is
/// installed on disk but not loaded (BlackHole before its reboot) is invisible
/// here, and that is the single most confusing failure this daemon has.
pub fn list_inputs() -> Result<Vec<DeviceInfo>> {
    let host = cpal::default_host();
    let mut out = Vec::new();
    for device in host.input_devices().context("enumerating input devices")? {
        let name = device.name().unwrap_or_else(|_| "<unnamed>".into());
        let Ok(default) = device.default_input_config() else {
            continue;
        };
        let mut supported_rates = Vec::new();
        if let Ok(configs) = device.supported_input_configs() {
            for c in configs {
                for rate in PREFERRED_RATES {
                    if (c.min_sample_rate().0..=c.max_sample_rate().0).contains(&rate)
                        && !supported_rates.contains(&rate)
                    {
                        supported_rates.push(rate);
                    }
                }
            }
        }
        out.push(DeviceInfo {
            name,
            channels: default.channels(),
            default_rate: default.sample_rate().0,
            supported_rates,
        });
    }
    Ok(out)
}

/// Find the capture device.
///
/// Deliberately never falls back to the default input. The default is whatever
/// was last chosen in System Settings, and when that is the built-in mic the
/// daemon still produces a plausible-looking signal from room sound — a tracker
/// that half-works is far worse to diagnose than one that refuses to start.
pub fn pick_input(hint: Option<&str>) -> Result<cpal::Device> {
    let host = cpal::default_host();
    let devices: Vec<_> = host
        .input_devices()
        .context("enumerating input devices")?
        .collect();

    let matches = |name: &str| -> bool {
        match hint {
            Some(h) => name.eq_ignore_ascii_case(h),
            None => name.to_ascii_lowercase().starts_with("blackhole"),
        }
    };

    for device in &devices {
        if device.name().is_ok_and(|n| matches(&n)) {
            return Ok(device.clone());
        }
    }

    let available: Vec<String> = devices
        .iter()
        .filter_map(|d| d.name().ok())
        .map(|n| format!("  {n}"))
        .collect();
    let wanted = hint.unwrap_or("a device whose name starts with \"BlackHole\"");
    bail!(
        "no input device matching {wanted}.\navailable inputs:\n{}\n\n\
         If BlackHole was just installed, it needs a reboot before CoreAudio \
         offers it.",
        if available.is_empty() {
            "  (none)".to_string()
        } else {
            available.join("\n")
        }
    )
}

/// Pick a stream config, preferring a rate we have constants for.
///
/// Never resamples — everything downstream is parameterised on the rate that
/// comes back, so 44.1k just shifts the hop duration slightly.
pub fn choose_config(device: &cpal::Device) -> Result<cpal::StreamConfig> {
    let default = device
        .default_input_config()
        .context("querying default input config")?;

    let mut rate = default.sample_rate().0;
    if let Ok(configs) = device.supported_input_configs() {
        let ranges: Vec<_> = configs.collect();
        for preferred in PREFERRED_RATES {
            if ranges
                .iter()
                .any(|c| (c.min_sample_rate().0..=c.max_sample_rate().0).contains(&preferred))
            {
                rate = preferred;
                break;
            }
        }
    }

    Ok(cpal::StreamConfig {
        channels: default.channels(),
        sample_rate: cpal::SampleRate(rate),
        buffer_size: cpal::BufferSize::Fixed(FRAMES_PER_BUFFER),
    })
}

/// Health shared between the callback and the supervisor.
pub struct Health {
    /// Micros since process start, stamped every callback. Staleness is the
    /// only reliable signal that a CoreAudio stream has stopped: a device-side
    /// sample rate change often just makes callbacks cease without an error.
    last_callback_us: AtomicU64,
    errored: AtomicBool,
    /// Set when the correlation guard has switched to left-only.
    split_legs: AtomicBool,
    origin: Instant,
}

impl Health {
    fn new() -> Self {
        Health {
            last_callback_us: AtomicU64::new(0),
            errored: AtomicBool::new(false),
            split_legs: AtomicBool::new(false),
            origin: Instant::now(),
        }
    }

    fn stamp(&self) {
        let us = self.origin.elapsed().as_micros() as u64;
        self.last_callback_us.store(us, Ordering::Relaxed);
    }

    pub fn errored(&self) -> bool {
        self.errored.load(Ordering::Relaxed)
    }

    pub fn split_legs(&self) -> bool {
        self.split_legs.load(Ordering::Relaxed)
    }

    /// None when no callback has fired yet.
    pub fn since_last_callback(&self) -> Option<Duration> {
        match self.last_callback_us.load(Ordering::Relaxed) {
            0 => None,
            us => Some(self.origin.elapsed().saturating_sub(Duration::from_micros(us))),
        }
    }
}

pub struct Capture {
    /// Dropping this stops the stream. cpal's macOS Stream is not Send, so it
    /// has to stay on the thread that built it.
    _stream: cpal::Stream,
    pub sample_rate: u32,
    pub samples: rtrb::Consumer<f32>,
    pub health: Arc<Health>,
}

/// Running estimate of how much the two legs agree, used to catch a clone whose
/// polarity is flipped on one side. Summing those annihilates centred content.
#[derive(Default)]
struct Correlation {
    sum_lr: f64,
    sum_ll: f64,
    sum_rr: f64,
    n: usize,
}

impl Correlation {
    /// Returns true once it is confident the legs are fighting.
    fn observe(&mut self, l: f32, r: f32) -> bool {
        self.sum_lr += (l * r) as f64;
        self.sum_ll += (l * l) as f64;
        self.sum_rr += (r * r) as f64;
        self.n += 1;
        if self.n < CORRELATION_WINDOW {
            return false;
        }
        let denom = (self.sum_ll * self.sum_rr).sqrt();
        // Near-silence has no polarity to speak of; leave the verdict alone.
        let verdict = denom > 1e-9 && (self.sum_lr / denom) < CORRELATION_FLOOR as f64;
        *self = Correlation::default();
        verdict
    }
}

/// Start capture, falling back to CoreAudio's own buffer size if the device
/// refuses a fixed one. Nothing downstream depends on the buffer length — it
/// only shifts a few milliseconds of latency — so the fallback is free.
pub fn start(device: &cpal::Device, config: &cpal::StreamConfig) -> Result<Capture> {
    match try_start(device, config) {
        Ok(capture) => Ok(capture),
        Err(fixed_err) if matches!(config.buffer_size, cpal::BufferSize::Fixed(_)) => {
            eprintln!("fixed buffer size rejected ({fixed_err:#}); using the device default");
            try_start(
                device,
                &cpal::StreamConfig {
                    buffer_size: cpal::BufferSize::Default,
                    ..config.clone()
                },
            )
        }
        Err(e) => Err(e),
    }
}

fn try_start(device: &cpal::Device, config: &cpal::StreamConfig) -> Result<Capture> {
    let channels = config.channels as usize;
    if channels == 0 {
        bail!("device reports zero input channels");
    }

    // One second of audio. The analysis thread drains far faster than this;
    // the slack only has to cover scheduling jitter.
    let (mut tx, samples) = rtrb::RingBuffer::<f32>::new(config.sample_rate.0 as usize);

    let health = Arc::new(Health::new());
    let cb_health = health.clone();
    let err_health = health.clone();

    let mut correlation = Correlation::default();
    let mut left_only = false;

    let stream = device
        .build_input_stream(
            config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                cb_health.stamp();
                for frame in data.chunks(channels) {
                    let mono = match (channels, left_only) {
                        (1, _) => frame[0],
                        (_, true) => frame[0],
                        (_, false) => {
                            if correlation.observe(frame[0], frame[1]) {
                                left_only = true;
                                cb_health.split_legs.store(true, Ordering::Relaxed);
                            }
                            0.5 * (frame[0] + frame[1])
                        }
                    };
                    // A full ring means the analysis thread has stalled. Drop
                    // rather than block — blocking here would glitch the whole
                    // machine's audio.
                    let _ = tx.push(mono);
                }
            },
            move |err| {
                err_health.errored.store(true, Ordering::Relaxed);
                eprintln!("audio stream error: {err}");
            },
            None,
        )
        .context("building input stream")?;

    stream.play().context("starting input stream")?;

    Ok(Capture {
        _stream: stream,
        sample_rate: config.sample_rate.0,
        samples,
        health,
    })
}

/// Wall-clock milliseconds, used only to seed the wire sequence number so a
/// daemon restart does not collide with the light's replay filter.
pub fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
