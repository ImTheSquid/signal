//! Shared Rhai configuration for the traffic-light script environment.
//!
//! The server-side validator and the ESP32 firmware both build their engines
//! through this crate, so the language surface and safety limits cannot drift
//! between validation and execution.

pub use rhai;

use rhai::Engine;

/// Maximum size of the compiled artifact the device loads, enforced at the API
/// and bounded again by the websocket framer.
///
/// The real ceiling: `scripts/pulse.rhai` is 11182 source bytes and 4807 of
/// artifact, loading to roughly 5x that in heap. Against ~155KB free, less a
/// 32KB stack and the lazy engine, that leaves room for something like 20KB of
/// artifact — so this is a bound rather than a squeeze, and the device refuses
/// anything it cannot actually fit, with `heap_check` saying what it wanted.
pub const MAX_ARTIFACT_BYTES: usize = 16 * 1024;

/// Maximum size in bytes of a script as submitted. Bounds the work the
/// validator will do on one request.
pub const MAX_RAW_SCRIPT_BYTES: usize = 256 * 1024;

/// Callbacks that back the script API. The validator passes no-op stubs;
/// the firmware passes closures driving the relays.
pub struct Handlers {
    /// set_lights(red, yellow, green)
    pub set_lights: Box<dyn Fn(bool, bool, bool) + Send + Sync>,
    /// sleep(ms) — implementations must chunk and honor aborts
    pub sleep: Box<dyn Fn(i64) + Send + Sync>,
    /// millis() — monotonic milliseconds since boot/script start
    pub millis: Box<dyn Fn() -> i64 + Send + Sync>,
    /// dmx_recv(timeout_ms) — the newest frame received within the timeout.
    /// `Ok(None)` means nothing arrived; `Err` means the receiver itself is
    /// broken (a port that will not bind), which must surface as a script error
    /// rather than an endless silent timeout.
    ///
    /// Implementations must chunk and honor aborts like `sleep`, and must
    /// coalesce to the newest datagram rather than replaying a queue.
    ///
    /// Deliberately raw: thresholding and the channel-to-lamp mapping are the
    /// script's business, so they can change without reflashing anything.
    pub dmx_recv: Box<dyn Fn(i64) -> Result<Option<DmxFrame>, String> + Send + Sync>,
    /// A fresh uniformly-distributed u32 per call.
    ///
    /// Rhai ships no RNG, and the `no_time` pin leaves a script no clock to
    /// improvise one from, so without this every run of a pattern is identical —
    /// which is exactly what makes generated lighting read as mechanical.
    pub random_u32: Box<dyn Fn() -> u32 + Send + Sync>,
    /// The configured minimum relay dwell in ms.
    ///
    /// A script that asks for transitions faster than this gets throttled, which
    /// silently distorts its timing. Exposing the number lets one pace itself to
    /// the hardware it is actually running on instead of assuming.
    pub lamp_dwell_ms: Box<dyn Fn() -> i64 + Send + Sync>,
}

/// A frame from the UDP sender, as handed to a script.
pub struct DmxFrame {
    pub seq: i64,
    /// Identifies the sender, so a script can tell one apart from another
    /// without being modified. The audio bridge uses `0xFFFE`.
    pub base: i64,
    pub channels: Vec<u8>,
}

impl Handlers {
    /// No-op handlers for compile-only validation.
    pub fn stubs() -> Self {
        Handlers {
            set_lights: Box::new(|_, _, _| {}),
            sleep: Box::new(|_| {}),
            millis: Box::new(|| 0),
            dmx_recv: Box::new(|_| Ok(None)),
            // Deterministic for validation: a compile-only pass must not depend
            // on entropy. State is per-Handlers, not a process-wide static, so
            // two engines in one process get independent reproducible streams
            // instead of interleaving into an unpredictable one.
            random_u32: Box::new({
                let state = std::sync::atomic::AtomicU32::new(0x9E37_79B9);
                move || {
                    use std::sync::atomic::Ordering;
                    let mut x = state.load(Ordering::Relaxed);
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    state.store(x, Ordering::Relaxed);
                    x
                }
            }),
            lamp_dwell_ms: Box::new(|| 0),
        }
    }
}

