use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use script_env::rhai::{Dynamic, EvalAltResult};
use script_env::Handlers;

use crate::config::CONFIG;
use crate::dmx::DmxSocket;
use crate::lights::Lights;
use crate::AppEvent;

/// Bound on first use, and only once — retrying a failed bind on every call
/// would hammer the port at the script's poll rate. The reason is kept so every
/// subsequent call can report it.
enum DmxState {
    Unbound,
    Bound(DmxSocket),
    Failed(String),
}

/// The last completed job's holder, shared with idle-script engines so
/// `get_last_holder()` can report it.
pub struct LastHolderInfo {
    pub name: String,
    pub result: String,
    pub ended: Instant,
}

pub type SharedLastHolder = Arc<Mutex<Option<LastHolderInfo>>>;

/// Rhai needs far more stack than the ESP-IDF pthread default — it's a
/// recursive interpreter. Sized to the engine's max_expr_depths/call_levels;
/// kept lean because this allocation exists whenever any script (incl. idle)
/// is running and the board lives close to the heap floor.
const SCRIPT_STACK_BYTES: usize = 32 * 1024;
/// Abort latency bound: blocking script calls wake at least this often to
/// check the flag.
pub(crate) const SLEEP_CHUNK: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RunKind {
    Job,
    Idle,
}

#[derive(Debug)]
pub enum Outcome {
    Ok,
    Error(String),
    Aborted,
    Deadline,
}

