mod config;
mod lights;
mod script;
mod wsproto;

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sntp::{EspSntp, SyncStatus};
use esp_idf_svc::wifi::{BlockingWifi, ClientConfiguration, Configuration, EspWifi};
use esp_idf_svc::ws::client::{
    EspWebSocketClient, EspWebSocketClientConfig, WebSocketEvent, WebSocketEventType,
};
use esp_idf_svc::ws::FrameType;

use crate::config::CONFIG;
use crate::lights::Lights;
use crate::script::{LastHolderInfo, Outcome, RunKind, Runner, SharedLastHolder};
use crate::wsproto::{DeviceMsg, LightsJson, ServerMsg};

const HEARTBEAT: Duration = Duration::from_secs(20);
const WIFI_CHECK: Duration = Duration::from_secs(10);
const IDLE_RESTART_PAUSE: Duration = Duration::from_millis(500);
const FW_VERSION: &str = env!("CARGO_PKG_VERSION");

/// What the light does when nobody holds a lock and no idle script is set
/// (or the stored one is broken). Idle scripts run once per idle transition
/// with no operation cap — loop yourself if you want an animation.
const BUILTIN_IDLE: &str = r#"
loop {
    set_lights(false, false, true);
    sleep(4000);
    set_lights(false, true, false);
    sleep(1500);
    set_lights(true, false, false);
    sleep(5000);
}
"#;

pub enum AppEvent {
    WsConnected,
    WsText(String),
    ScriptDone {
        run_gen: u64,
        kind: RunKind,
        job_id: Option<String>,
        holder: Option<String>,
        outcome: Outcome,
    },
}

struct App {
    run_gen: u64,
    running_id: Option<String>,
    idle_script: Option<String>,
    idle_broken: bool,
    idle_restart_at: Option<Instant>,
    runner: Runner,
    lights: Arc<Lights>,
    last_holder: SharedLastHolder,
    tx: Sender<AppEvent>,
}

impl App {
    fn start_job(&mut self, id: String, holder: String, script: String, ttl_ms: u64) {
        if self.running_id.as_deref() == Some(id.as_str()) {
            return; // duplicate delivery (two server instances during recycle)
        }
        log::info!("starting job {id} for {holder} (ttl {ttl_ms}ms)");
        self.run_gen += 1;
        self.idle_restart_at = None;
        self.running_id = Some(id.clone());
        self.runner.start(
            self.run_gen,
            RunKind::Job,
            Some(id),
            Some(holder),
            script,
            Some(Duration::from_millis(ttl_ms)),
            self.lights.clone(),
            self.last_holder.clone(),
            self.tx.clone(),
        );
    }

    fn start_idle(&mut self) {
        self.run_gen += 1;
        self.idle_restart_at = None;
        self.running_id = None;
        let script = match (&self.idle_script, self.idle_broken) {
            (Some(script), false) => script.clone(),
            _ => BUILTIN_IDLE.to_string(),
        };
        self.runner.start(
            self.run_gen,
            RunKind::Idle,
            None,
            None,
            script,
            None,
            self.lights.clone(),
            self.last_holder.clone(),
            self.tx.clone(),
        );
    }

    fn set_idle_script(&mut self, script: String) {
        let changed = self.idle_script.as_deref() != Some(script.as_str());
        self.idle_script = Some(script);
        self.idle_broken = false;
        if changed && self.running_id.is_none() {
            self.start_idle();
        }
    }

    fn handle(&mut self, event: AppEvent, client: &mut EspWebSocketClient<'_>) {
        match event {
            AppEvent::WsConnected => {
                log::info!("websocket connected");
                // The server sends `hello` for resync; nothing to do here.
            }
            AppEvent::WsText(text) => match serde_json::from_str::<ServerMsg>(&text) {
                Ok(msg) => self.handle_server_msg(msg),
                Err(e) => log::warn!("unparseable server message: {e}"),
            },
            AppEvent::ScriptDone {
                run_gen,
                kind,
                job_id,
                holder,
                outcome,
            } => {
                log::info!("script done ({kind:?}): {outcome:?}");
                if let Some(id) = job_id {
                    let (result, error) = match &outcome {
                        Outcome::Ok => ("ok", None),
                        Outcome::Error(e) => ("error", Some(e.as_str())),
                        Outcome::Aborted => ("aborted", None),
                        Outcome::Deadline => ("deadline", None),
                    };
                    send(client, &DeviceMsg::JobDone { id: &id, result, error });
                    *self.last_holder.lock().unwrap() = Some(LastHolderInfo {
                        name: holder.unwrap_or_default(),
                        result: result.to_string(),
                        ended: Instant::now(),
                    });
                }
                if kind == RunKind::Idle && matches!(outcome, Outcome::Error(_)) {
                    log::warn!("idle script failed; falling back to built-in cycle");
                    self.idle_broken = true;
                }
                if run_gen == self.run_gen {
                    // The active run ended on its own (not superseded).
                    match (kind, &outcome) {
                        // One-shot idle scripts run once per idle transition;
                        // the lamps hold whatever they set.
                        (RunKind::Idle, Outcome::Ok) => {}
                        // A failed idle script must not freeze the light —
                        // restart (now marked broken → built-in cycle).
                        (RunKind::Idle, _) => {
                            self.idle_restart_at = Some(Instant::now() + IDLE_RESTART_PAUSE);
                            self.running_id = None;
                        }
                        (RunKind::Job, _) => self.start_idle(),
                    }
                }
            }
        }
    }

