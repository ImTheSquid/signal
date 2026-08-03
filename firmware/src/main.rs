mod config;
mod dmx;
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
use crate::wsproto::{DeviceMsg, JsonFramer, LightsJson, ServerMsg, ServerMsgRaw};

const HEARTBEAT: Duration = Duration::from_secs(20);
const WIFI_CHECK: Duration = Duration::from_secs(10);
const IDLE_RESTART_PAUSE: Duration = Duration::from_millis(500);
const FW_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The built-in cycle, run natively from the main loop rather than as a Rhai
/// script.
///
/// It used to be a script with an infinite `loop`, which meant a Rhai engine —
/// measured at 96.7KB on 32-bit — stayed resident the entire time the light was
/// idle. That is exactly when a job arrives and needs a contiguous receive
/// transient (measured 14.1KB for a 3.4KB script), and it left ~33KB free
/// instead of ~144KB. On this target `panic-strategy = abort`, so a failed
/// allocation reboots the board; the job is then never acknowledged and the
/// server heals its history row to `lost`.
///
/// Admin-set idle scripts still run through the interpreter and are unaffected.
/// One-shot scripts (the common case) free the engine as soon as they return.
const IDLE_GREEN: u128 = 4000;
const IDLE_YELLOW: u128 = 1500;
const IDLE_RED: u128 = 5000;

/// Lamp state for the built-in cycle at `elapsed` into it. Changes at most every
/// 1.5s, so it never contends with the relay dwell limit.
fn builtin_idle_lamps(elapsed: Duration) -> (bool, bool, bool) {
    let period = IDLE_GREEN + IDLE_YELLOW + IDLE_RED;
    let t = elapsed.as_millis() % period;
    if t < IDLE_GREEN {
        (false, false, true)
    } else if t < IDLE_GREEN + IDLE_YELLOW {
        (false, true, false)
    } else {
        (true, false, false)
    }
}

/// A `job_done` that has not been handed to the server yet.
///
/// `send` drops the message if the socket is down or the write fails, and
/// `job_done` was sent exactly once. A single transient disconnect at the moment
/// a job ended therefore produced `running: "idle"` plus a history row that healed
/// to `lost` — with no reboot and no dependence on script size, which is why it is
/// an independent cause of the symptom we chased through the receive path.
struct PendingDone {
    id: String,
    result: &'static str,
    error: Option<String>,
    since: Instant,
    last_try: Option<Instant>,
}

/// How often to retry an unacknowledged `job_done`.
const DONE_RETRY: Duration = Duration::from_millis(500);
/// Stop retrying eventually; the server heals the row itself, and holding the
/// strings forever on this heap is worse than a `lost` entry.
const DONE_GIVE_UP: Duration = Duration::from_secs(300);

/// How the light is filling idle time.
enum Idle {
    /// The admin-set script is running through the interpreter.
    Script,
    /// The built-in cycle, driven from the main loop with no engine resident.
    Builtin { since: Instant },
}

pub enum AppEvent {
    WsConnected,
    /// Already parsed: the framer that reassembles fragments deserializes as it
    /// goes, so there is no second parse here.
    WsMsg(ServerMsg),
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
    /// Whether idle time is filled by the interpreter or the native cycle.
    idle: Idle,
    idle_restart_at: Option<Instant>,
    /// Set when a job ends, cleared only once the server has it.
    pending_done: Option<PendingDone>,
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

        match (&self.idle_script, self.idle_broken) {
            (Some(script), false) => {
                let script = script.clone();
                self.idle = Idle::Script;
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
            _ => {
                // Free the interpreter rather than leave 96.7KB resident for as
                // long as the light is idle.
                self.runner.stop();
                self.idle = Idle::Builtin {
                    since: Instant::now(),
                };
            }
        }
    }

    /// Try to hand a queued `job_done` to the server, rate-limited.
    ///
    /// Cleared only when the write succeeds. `client.send` returning Ok is not
    /// proof of delivery, so `hello` is used as the reconciliation point: if the
    /// server still names this job on reconnect, it never processed the result and
    /// the retry re-arms.
    fn flush_done(&mut self, client: &mut EspWebSocketClient<'_>) {
        let Some(done) = self.pending_done.as_mut() else {
            return;
        };
        if done.since.elapsed() > DONE_GIVE_UP {
            log::warn!("giving up on job_done for {}", done.id);
            self.pending_done = None;
            return;
        }
        if done.last_try.is_some_and(|at| at.elapsed() < DONE_RETRY) {
            return;
        }
        if !client.is_connected() {
            return;
        }
        done.last_try = Some(Instant::now());

        let msg = DeviceMsg::JobDone {
            id: &done.id,
            result: done.result,
            error: done.error.as_deref(),
        };
        if send(client, &msg) {
            self.pending_done = None;
        }
    }

    fn set_idle_script(&mut self, script: String) {
        self.idle_script = Some(script);
        self.idle_broken = false;
        // Restart unconditionally when idle, not only when the text changed.
        // The built-in cycle never ends on its own, so gating on `changed` meant
        // that once it was running, re-saving the *same* script was a permanent
        // no-op — and the admin form is prefilled from the server, so the natural
        // retry submits byte-identical text and could never recover.
        if self.running_id.is_none() {
            self.start_idle();
        }
    }