/// Apply the sandbox limits. Parse-time limits (expression depth) also make
/// `Engine::compile` reject pathological inputs during validation.
pub fn apply_limits(engine: &mut Engine) {
    // No operation cap. A script running a beat grid forward burns 5M operations
    // in about ten minutes, so the cap ended shows rather
    // than protecting anything. What actually bounds a run is the job TTL (which
    // the lock's remaining time sets) and the abort flag; the progress callback
    // still yields for the watchdog on every engine, so an unbounded count
    // cannot starve the system. 0 means unlimited in rhai.
    engine.set_max_operations(0);
    engine.set_max_call_levels(16);
    engine.set_max_expr_depths(32, 16);
    engine.set_max_string_size(4 * 1024);
    engine.set_max_array_size(1024);
    // NB: 0 would mean "no limit" in rhai, not "no maps".
    engine.set_max_map_size(64);
    engine.disable_symbol("eval");
    engine.set_strict_variables(true);
}

/// Register the script-facing API.
pub fn register_api(engine: &mut Engine, handlers: Handlers) {
    let Handlers {
        set_lights,
        sleep,
        millis,
        dmx_recv,
        random_u32,
        lamp_dwell_ms,
    } = handlers;
    engine.register_fn("set_lights", move |r: bool, y: bool, g: bool| set_lights(r, y, g));

    // sleep_until needs both, so they are shared rather than moved.
    let sleep = std::sync::Arc::new(sleep);
    let millis = std::sync::Arc::new(millis);
    engine.register_fn("sleep", {
        let sleep = sleep.clone();
        move |ms: i64| sleep(ms)
    });
    engine.register_fn("millis", {
        let millis = millis.clone();
        move || millis()
    });
    // sleep_until(t) instead of sleep(period): a pattern built from relative
    // sleeps accumulates every delay the work in between cost — and set_lights
    // blocks for the relay dwell — so its period drifts long and it slides off
    // the beat. Against an absolute target the error cannot accumulate.
    engine.register_fn("sleep_until", {
        let sleep = sleep.clone();
        let millis = millis.clone();
        move |target_ms: i64| {
            let remaining = target_ms - millis();
            if remaining > 0 {
                sleep(remaining);
            }
        }
    });

    engine.register_fn("lamp_dwell_ms", move || lamp_dwell_ms());

    // Random, because a pattern that repeats identically reads as mechanical.
    // Two draws per value so the range math cannot overflow and a large range
    // still gets full resolution.
    let random_u32 = std::sync::Arc::new(random_u32);
    let random_u64 = {
        let random_u32 = random_u32.clone();
        move || ((random_u32() as u64) << 32) | random_u32() as u64
    };
    let random_u64 = std::sync::Arc::new(random_u64);
    // [0.0, 1.0). 24 bits, which is all an f32 mantissa holds.
    engine.register_fn("rand_float", {
        let random_u32 = random_u32.clone();
        move || -> rhai::FLOAT { (random_u32() >> 8) as rhai::FLOAT / 16_777_216.0 }
    });
    // Inclusive on both ends, which is what a script wants for channel or lamp
    // indices. Lemire's multiply-shift rather than a modulo, so the distribution
    // isn't skewed toward the low end of the range.
    engine.register_fn("rand_int", {
        let random_u64 = random_u64.clone();
        move |lo: i64, hi: i64| -> Result<i64, Box<rhai::EvalAltResult>> {
            if hi < lo {
                return Err(format!("rand_int: empty range {lo}..{hi}").into());
            }
            let span = (hi as i128 - lo as i128 + 1) as u128;
            let scaled = ((random_u64() as u128) * span) >> 64;
            Ok((lo as i128 + scaled as i128) as i64)
        }
    });
    // rand_chance(0.25) reads better at a call site than rand_float() < 0.25,
    // and clamps rather than surprising a script that computed p out of range.
    engine.register_fn("rand_chance", {
        let random_u32 = random_u32.clone();
        move |p: rhai::FLOAT| -> bool {
            (random_u32() >> 8) as rhai::FLOAT / 16_777_216.0 < p
        }
    });
    // Returns #{ ok, base, seq, ch }. `ch` holds raw 0-255 bytes and `base`
    // identifies the sender. On timeout `ok` is false and `ch` is
    // empty, so a script that ignores `ok` gets an index error rather than
    // silently acting on a stale or all-zero frame.
    engine.register_fn(
        "dmx_recv",
        move |timeout_ms: i64| -> Result<rhai::Map, Box<rhai::EvalAltResult>> {
            let frame = dmx_recv(timeout_ms).map_err(|e| -> Box<rhai::EvalAltResult> {
                format!("dmx_recv: {e}").into()
            })?;
            let mut map = rhai::Map::new();
            map.insert("ok".into(), frame.is_some().into());
            let (base, seq, channels) = match frame {
                Some(f) => (f.base, f.seq, f.channels),
                None => (0, 0, Vec::new()),
            };
            map.insert("base".into(), base.into());
            map.insert("seq".into(), seq.into());
            let ch: rhai::Array = channels.into_iter().map(|v| (v as i64).into()).collect();
            map.insert("ch".into(), ch.into());
            Ok(map)
        },
    );
}

