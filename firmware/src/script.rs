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

/// Heap the interpreter needs before the script's own AST, measured by
/// `crates/script-env/tests/footprint.rs` and halved for this 32-bit target.
const ENGINE_HEAP_BYTES: usize = 70 * 1024;
/// AST bytes per byte of minified source, from the same measurement, rounded up.
const AST_BYTES_PER_SOURCE_BYTE: usize = 8;
/// Room for the run itself — scope, values, and the DMX frames a script pulls in
/// while it works. Without it a script fits at startup and dies once busy.
const RUN_MARGIN_BYTES: usize = 8 * 1024;

/// Whether a script of this size can be run without exhausting the heap.
///
/// Rust aborts on allocation failure, and on this board abort reboots, so an
/// interpreter that runs out of memory takes the whole light down and the job
/// never reports back — it shows up as a `lost` history row and a lamp that
/// flickers on each boot. There is no way to catch that after the fact, so the
/// only protection is to decline beforehand.
///
/// Conservative on purpose: refusing a script that would have just fit costs a
/// legible error, while accepting one that does not costs a reboot cycle.
fn heap_check(script_len: usize) -> Result<(), String> {
    let free = unsafe { esp_idf_svc::sys::esp_get_free_heap_size() } as usize;
    let largest =
        unsafe { esp_idf_svc::sys::heap_caps_get_largest_free_block(esp_idf_svc::sys::MALLOC_CAP_8BIT) };
    let needed = SCRIPT_STACK_BYTES
        + ENGINE_HEAP_BYTES
        + script_len * AST_BYTES_PER_SOURCE_BYTE
        + RUN_MARGIN_BYTES;

    // The stack is one contiguous allocation, so free heap alone does not say it
    // can be had — this board idles with ~140KB free but a largest block of
    // ~108KB.
    if largest < SCRIPT_STACK_BYTES {
        return Err(format!(
            "device out of memory: largest free block is {largest} bytes, \
             the script stack needs {SCRIPT_STACK_BYTES}"
        ));
    }
    if free < needed {
        return Err(format!(
            "script too large for this device: {script_len} bytes needs about \
             {needed} bytes of heap, {free} free"
        ));
    }
    Ok(())
}
/// Abort latency bound: blocking script calls wake at least this often to
/// check the flag.
///
/// Also the poll granularity for `dmx_recv`, which is why it is 10ms and not
/// more: DMX arrives at ~41Hz (24ms/frame), so a 50ms wake could only ever see
/// every other frame — enough for a threshold, not enough for onset detection.
pub(crate) const SLEEP_CHUNK: Duration = Duration::from_millis(10);

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

    /// Stop the current script and wait until its thread exits, so at most one
    /// interpreter (and one script stack) ever exists.
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

        // Decline before allocating anything, so a script that cannot fit fails
        // as a run rather than as a reboot.
        let spawned = match heap_check(script.len()) {
            Err(e) => Err(std::io::Error::other(e)),
            Ok(()) => std::thread::Builder::new()
                .name("rhai".into())
                .stack_size(SCRIPT_STACK_BYTES)
                .spawn(move || {
                    let deadline = ttl.map(|t| Instant::now() + t);
                    let outcome =
                        run_script(script, kind, deadline, &abort, &lights, &last_holder);
                    let _ = tx.send(AppEvent::ScriptDone {
                        run_gen,
                        kind,
                        job_id,
                        holder,
                        outcome,
                    });
                }),
        };
        match spawned {
            Ok(handle) => self.handle = Some(handle),
            // Either the heap check declined or the spawn itself failed, both of
            // which are heap exhaustion. Never worth a panic-reset: report it
            // like a script failure so the main loop keeps its invariants.
            Err(e) => {
                log::error!("script not started: {e}");
                let _ = tx_on_fail.send(AppEvent::ScriptDone {
                    run_gen,
                    kind,
                    job_id: job_id_on_fail,
                    holder: holder_on_fail,
                    outcome: Outcome::Error(e.to_string()),
                });
            }
        }
    }
}

fn run_script(
    script: String,
    kind: RunKind,
    deadline: Option<Instant>,
    abort: &Arc<AtomicBool>,
    lights: &Arc<Lights>,
    last_holder: &SharedLastHolder,
) -> Outcome {
    // The full surface: a key holder's script may use anything the docs
    // promise, and which packages it needs is not knowable before it runs.
    let mut engine = script_env::new_engine(script_env::Components::all());
    if kind == RunKind::Idle {
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
                            // Apply it anyway. Returning early silently discarded
                            // the state the script asked for, so a killed script
                            // left the lamps mid-pattern — the dwell exists to
                            // pace the relays, not to veto the last write.
                            lights.set(r, y, g);
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
            // Hardware RNG. A register read, so it is cheap enough to call per
            // value and needs no seeding or state.
            random_u32: Box::new(|| unsafe { esp_idf_svc::sys::esp_random() }),
            lamp_dwell_ms: Box::new(|| CONFIG.min_lamp_dwell_ms as i64),
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

    // Compile first and drop the source before evaluating: `Engine::run` holds
    // both the text and the AST live for the whole run, and on this heap the
    // script's own bytes are worth handing back.
    let ast = match engine.compile(&script) {
        Ok(ast) => ast,
        Err(e) => return Outcome::Error(e.to_string()),
    };
    drop(script);

    match engine.run_ast(&ast) {
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
