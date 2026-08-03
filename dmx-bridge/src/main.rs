//! rekordbox → traffic light bridge.
//!
//! Stage 0: wifi and UDP logging only. The USB side comes next, and it takes
//! the console with it — see netlog.

mod config;
mod netlog;
mod remote;
mod usb;
mod wire;

use std::time::{Duration, Instant};

use anyhow::Result;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{BlockingWifi, ClientConfiguration, Configuration, EspWifi};

use crate::config::CONFIG;

const HEARTBEAT: Duration = Duration::from_secs(5);
/// How often a real FTDI puts its two status bytes on the IN endpoint when idle.
const FTDI_STATUS_INTERVAL: Duration = Duration::from_millis(16);
/// Whether to send them at all. Unsolicited traffic toward a host that has been
/// crashing is the first thing to drop, and an output-only DMX sender never
/// reads the IN endpoint anyway.
const FTDI_IDLE_STATUS: bool = false;

fn connect_wifi(wifi: &mut BlockingWifi<EspWifi<'static>>) -> Result<()> {
    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: CONFIG
            .wifi_ssid
            .try_into()
            .map_err(|_| anyhow::anyhow!("wifi_ssid too long"))?,
        password: CONFIG
            .wifi_pass
            .try_into()
            .map_err(|_| anyhow::anyhow!("wifi_pass too long"))?,
        ..Default::default()
    }))?;
    wifi.start()?;
    loop {
        match wifi.connect().and_then(|()| wifi.wait_netif_up()) {
            Ok(()) => return Ok(()),
            Err(e) => {
                log::warn!("wifi connect failed ({e}), retrying in 5s");
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    }
}

fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    netlog::init(log::LevelFilter::Info);

    if CONFIG.wifi_ssid.is_empty() {
        log::error!("wifi_ssid is empty — copy cfg.toml.example to cfg.toml");
    }

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    let mut wifi = BlockingWifi::wrap(
        EspWifi::new(peripherals.modem, sys_loop.clone(), Some(nvs))?,
        sys_loop,
    )?;
    connect_wifi(&mut wifi)?;

    // Power save adds 100ms+ latency spikes, which lands directly on the beat.
    // This board is bus-powered, so there is nothing to save it for.
    esp_idf_svc::sys::esp!(unsafe {
        esp_idf_svc::sys::esp_wifi_set_ps(esp_idf_svc::sys::wifi_ps_type_t_WIFI_PS_NONE)
    })?;

    let ip = wifi.wifi().sta_netif().get_ip_info()?;
    log::info!("wifi up: {ip:?}");

    match netlog::attach(CONFIG.log_host, CONFIG.log_port) {
        Ok(addr) => log::info!("log sink attached: {addr}"),
        Err(e) => log::error!("log sink failed: {e}"),
    }

    remote::spawn(CONFIG.reboot_port);

    // Hold off TinyUSB so USB-Serial-JTAG stays available long enough for
    // espflash to reset and reflash. Without this window every iteration needs
    // a physical BOOT+RESET, because the two share one PHY.
    if CONFIG.usb_start_delay_ms > 0 {
        log::info!(
            "USB starts in {}ms — reflash window is open now",
            CONFIG.usb_start_delay_ms
        );
        std::thread::sleep(Duration::from_millis(CONFIG.usb_start_delay_ms));
    }
    if let Err(e) = usb::install() {
        log::error!("usb install failed: {e}");
    }

    let base = CONFIG.dmx_base_channel as usize;
    let count = (CONFIG.dmx_channel_count as usize).min(wire::MAX_CHANNELS);
    let keepalive = Duration::from_millis(CONFIG.keepalive_ms);

    let mut sender = match wire::Sender::new(CONFIG.light_host, CONFIG.light_port) {
        Ok(s) => {
            log::info!("forwarding ch{}..{} to {}", base, base + count - 1, s.dest());
            Some(s)
        }
        Err(e) => {
            log::error!("cannot open sender to {}: {e}", CONFIG.light_host);
            None
        }
    };

    let boot = Instant::now();
    let mut asm = usb::FrameAsm::new();
    let mut last_wifi_check = Instant::now();
    let mut last_heartbeat = Instant::now();
    let mut last_status = Instant::now();
    let mut last_sent: Option<Vec<u8>> = None;
    let mut last_send = Instant::now();
    let mut frames_at_heartbeat = 0u64;
    let mut sent = 0u64;
    // (count of non-zero channels, first few as (channel, value))
    let mut last_scan: Option<(usize, Vec<(usize, u8)>)> = None;
    loop {
        // A real FTDI emits idle status bytes on the IN endpoint, but rekordbox
        // drives this output-only and never reads them. Off unless something
        // turns out to need it.
        let mut status_due = FTDI_IDLE_STATUS && last_status.elapsed() >= FTDI_STATUS_INTERVAL;
        if status_due {
            last_status = Instant::now();
        }

        if let Some(frame) = usb::pump(&mut asm, &mut status_due) {
            // Diagnostic: is the universe blank, or is the fixture somewhere
            // other than where we are looking? Reported in the heartbeat rather
            // than per frame. Index 0 is the DMX start code, so index N is
            // channel N.
            let nonzero: Vec<(usize, u8)> = frame
                .iter()
                .enumerate()
                .filter(|(i, &v)| *i > 0 && v != 0)
                .map(|(i, &v)| (i, v))
                .collect();
            last_scan = Some((nonzero.len(), nonzero.into_iter().take(6).collect()));

            let channels: Vec<u8> = (0..count)
                .map(|i| usb::FrameAsm::channel(&frame, base + i))
                .collect();

            // Send on change, plus a keepalive so silence means "bridge gone"
            // rather than "nothing happening".
            let changed = last_sent.as_deref() != Some(channels.as_slice());
            if changed || last_send.elapsed() >= keepalive {
                if let Some(tx) = sender.as_mut() {
                    match tx.send(base as u16, &channels) {
                        Ok(()) => sent += 1,
                        Err(e) => log::warn!("send failed: {e}"),
                    }
                }
                last_send = Instant::now();
                if changed {
                    log::info!("ch{base}+: {channels:?}");
                    last_sent = Some(channels);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(2));

        if last_heartbeat.elapsed() >= HEARTBEAT {
            let secs = last_heartbeat.elapsed().as_secs_f32();
            last_heartbeat = Instant::now();
            let fps = (asm.frames - frames_at_heartbeat) as f32 / secs;
            frames_at_heartbeat = asm.frames;
            let heap = unsafe { esp_idf_svc::sys::esp_get_free_heap_size() };
            log::info!(
                "alive: up={}s heap={heap} usb={} dmx={:.0}fps frames={} sent={} resyncs={} len={}..{} badstart={}",
                boot.elapsed().as_secs(),
                unsafe { esp_idf_svc::sys::tud_mounted() },
                fps,
                asm.frames,
                sent,
                asm.resyncs,
                if asm.min_len == usize::MAX { 0 } else { asm.min_len },
                asm.max_len,
                asm.bad_start_code
            );
            match &last_scan {
                Some((n, sample)) if *n > 0 => {
                    log::info!("universe: {n} non-zero channels, first: {sample:?}")
                }
                Some(_) => log::info!("universe: entirely blank (all 512 channels zero)"),
                None => {}
            }
        }

        if last_wifi_check.elapsed() >= Duration::from_secs(10) {
            last_wifi_check = Instant::now();
            if !wifi.is_connected().unwrap_or(false) {
                log::warn!("wifi down, reconnecting");
                let _ = wifi.wifi_mut().connect();
            }
        }
    }
}