/// Info about the last key holder, exposed to idle scripts.
pub struct LastHolder {
    pub name: String,
    /// "ok" | "error" | "aborted" | "deadline"
    pub result: String,
    pub ended_ms_ago: i64,
}

/// Extra API available ONLY in idle scripts. `get_last_holder()` returns
/// `#{ name, result, ended_ms_ago }` — empty name / -1 when nobody has held
/// the light since boot. User scripts calling it compile (function lookup is
/// runtime in Rhai) but fail on-device, which is the intended restriction.
pub fn register_idle_api(
    engine: &mut Engine,
    get_last_holder: Box<dyn Fn() -> Option<LastHolder> + Send + Sync>,
) {
    engine.register_fn("get_last_holder", move || -> rhai::Map {
        let mut map = rhai::Map::new();
        match get_last_holder() {
            Some(h) => {
                map.insert("name".into(), h.name.into());
                map.insert("result".into(), h.result.into());
                map.insert("ended_ms_ago".into(), h.ended_ms_ago.into());
            }
            None => {
                map.insert("name".into(), "".into());
                map.insert("result".into(), "".into());
                map.insert("ended_ms_ago".into(), (-1_i64).into());
            }
        }
        map
    });
}

/// An engine carrying the whole standard library, with the sandbox limits applied.
///
/// The single construction site for the language surface: the validator and the
/// firmware both come through here, so what validates and what runs cannot
/// drift apart.
///
/// The library is lazy: its functions stay in static manifests and are looked up
/// only when a script calls them, so carrying all of it costs a small module
/// rather than the ~96KB an eager `StandardPackage` takes on the device. That
/// module is built once and shared by pointer, so every run after the first pays
/// a refcount.
pub fn new_engine() -> Engine {
    use rhai::packages::{Package, StandardPackage};

    let mut engine = Engine::new_raw();

    // `Engine::new` turns this on and `new_raw` does not, so building up from
    // raw silently loses it. Without the interner every identifier and string
    // literal in a script allocates separately instead of being shared: measured
    // on a real job script, the AST goes 91648 -> 110368 bytes and 1303 -> 1879
    // allocations, and on the device each of those allocations also costs an
    // 8-byte heap header. Not optional on a board this tight.
    engine.set_max_strings_interned(1024);

    // `init_engine` is how a package registers custom operators or syntax;
    // calling it keeps this equivalent to `register_into_engine`.
    static STANDARD: std::sync::OnceLock<std::sync::Arc<rhai::Module>> =
        std::sync::OnceLock::new();
    <StandardPackage as Package>::init_engine(&mut engine);
    engine.register_global_module(
        STANDARD
            .get_or_init(|| StandardPackage::new_lazy().as_shared_module())
            .clone(),
    );

    apply_limits(&mut engine);
    engine
}

/// A script lowered for the device: the artifact it runs, and the position
/// table that stays behind.
///
/// Split because the table is only needed to turn a fault back into a line, and
/// that happens on the server. Sending it would put bytes on a device that has
/// no use for them — the same reason `JobSchema.map` never leaves the server.
pub struct Artifact {
    /// What the device loads and runs.
    pub program: Vec<u8>,
    /// Maps a program counter back to a position in the submitted source. Keep
    /// it beside the job; the device never sees it.
    pub sidecar: rhai::grain::Sidecar,
    /// Nodes the compiler could not lower, which stay AST fragments and run on
    /// rhai's walker. Zero for everything in this repo; a script that reports
    /// more is paying the tree's per-node allocation cost for that part.
    pub residual: usize,
}