pub struct Runner {
    abort: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Runner {
    pub fn new() -> Self {
        Runner {
            abort: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }

    /// Ask the current script to stop without waiting for it.
    pub fn request_abort(&self) {
        self.abort.store(true, Ordering::SeqCst);
    }

    /// Stop the current script and wait until its thread exits, so at most
    /// one interpreter (and one 64K stack) ever exists.
    pub fn stop(&mut self) {
        self.request_abort();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// Run a script in a fresh thread; the outcome arrives on `tx` tagged
    /// with `run_gen` so the main loop can tell stale completions from current.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        &mut self,
        run_gen: u64,
        kind: RunKind,
        job_id: Option<String>,
        holder: Option<String>,
        script: String,
        ttl: Option<Duration>,
        lights: Arc<Lights>,
        last_holder: SharedLastHolder,
        tx: Sender<AppEvent>,
    ) {
        self.stop();
        let abort = Arc::new(AtomicBool::new(false));
        self.abort = abort.clone();

        let job_id_on_fail = job_id.clone();
        let holder_on_fail = holder.clone();
        let tx_on_fail = tx.clone();
        let spawned = std::thread::Builder::new()
            .name("rhai".into())
            .stack_size(SCRIPT_STACK_BYTES)
            .spawn(move || {
                let deadline = ttl.map(|t| Instant::now() + t);
                let outcome = run_script(&script, kind, deadline, &abort, &lights, &last_holder);
                let _ = tx.send(AppEvent::ScriptDone {
                    run_gen,
                    kind,
                    job_id,
                    holder,
                    outcome,
                });
            });
        match spawned {
            Ok(handle) => self.handle = Some(handle),
            // Usually heap exhaustion — never worth a panic-reset. Report it
            // like a script failure so the main loop keeps its invariants.
            Err(e) => {
                log::error!("script thread spawn failed: {e}");
                let _ = tx_on_fail.send(AppEvent::ScriptDone {
                    run_gen,
                    kind,
                    job_id: job_id_on_fail,
                    holder: holder_on_fail,
                    outcome: Outcome::Error(format!("device out of memory: {e}")),
                });
            }
        }
    }
}

fn run_script(
    script: &str,
    kind: RunKind,
    deadline: Option<Instant>,
    abort: &Arc<AtomicBool>,
    lights: &Arc<Lights>,
    last_holder: &SharedLastHolder,
) -> Outcome {
    let mut engine = script_env::rhai::Engine::new();
    script_env::apply_limits(&mut engine);
    if kind == RunKind::Idle {
        // The idle script runs once per idle transition and may loop forever
        // by design (admin-authored); the abort flag remains its kill switch.
        engine.set_max_operations(0);
        script_env::register_idle_api(&mut engine, {
            let last_holder = last_holder.clone();
            Box::new(move || {
                last_holder.lock().unwrap().as_ref().map(|h| script_env::LastHolder {
                    name: h.name.clone(),
                    result: h.result.clone(),
                    ended_ms_ago: h.ended.elapsed().as_millis() as i64,
                })
            })
        });
    }

    let start = Instant::now();
    script_env::register_api(
        &mut engine,
        Handlers {
            set_lights: Box::new({
                let lights = lights.clone();
                let abort = abort.clone();
                move |r, y, g| {
                    // Relays need dwell time between transitions. Make the
                    // script wait rather than drop the write, so the lamps
                    // still end up where it asked. Chunked like sleep() so a
                    // throttled script stays killable.
                    while let Some(until) = lights.ready_at(r, y, g) {
                        if abort.load(Ordering::SeqCst)
                            || deadline.is_some_and(|d| Instant::now() >= d)
                        {
                            return;
                        }
                        let now = Instant::now();
                        if until <= now {
                            break;
                        }
                        std::thread::sleep(SLEEP_CHUNK.min(until - now));
                    }
                    lights.set(r, y, g)
                }
            }),
            sleep: Box::new({
                let abort = abort.clone();
                move |ms| {
                    let until = Instant::now() + Duration::from_millis(ms.max(0) as u64);
                    while Instant::now() < until {
                        if abort.load(Ordering::SeqCst) {
                            return;
                        }
                        if deadline.is_some_and(|d| Instant::now() >= d) {
                            return;
                        }
                        std::thread::sleep(SLEEP_CHUNK.min(until - Instant::now()));
                    }
                }
            }),
            millis: Box::new(move || start.elapsed().as_millis() as i64),
            dmx_recv: Box::new({
                let abort = abort.clone();
                // Bound lazily so a script that never calls dmx_recv never
                // opens a port, and dropped with the run so the next one can
                // rebind. Mutex because Handlers is Fn, not FnMut.
                let state = Mutex::new(DmxState::Unbound);
                move |timeout_ms| {
                    let mut state = state.lock().unwrap();
                    if matches!(*state, DmxState::Unbound) {
                        *state = match DmxSocket::bind(CONFIG.dmx_port) {
                            Ok(socket) => DmxState::Bound(socket),
                            Err(e) => DmxState::Failed(format!(
                                "cannot bind udp/{}: {e}",
                                CONFIG.dmx_port
                            )),
                        };
                    }
                    let socket = match &mut *state {
                        DmxState::Bound(socket) => socket,
                        // Surface it every call: a script silently timing out
                        // forever is indistinguishable from an idle sender.
                        DmxState::Failed(why) => return Err(why.clone()),
                        DmxState::Unbound => unreachable!("just initialised"),
                    };
                    Ok(socket
                        .recv(
                            Duration::from_millis(timeout_ms.max(0) as u64),
                            SLEEP_CHUNK,
                            &|| {
                                abort.load(Ordering::SeqCst)
                                    || deadline.is_some_and(|d| Instant::now() >= d)
                            },
                        )
                        .map(|f| script_env::DmxFrame {
                            seq: f.seq as i64,
                            base: f.base as i64,
                            channels: f.channels,
                        }))
                }
            }),
        },
    );

    engine.on_progress({
        let abort = abort.clone();
        move |ops| {
            if abort.load(Ordering::SeqCst) {
                return Some(Dynamic::from("aborted"));
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Some(Dynamic::from("deadline"));
            }
            // Busy scripts never yield on their own; give the IDLE task
            // (and its watchdog) a breath now and then.
            if ops % 8192 == 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
            None
        }
    });

    match engine.run(script) {
        Ok(()) => Outcome::Ok,
        Err(e) => match *e {
            EvalAltResult::ErrorTerminated(token, _) => {
                if token.to_string() == "deadline" {
                    Outcome::Deadline
                } else {
                    Outcome::Aborted
                }
            }
            other => Outcome::Error(other.to_string()),
        },
    }
}
