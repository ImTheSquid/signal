use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use script_env::rhai::{Dynamic, EvalAltResult};
use script_env::Handlers;

use crate::lights::Lights;
use crate::AppEvent;

/// Rhai needs far more stack than the ESP-IDF pthread default — it's a
/// recursive interpreter.
const SCRIPT_STACK_BYTES: usize = 64 * 1024;
/// Abort latency bound: sleep() wakes at least this often to check the flag.
const SLEEP_CHUNK: Duration = Duration::from_millis(50);

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
    pub fn start(
        &mut self,
        run_gen: u64,
        kind: RunKind,
        job_id: Option<String>,
        script: String,
        ttl: Option<Duration>,
        lights: Arc<Lights>,
        tx: Sender<AppEvent>,
    ) {
        self.stop();
        let abort = Arc::new(AtomicBool::new(false));
        self.abort = abort.clone();

        let handle = std::thread::Builder::new()
            .name("rhai".into())
            .stack_size(SCRIPT_STACK_BYTES)
            .spawn(move || {
                let deadline = ttl.map(|t| Instant::now() + t);
                let outcome = run_script(&script, deadline, &abort, &lights);
                let _ = tx.send(AppEvent::ScriptDone {
                    run_gen,
                    kind,
                    job_id,
                    outcome,
                });
            })
            .expect("spawning script thread");
        self.handle = Some(handle);
    }
}

fn run_script(
    script: &str,
    deadline: Option<Instant>,
    abort: &Arc<AtomicBool>,
    lights: &Arc<Lights>,
) -> Outcome {
    let mut engine = script_env::rhai::Engine::new();
    script_env::apply_limits(&mut engine);

    let start = Instant::now();
    script_env::register_api(
        &mut engine,
        Handlers {
            set_lights: Box::new({
                let lights = lights.clone();
                move |r, y, g| lights.set(r, y, g)
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