    fn handle(&mut self, event: AppEvent, client: &mut EspWebSocketClient<'_>) {
        match event {
            AppEvent::WsConnected => {
                log::info!("websocket connected");
                // The server sends `hello` for resync; nothing to do here.
            }
            AppEvent::WsMsg(msg) => self.handle_server_msg(msg),
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
                    self.pending_done = Some(PendingDone {
                        id: id.clone(),
                        result,
                        error: error.map(str::to_owned),
                        since: Instant::now(),
                        last_try: None,
                    });
                    self.flush_done(client);
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
                // The server deletes job:current when it records a result, so a
                // hello that no longer names our finished job proves it landed.
                if let Some(done) = self.pending_done.as_ref() {
                    let still_there = job.as_ref().is_some_and(|j| j.id == done.id);
                    if !still_there {
                        self.pending_done = None;
                    } else if let Some(d) = self.pending_done.as_mut() {
                        d.last_try = None; // re-arm: it never got through
                    }
                }
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
            ServerMsg::Idle { script } => self.set_idle_script(script),
        }
    }

    fn send_state(&self, client: &mut EspWebSocketClient<'_>) {
        let (r, y, g) = self.lights.get();
        let heap = unsafe { esp_idf_svc::sys::esp_get_free_heap_size() };
        let heap_block = unsafe {
            esp_idf_svc::sys::heap_caps_get_largest_free_block(
                esp_idf_svc::sys::MALLOC_CAP_8BIT,
            )
        } as u32;
        let (or_, oy, og) = self.lights.ops();
        let _ = send(
            client,
            &DeviceMsg::State {
                lights: LightsJson { r, y, g },
                running: self.running_id.as_deref().unwrap_or("idle"),
                heap,
                heap_block,
                ops: [or_, oy, og],
                fw: FW_VERSION,
            },
        );
    }
}

/// Feed received bytes through the framer and forward whatever completes.
fn deliver(
    framer: &std::sync::Mutex<JsonFramer<ServerMsgRaw>>,
    tx: &Sender<AppEvent>,
    bytes: &[u8],
) {
    let Ok(mut framer) = framer.lock() else { return };
    for msg in framer.push(bytes) {
        match msg.and_then(ServerMsg::try_from) {
            Ok(msg) => {
                let _ = tx.send(AppEvent::WsMsg(msg));
            }
            Err(e) => log::warn!("bad server message: {e}"),
        }
    }
}

/// Returns whether the frame was handed to the client. Callers that must not
/// lose a message (see `PendingDone`) keep it queued on `false`.
fn send(client: &mut EspWebSocketClient<'_>, msg: &DeviceMsg<'_>) -> bool {
    if !client.is_connected() {
        return false;
    }
    let payload = serde_json::to_string(msg).expect("serializing device message");
    match client.send(FrameType::Text(false), payload.as_bytes()) {
        Ok(_) => true,
        Err(e) => {
            log::warn!("ws send failed: {e}");
            false
        }
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
        Duration::from_millis(CONFIG.min_lamp_dwell_ms),
    )?);

    let (tx, rx): (Sender<AppEvent>, Receiver<AppEvent>) = mpsc::channel();

    let mut app = App {
        run_gen: 0,
        running_id: None,
        idle_script: None,
        idle_broken: false,
        idle: Idle::Builtin { since: Instant::now() },
        idle_restart_at: None,
        pending_done: None,
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
        // Messages arrive in receive-buffer-sized chunks with no length info, so
        // they have to be reframed before parsing. Mutex because the callback is
        // Fn, not FnMut.
        let framer: std::sync::Mutex<JsonFramer<ServerMsgRaw>> =
            std::sync::Mutex::new(JsonFramer::new());
        move |event: &Result<WebSocketEvent<'_>, esp_idf_svc::io::EspIOError>| {
            if let Ok(event) = event {
                match &event.event_type {
                    WebSocketEventType::Connected => {
                        // A fragment left over from the previous session would
                        // corrupt the first message of this one.
                        if let Ok(mut framer) = framer.lock() {
                            framer.reset();
                        }
                        let _ = tx.send(AppEvent::WsConnected);
                    }
                    // Drop a partial as soon as the link goes, not just on the
                    // next connect. A message cut in half by a network drop would
                    // otherwise be held for the whole dead-connection window, and
                    // a half-received 16KB job script is heap this device cannot
                    // spare — free heap is ~33KB while any script is running.
                    WebSocketEventType::Disconnected
                    | WebSocketEventType::Close(_)
                    | WebSocketEventType::Closed => {
                        if let Ok(mut framer) = framer.lock() {
                            if framer.is_partial() {
                                log::warn!("link closed mid-message; discarding partial");
                            }
                            framer.reset();
                        }
                    }
                    // Binary is the normal path: bytes arrive unvalidated, so a
                    // multi-byte character split across chunks survives. Text is
                    // still accepted so the server and firmware can be deployed
                    // in either order.
                    WebSocketEventType::Binary(bytes) => {
                        deliver(&framer, &tx, bytes);
                    }
                    WebSocketEventType::Text(text) => {
                        deliver(&framer, &tx, text.as_bytes());
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

        // Drive the built-in cycle. Cheap: it only writes when the state changes,
        // and it changes at most every 1.5s.
        if let Idle::Builtin { since } = app.idle {
            let want = builtin_idle_lamps(since.elapsed());
            if lights.get() != want {
                lights.set(want.0, want.1, want.2);
            }
        }

        app.flush_done(&mut client);

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