/// Lower a compiled script to a device artifact.
///
/// The parser, the optimiser and the tree all stay here. What reaches the light
/// is a flat buffer it verifies and executes, which is the difference between
/// ~756 allocations and ~54 for `scripts/pulse.rhai`.
pub fn lower(ast: &rhai::AST) -> Result<Artifact, String> {
    let program = rhai::grain::Compiler::new().compile(ast);
    let residual = program.residual_count();
    let stripped = program
        .write_stripped()
        .map_err(|e| format!("cannot serialise the artifact: {e}"))?;
    Ok(Artifact {
        program: stripped.artifact,
        sidecar: stripped.sidecar,
        residual,
    })
}

/// Run an artifact on `engine`.
///
/// `bytes` is borrowed for the run rather than copied into an owned program:
/// the caller has to keep them anyway, and owning costs another ~6KB.
///
/// The artifact arrives over the network from a key holder, so it is verified
/// before it executes — `Program::read` is total over arbitrary bytes precisely
/// so that check is possible rather than assumed.
pub fn run_artifact(engine: &Engine, bytes: &[u8]) -> Result<(), Box<rhai::EvalAltResult>> {
    let program =
        rhai::grain::Program::read(bytes).map_err(|e| -> Box<rhai::EvalAltResult> {
            format!("artifact will not load: {e}").into()
        })?;
    program
        .verify()
        .map_err(|e| -> Box<rhai::EvalAltResult> { format!("artifact failed verification: {e:?}").into() })?;

    // Nothing here needs to read variables back out of the run, so `run` rather
    // than `run_with_scope` — it owns the scope and returns nothing.
    rhai::grain::Vm::new(engine).run(&program)
}