    fn handle_server_msg(&mut self, msg: ServerMsg) {
        match msg {
            ServerMsg::Hello { job, idle } => {
                if let Some(idle) = idle {
                    self.set_idle_script(idle.script);
                }
                match job {
                    Some(job) => self.start_job(job.id, job.holder, job.script, job.ttl_ms),
                    None => {
                        if self.running_id.is_some() {
                            // Lock is gone (expired/released while we were away).
                            self.runner.request_abort();
                        }
                    }
                }
            }
            ServerMsg::Job {
                id,
                holder,
                script,
                ttl_ms,
            } => self.start_job(id, holder, script, ttl_ms),
            ServerMsg::Abort => {
                if self.running_id.is_some() {
                    self.runner.request_abort();
                }
            }
            ServerMsg::Idle { script, .. } => self.set_idle_script(script),
        }
    }

    fn send_state(&self, client: &mut EspWebSocketClient<'_>) {
        let (r, y, g) = self.lights.get();
        let heap = unsafe { esp_idf_svc::sys::esp_get_free_heap_size() };
        send(
            client,
            &DeviceMsg::State {
                lights: LightsJson { r, y, g },
                running: self.running_id.as_deref().unwrap_or("idle"),
                heap,
                fw: FW_VERSION,
            },
        );
    }
}

fn send(client: &mut EspWebSocketClient<'_>, msg: &DeviceMsg<'_>) {
    if !client.is_connected() {
        return;
    }
    let payload = serde_json::to_string(msg).expect("serializing device message");
    if let Err(e) = client.send(FrameType::Text(false), payload.as_bytes()) {
        log::warn!("ws send failed: {e}");
    }
}

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
            Ok(()) => {
                log::info!("wifi up: {:?}", wifi.wifi().sta_netif().get_ip_info()?);
                return Ok(());
            }
            Err(e) => {
                log::warn!("wifi connect failed ({e}), retrying in 5s");
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    }
}

fn wait_for_sntp() {
    // TLS cert validation needs wall-clock time; plain ws:// does not.
    if !CONFIG.ws_url.starts_with("wss://") {
        return;
    }
    let sntp = match EspSntp::new_default() {
        Ok(sntp) => sntp,
        Err(e) => {
            log::warn!("sntp init failed: {e}");
            return;
        }
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while sntp.get_sync_status() != SyncStatus::Completed && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
    }
    log::info!("sntp status: {:?}", sntp.get_sync_status());
    // Keep SNTP running for periodic resync.
    std::mem::forget(sntp);
}

fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    // Relays on non-strap pins, driven to "off" immediately.
    let lights = Arc::new(Lights::new(
        peripherals.pins.gpio32.degrade_output(),
        peripherals.pins.gpio33.degrade_output(),
        peripherals.pins.gpio25.degrade_output(),
        CONFIG.active_low,
    )?);

    let (tx, rx): (Sender<AppEvent>, Receiver<AppEvent>) = mpsc::channel();

    let mut app = App {
        run_gen: 0,
        running_id: None,
        idle_script: None,
        idle_broken: false,
        idle_restart_at: None,
        runner: Runner::new(),
        lights: lights.clone(),
        last_holder: SharedLastHolder::default(),
        tx: tx.clone(),
    };
    // Show the built-in cycle from boot, before the network is even up.
    app.start_idle();

    let mut wifi = BlockingWifi::wrap(
        EspWifi::new(peripherals.modem, sys_loop.clone(), Some(nvs))?,
        sys_loop,
    )?;
    connect_wifi(&mut wifi)?;
    wait_for_sntp();

    let headers = format!("Authorization: Bearer {}\r\n", CONFIG.device_token);
    let ws_config = EspWebSocketClientConfig {
        headers: Some(&headers),
        crt_bundle_attach: Some(esp_idf_svc::sys::esp_crt_bundle_attach),
        // TLS handshakes run on the client's own task; the default stack is tight.
        task_stack: 10 * 1024,
        ..Default::default()
    };
    let mut client = EspWebSocketClient::new(CONFIG.ws_url, &ws_config, Duration::from_secs(10), {
        let tx = tx.clone();
        move |event: &Result<WebSocketEvent<'_>, esp_idf_svc::io::EspIOError>| {
            if let Ok(event) = event {
                match &event.event_type {
                    WebSocketEventType::Connected => {
                        let _ = tx.send(AppEvent::WsConnected);
                    }
                    WebSocketEventType::Text(text) => {
                        let _ = tx.send(AppEvent::WsText(text.to_string()));
                    }
                    _ => {}
                }
            }
        }
    })?;

    let mut last_heartbeat = Instant::now();
    let mut last_wifi_check = Instant::now();

    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(event) => app.handle(event, &mut client),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => unreachable!("app holds a sender"),
        }

        if app.idle_restart_at.is_some_and(|at| Instant::now() >= at) {
            app.start_idle();
        }

        if lights.take_dirty() || last_heartbeat.elapsed() >= HEARTBEAT {
            app.send_state(&mut client);
            last_heartbeat = Instant::now();
        }

        if last_wifi_check.elapsed() >= WIFI_CHECK {
            last_wifi_check = Instant::now();
            if !wifi.is_connected().unwrap_or(false) {
                log::warn!("wifi down, reconnecting");
                let _ = wifi.wifi_mut().connect();
            }
        }
    }
}