/// A fully configured engine with stub handlers, for validation.
///
/// The same engine the device runs, so what validates is what executes.
pub fn validation_engine() -> Engine {
    let mut engine = new_engine();
    register_api(&mut engine, Handlers::stubs());
    engine
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_valid_script() {
        let engine = validation_engine();
        assert!(engine
            .compile("let on = true; set_lights(on, false, false); sleep(100);")
            .is_ok());
    }

    #[test]
    fn rejects_syntax_error() {
        let engine = validation_engine();
        assert!(engine.compile("let x = ;").is_err());
    }

    #[test]
    fn rejects_undefined_variable_in_strict_mode() {
        let engine = validation_engine();
        assert!(engine.compile("set_lights(nope, false, false);").is_err());
    }

    /// Function calls resolve by signature at runtime, so unknown functions
    /// pass compilation — they surface on-device as job_done errors.
    #[test]
    fn unknown_function_compiles() {
        let engine = validation_engine();
        assert!(engine.compile("launch_missiles();").is_ok());
    }

    #[test]
    fn idle_api_reports_last_holder() {
        let mut engine = validation_engine();
        register_idle_api(
            &mut engine,
            Box::new(|| {
                Some(LastHolder {
                    name: "amy".into(),
                    result: "ok".into(),
                    ended_ms_ago: 1234,
                })
            }),
        );
        let out: String = engine
            .eval(r#"let h = get_last_holder(); h.name + ":" + h.result"#)
            .unwrap();
        assert_eq!(out, "amy:ok");
    }

    #[test]
    fn idle_api_empty_when_no_holder_yet() {
        let mut engine = validation_engine();
        register_idle_api(&mut engine, Box::new(|| None));
        let out: i64 = engine
            .eval(r#"let h = get_last_holder(); h.ended_ms_ago"#)
            .unwrap();
        assert_eq!(out, -1);
    }

    #[test]
    fn dmx_recv_reports_raw_channels() {
        let mut engine = Engine::new();
        apply_limits(&mut engine);
        register_api(
            &mut engine,
            Handlers {
                dmx_recv: Box::new(|_| {
                    Ok(Some(DmxFrame {
                        seq: 42,
                        base: 5,
                        channels: vec![255, 7, 128],
                    }))
                }),
                ..Handlers::stubs()
            },
        );
        let out: String = engine
            .eval(
                r#"let p = dmx_recv(50);
                   `${p.ok}:${p.base}:${p.seq}:${p.ch[0]},${p.ch[1]},${p.ch[2]}`"#,
            )
            .unwrap();
        assert_eq!(out, "true:5:42:255,7,128");
    }

    /// A receiver that cannot bind must fail the script, not time out forever —
    /// a silent no-op is indistinguishable from "the sender is idle".
    #[test]
    fn dmx_recv_bind_failure_is_a_script_error() {
        let mut engine = Engine::new();
        apply_limits(&mut engine);
        register_api(
            &mut engine,
            Handlers {
                dmx_recv: Box::new(|_| Err("address in use".into())),
                ..Handlers::stubs()
            },
        );
        let err = engine.eval::<rhai::Map>("dmx_recv(50)").unwrap_err();
        assert!(err.to_string().contains("address in use"), "{err}");
    }

    #[test]
    fn dmx_recv_timeout_is_not_ok_and_empty() {
        let engine = validation_engine();
        let out: String = engine
            .eval(r#"let p = dmx_recv(50); `${p.ok}:${p.ch.len()}`"#)
            .unwrap();
        assert_eq!(out, "false:0");
    }

    /// Floats are enabled as f32, which is what the Xtensa FPU accelerates —
    /// rhai's default f64 would be software-emulated on this hardware.
    #[test]
    fn float_math_is_available_and_single_precision() {
        let engine = validation_engine();
        assert_eq!(core::mem::size_of::<rhai::FLOAT>(), 4, "FLOAT must be f32");
        let out: rhai::FLOAT = engine.eval("let x = 255.0; x / 2.0").unwrap();
        assert!((out - 127.5).abs() < 0.001, "{out}");
    }

    /// Scaling a raw channel into a fraction is the reason floats are on.
    /// `to_float()` is the conversion — rhai has no `as float` cast.
    #[test]
    fn float_scaling_of_a_channel_works() {
        let mut engine = Engine::new();
        apply_limits(&mut engine);
        register_api(
            &mut engine,
            Handlers {
                dmx_recv: Box::new(|_| {
                    Ok(Some(DmxFrame {
                        seq: 1,
                        base: 1,
                        channels: vec![191],
                    }))
                }),
                ..Handlers::stubs()
            },
        );
        let level: rhai::FLOAT = engine
            .eval("let p = dmx_recv(50); p.ch[0].to_float() / 255.0")
            .unwrap();
        assert!((level - 0.749).abs() < 0.01, "{level}");
    }

    /// The shape the bridge will actually submit: rekordbox sends the fixture as
    /// RGB, and the script decides that blue drives the yellow lamp.
    #[test]
    fn dmx_loop_compiles() {
        let engine = validation_engine();
        assert!(engine
            .compile(
                "loop { let p = dmx_recv(50); if p.ok {                  set_lights(p.ch[0] >= 128, p.ch[2] >= 128, p.ch[1] >= 128); } }"
            )
            .is_ok());
    }

    #[test]
    fn rand_int_stays_in_range_and_covers_it() {
        let engine = validation_engine();
        let out: rhai::Array = engine
            // Under set_max_array_size, so this cannot just be a big sample.
            .eval("let a = []; for i in 0..900 { a.push(rand_int(1, 6)); } a")
            .unwrap();
        let vals: Vec<i64> = out.into_iter().map(|v| v.cast()).collect();
        assert!(vals.iter().all(|v| (1..=6).contains(v)), "out of range");
        for want in 1..=6 {
            assert!(vals.contains(&want), "never produced {want}");
        }
    }

    /// A single-value range must not be an error, and must not need special
    /// casing at the call site.
    #[test]
    fn rand_int_single_value_range() {
        let engine = validation_engine();
        assert_eq!(engine.eval::<i64>("rand_int(7, 7)").unwrap(), 7);
    }

    #[test]
    fn rand_int_rejects_empty_range() {
        let engine = validation_engine();
        let err = engine.eval::<i64>("rand_int(6, 1)").unwrap_err();
        assert!(err.to_string().contains("empty range"), "{err}");
    }

    /// Negative bounds are the case an unsigned intermediate gets wrong.
    #[test]
    fn rand_int_handles_negative_bounds() {
        let engine = validation_engine();
        let out: rhai::Array = engine
            .eval("let a = []; for i in 0..500 { a.push(rand_int(-5, -1)); } a")
            .unwrap();
        assert!(out
            .into_iter()
            .all(|v| (-5..=-1).contains(&v.cast::<i64>())));
    }

    /// The whole i64 range: the reason the range math goes through i128.
    #[test]
    fn rand_int_handles_full_range_without_overflow() {
        let engine = validation_engine();
        engine
            .eval::<i64>("rand_int(-9223372036854775808, 9223372036854775807)")
            .unwrap();
    }

    #[test]
    fn rand_float_is_a_unit_interval() {
        let engine = validation_engine();
        let out: rhai::Array = engine
            .eval("let a = []; for i in 0..1000 { a.push(rand_float()); } a")
            .unwrap();
        let vals: Vec<rhai::FLOAT> = out.into_iter().map(|v| v.cast()).collect();
        assert!(vals.iter().all(|v| (0.0..1.0).contains(v)), "outside [0,1)");
        let mean = vals.iter().sum::<rhai::FLOAT>() / vals.len() as rhai::FLOAT;
        assert!((mean - 0.5).abs() < 0.05, "mean {mean} looks non-uniform");
    }

    #[test]
    fn rand_chance_honors_its_probability() {
        let engine = validation_engine();
        let hits: i64 = engine
            .eval("let n = 0; for i in 0..2000 { if rand_chance(0.25) { n += 1; } } n")
            .unwrap();
        assert!((400..=600).contains(&hits), "{hits}/2000 for p=0.25");
    }

    /// Out-of-range probabilities must be total, not surprising: p<=0 never
    /// fires and p>=1 always does.
    #[test]
    fn rand_chance_clamps() {
        let engine = validation_engine();
        assert_eq!(
            engine
                .eval::<i64>("let n = 0; for i in 0..200 { if rand_chance(0.0) { n += 1; } } n")
                .unwrap(),
            0
        );
        assert_eq!(
            engine
                .eval::<i64>("let n = 0; for i in 0..200 { if rand_chance(1.0) { n += 1; } } n")
                .unwrap(),
            200
        );
    }

    #[test]
    fn lamp_dwell_is_visible_to_scripts() {
        let mut engine = Engine::new();
        apply_limits(&mut engine);
        register_api(
            &mut engine,
            Handlers {
                lamp_dwell_ms: Box::new(|| 120),
                ..Handlers::stubs()
            },
        );
        assert_eq!(engine.eval::<i64>("lamp_dwell_ms()").unwrap(), 120);
    }

    /// The point of sleep_until: it asks for the time *remaining* to an absolute
    /// target, so work already done is absorbed instead of added on.
    #[test]
    fn sleep_until_subtracts_elapsed_time() {
        use std::sync::atomic::{AtomicI64, Ordering};
        use std::sync::Arc;

        let clock = Arc::new(AtomicI64::new(0));
        let slept = Arc::new(std::sync::Mutex::new(Vec::new()));

        let mut engine = Engine::new();
        apply_limits(&mut engine);
        register_api(
            &mut engine,
            Handlers {
                millis: Box::new({
                    let clock = clock.clone();
                    move || clock.load(Ordering::SeqCst)
                }),
                sleep: Box::new({
                    let clock = clock.clone();
                    let slept = slept.clone();
                    move |ms| {
                        slept.lock().unwrap().push(ms);
                        clock.fetch_add(ms, Ordering::SeqCst);
                    }
                }),
                // 30ms of "work" per step, charged to the clock.
                set_lights: Box::new({
                    let clock = clock.clone();
                    move |_, _, _| {
                        clock.fetch_add(30, Ordering::SeqCst);
                    }
                }),
                ..Handlers::stubs()
            },
        );

        engine
            .run(
                "let t = 0;
                 for i in 0..4 {
                     t += 100;
                     set_lights(true, false, false);
                     sleep_until(t);
                 }",
            )
            .unwrap();

        // Four 100ms steps land at 400ms even though 120ms went to set_lights.
        assert_eq!(clock.load(Ordering::SeqCst), 400);
        assert_eq!(*slept.lock().unwrap(), vec![70, 70, 70, 70]);
    }

    /// Overrunning the target must not sleep negatively or wrap — it returns at
    /// once and the pattern catches up on the next step.
    #[test]
    fn sleep_until_in_the_past_does_not_sleep() {
        use std::sync::Arc;
        let slept = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut engine = Engine::new();
        apply_limits(&mut engine);
        register_api(
            &mut engine,
            Handlers {
                millis: Box::new(|| 500),
                sleep: Box::new({
                    let slept = slept.clone();
                    move |ms| slept.lock().unwrap().push(ms)
                }),
                ..Handlers::stubs()
            },
        );
        engine.run("sleep_until(200);").unwrap();
        assert!(slept.lock().unwrap().is_empty());
    }

    /// The operation cap is off deliberately; the TTL and abort flag bound a run.
    /// A busy loop that would have died at 5M operations must now survive.
    #[test]
    fn no_operation_cap() {
        let engine = validation_engine();
        let out: i64 = engine
            .eval("let n = 0; for i in 0..2000000 { n += 1; } n")
            .unwrap();
        assert_eq!(out, 2_000_000);
    }

    #[test]
    fn rejects_expr_depth_bomb() {
        let engine = validation_engine();
        let bomb = format!("let x = {}1{};", "(".repeat(200), ")".repeat(200));
        assert!(engine.compile(&bomb).is_err());
    }
}

#[cfg(test)]
mod surface_tests {
    use super::*;

    /// Exercises one feature from every package `Engine::new()` carries, so a
    /// gap between the lazy library and the surface it replaces shows up here
    /// rather than in somebody's script at 2am. Rhai resolves calls at run
    /// time, so these must be *run*, not just compiled.
    const SURFACE: &str = r#"
        let n = 7 + 3 * 2;              // arithmetic
        let ok = n > 5 && n != 0;       // logic
        let xs = [1, 2, 3];             // array
        xs.push(4);
        let m = #{ a: 1 };              // map
        let s = "ab" + "cd";            // string
        let t = s.sub_string(1, 2);     // more string
        let f = (2.0).sqrt();           // math
        let total = 0;
        for i in 0..3 { total += i; }   // iterator
        let b = n & 3;                  // bit field
        let ty = type_of(n);            // language core
        ok && xs.len() == 4 && m.a == 1 && t != "" && f > 0.0 && total == 3 && b >= 0 && ty != ""
    "#;

    #[test]
    fn the_lazy_library_covers_the_standard_surface() {
        let engine = validation_engine();
        let got: bool = engine
            .eval(SURFACE)
            .expect("the lazy library must run everything Engine::new() would");
        assert!(got);
    }

    /// The same surface, lowered to an artifact and run the way the device runs
    /// it, so the lazy lookup is exercised through the VM and not only the walker.
    #[test]
    fn the_surface_runs_as_an_artifact() {
        let engine = validation_engine();
        let artifact = lower(&engine.compile(SURFACE).unwrap()).unwrap();
        assert_eq!(artifact.residual, 0, "would fall back to the tree walker");
        run_artifact(&engine, &artifact.program).expect("artifact must run the full surface");
    }

    /// Every script gets the whole library without declaring anything: the
    /// functions that used to need a component resolve on first call.
    #[test]
    fn lazy_functions_resolve_on_call() {
        let engine = validation_engine();
        assert!(engine.eval::<bool>("(1).to_float() > 0.0").unwrap());
        assert!(engine.eval::<bool>("let b = blob(2); b.len() == 2").unwrap());
        assert!(engine.eval::<bool>("let x = 6; x.get_bit(1)").unwrap());
        engine.eval::<()>("print(1)").unwrap();
    }

    /// The light's default idle script, run the way the device runs it.
    #[test]
    fn the_idle_script_runs_as_an_artifact() {
        let src = "set_lights(false, false, false);";
        let artifact = lower(&validation_engine().compile(src).unwrap()).unwrap();
        assert_eq!(artifact.residual, 0, "idle would fall back to the tree walker");

        let engine = validation_engine();
        assert!(run_artifact(&engine, &artifact.program).is_ok());
        assert!(engine.eval::<()>(src).is_ok(), "source path must agree");
    }

    #[test]
    fn using_the_last_holder_works() {
        let mut engine = validation_engine();
        register_idle_api(
            &mut engine,
            Box::new(|| {
                Some(LastHolder {
                    name: "amy".into(),
                    result: "ok".into(),
                    ended_ms_ago: 1234,
                })
            }),
        );
        assert!(engine
            .eval::<bool>(r#"get_last_holder().name + ":" + get_last_holder().result == "amy:ok""#)
            .unwrap());
    }
}
