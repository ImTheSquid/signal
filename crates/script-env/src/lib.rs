//! Shared Rhai configuration for the traffic-light script environment.
//!
//! The server-side validator and the ESP32 firmware both build their engines
//! through this crate, so the language surface and safety limits cannot drift
//! between validation and execution.

pub use rhai;

use rhai::Engine;

/// Maximum size in bytes of the script that reaches the device, measured after
/// minification. Enforced at the API and bounded again by the websocket framer.
///
/// This is the *best case*: what fits when a script declares the narrowest set
/// of [`Components`] it can. A script that declares nothing carries rhai's whole
/// standard library and fits about 5KB, so this limit passing does not mean the
/// device will take it — `heap_check` in the firmware gives the real answer, in
/// a message that says how much was needed and how much was free.
///
/// This bounds the wire and the work one request can ask of the validator. It
/// is **not** the device's constraint any more: the light loads an artifact
/// rather than parsing this, so [`MAX_ARTIFACT_BYTES`] is what its memory is
/// about, and `heap_check` in the firmware makes the real decision against the
/// heap it actually has and says what it wanted.
///
/// It was briefly 4KB, sized from what a *tree* cost — 24 device bytes per
/// source byte, which is what made a 5KB script impossible.
pub const MAX_SCRIPT_BYTES: usize = 16 * 1024;

/// Maximum size of the compiled artifact the device loads.
///
/// The real ceiling, and much further out than the source one it replaces:
/// `scripts/follow.rhai` is 5054 minified bytes and 8719 of artifact, so about
/// 1.7 bytes of artifact per source byte, loading to roughly 5x that in heap.
/// Against ~155KB free, less a 32KB stack and a declared engine, that leaves
/// room for something like 17KB of artifact — so this is a bound rather than a
/// squeeze, and the device refuses anything it cannot actually fit.
pub const MAX_ARTIFACT_BYTES: usize = 16 * 1024;

/// Maximum size in bytes of a script as submitted, before minification. Comments
/// and indentation are stripped before `MAX_SCRIPT_BYTES` applies, so this is what
/// bounds the work the validator will do on one request.
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
    /// dmx_recv(timeout_ms) — the newest DMX frame received within the timeout.
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

/// A DMX frame as handed to a script.
pub struct DmxFrame {
    pub seq: i64,
    /// DMX channel number that `channels[0]` holds, so a script can locate a
    /// fixture without knowing how the sender was configured.
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
    // No operation cap. A script doing real signal analysis on a 40Hz DMX stream
    // burns 5M operations in about ten minutes, so the cap ended shows rather
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
    // Returns #{ ok, base, seq, ch }. `ch` holds raw 0-255 values, `base` is the
    // DMX channel ch[0] corresponds to. On timeout `ok` is false and `ch` is
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

/// Which of rhai's optional standard packages an engine carries.
///
/// `Engine::run` takes `&self`, so an engine cannot gain a package once a run
/// has started; the set is fixed before evaluation. That matters because
/// registering all of them costs ~69KB of the device's ~140KB free heap, which
/// leaves room for about 5KB of script — less than `scripts/follow.rhai` needs.
/// Registering only what a script uses costs ~21KB for that script and roughly
/// doubles the ceiling. `crates/script-env/tests/footprint.rs` prices it.
///
/// Arithmetic and logic are not represented here because they are not optional:
/// every script counts and compares, and together they are a small part of the
/// cost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Components {
    pub core: bool,
    pub array: bool,
    pub map: bool,
    pub string: bool,
    pub math: bool,
    pub iterator: bool,
    pub blob: bool,
    pub bit_field: bool,
    pub functions: bool,
}

impl Components {
    /// Everything the documented surface promises. What validation must accept,
    /// and the safe answer whenever the needed set is unknown.
    pub const fn all() -> Self {
        Components {
            core: true,
            array: true,
            map: true,
            string: true,
            math: true,
            iterator: true,
            blob: true,
            bit_field: true,
            functions: true,
        }
    }

    /// Everything except what a script must opt out of explicitly.
    ///
    /// There is deliberately no `detect(script)`. Rhai resolves calls at run
    /// time, so which packages a script needs is not decidable from its text:
    /// the only sound answers are to run every path (impossible) or to be told.
    /// Inferring it from substrings would trade a reboot for a script that dies
    /// the first time an untested branch is taken, which is worse — the failure
    /// moves from startup to the middle of a set.
    /// Every component name, in the order they are documented.
    pub const NAMES: [&'static str; 9] = [
        "core",
        "array",
        "map",
        "string",
        "math",
        "iterator",
        "blob",
        "bitfield",
        "functions",
    ];

    /// Parse a declaration, as submitted with a script.
    ///
    /// Unknown names are rejected rather than ignored: a typo that silently
    /// dropped a component would surface much later, as the script dying at the
    /// first call it could not resolve.
    pub fn from_names<S: AsRef<str>>(names: impl IntoIterator<Item = S>) -> Result<Self, String> {
        let mut c = Components::none();
        for name in names {
            let name = name.as_ref();
            match name {
                "core" => c.core = true,
                "array" => c.array = true,
                "map" => c.map = true,
                "string" => c.string = true,
                "math" => c.math = true,
                "iterator" => c.iterator = true,
                "blob" => c.blob = true,
                "bitfield" => c.bit_field = true,
                "functions" => c.functions = true,
                other => {
                    return Err(format!(
                        "unknown component {other:?}; valid components are {}",
                        Self::NAMES.join(", ")
                    ))
                }
            }
        }
        Ok(c)
    }

    /// The declared names, for echoing back and for storing with a job.
    pub fn names(&self) -> Vec<&'static str> {
        let on = [
            self.core,
            self.array,
            self.map,
            self.string,
            self.math,
            self.iterator,
            self.blob,
            self.bit_field,
            self.functions,
        ];
        Self::NAMES
            .iter()
            .zip(on)
            .filter(|(_, on)| *on)
            .map(|(n, _)| *n)
            .collect()
    }

    pub const fn none() -> Self {
        Components {
            core: false,
            array: false,
            map: false,
            string: false,
            math: false,
            iterator: false,
            blob: false,
            bit_field: false,
            functions: false,
        }
    }
}

/// An engine carrying `components`, with the sandbox limits applied.
///
/// The single construction site for the language surface: the validator and the
/// firmware both come through here, so what validates and what runs cannot
/// drift apart.
pub fn new_engine(components: Components) -> Engine {
    use rhai::packages::Package;

    // Arithmetic and logic are built once and shared by pointer thereafter:
    // `register_global_module` stores the handle rather than copying functions
    // in, so every run after the first pays a refcount instead of rebuilding
    // them. That is churn the board feels — it idles with 140KB free but a
    // largest block of 108KB.
    //
    // Only these two, deliberately. A cached module is never freed, so caching
    // the optional ones would pin whatever any script ever declared: one idle
    // script wanting the full surface would hold ~63KB for the life of the
    // board and cost every later job more than declaring ever saved it
    // (measured: a 5.9KB ceiling against the 12.3KB this arrangement gives).
    // These two are in every engine regardless, so they can never be wasted.
    macro_rules! shared {
        ($pkg:ty) => {{
            static CELL: std::sync::OnceLock<std::sync::Arc<rhai::Module>> =
                std::sync::OnceLock::new();
            CELL.get_or_init(|| <$pkg>::new().as_shared_module()).clone()
        }};
    }
    // `init_engine` is how a package registers custom operators or syntax. Every
    // package used here leaves it empty, but calling it keeps this exactly
    // equivalent to `register_into_engine` if that ever stops being true.
    macro_rules! add_shared {
        ($engine:expr, $pkg:ty) => {{
            <$pkg as Package>::init_engine(&mut $engine);
            $engine.register_global_module(shared!($pkg));
        }};
    }
    // Built per run and freed with the engine, so an unused component costs
    // nothing beyond the run that asked for it.
    macro_rules! add {
        ($engine:expr, $pkg:ty) => {{
            <$pkg>::new().register_into_engine(&mut $engine);
        }};
    }

    let mut engine = Engine::new_raw();

    // `Engine::new` turns this on and `new_raw` does not, so building up from
    // raw silently loses it. Without the interner every identifier and string
    // literal in a script allocates separately instead of being shared: measured
    // on scripts/follow.rhai, the AST goes 91648 -> 110368 bytes and 1303 -> 1879
    // allocations, and on the device each of those allocations also costs an
    // 8-byte heap header. Not optional on a board this tight.
    engine.set_max_strings_interned(1024);

    // Not optional — see `Components`.
    add_shared!(engine, rhai::packages::ArithmeticPackage);
    add_shared!(engine, rhai::packages::LogicPackage);

    if components.core {
        add!(engine, rhai::packages::LanguageCorePackage);
    }
    if components.array {
        add!(engine, rhai::packages::BasicArrayPackage);
    }
    if components.map {
        add!(engine, rhai::packages::BasicMapPackage);
    }
    if components.string {
        add!(engine, rhai::packages::BasicStringPackage);
        // Both, or `all()` would be narrower than the `Engine::new()` it stands
        // in for: StandardPackage carries MoreStringPackage too.
        add!(engine, rhai::packages::MoreStringPackage);
    }
    if components.math {
        add!(engine, rhai::packages::BasicMathPackage);
    }
    if components.iterator {
        add!(engine, rhai::packages::BasicIteratorPackage);
    }
    if components.blob {
        add!(engine, rhai::packages::BasicBlobPackage);
    }
    if components.bit_field {
        add!(engine, rhai::packages::BitFieldPackage);
    }
    if components.functions {
        add!(engine, rhai::packages::BasicFnPackage);
    }

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
    pub positions: Vec<u8>,
    /// Nodes the compiler could not lower, which stay AST fragments and run on
    /// rhai's walker. Zero for everything in this repo; a script that reports
    /// more is paying the tree's per-node allocation cost for that part.
    pub residual: usize,
}

/// Lower a compiled script to a device artifact.
///
/// The parser, the optimiser and the tree all stay here. What reaches the light
/// is a flat buffer it verifies and executes, which is the difference between
/// ~1069 allocations and ~65 for `scripts/follow.rhai`.
pub fn lower(ast: &rhai::AST) -> Result<Artifact, String> {
    let mut program = rhaigrain::Compiler::new().compile(ast);
    let residual = program.residual_count();
    let positions = program.strip_positions();
    let program = program
        .write()
        .map_err(|e| format!("cannot serialise the artifact: {e}"))?;
    Ok(Artifact {
        program,
        positions,
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
        rhaigrain::Program::read(bytes).map_err(|e| -> Box<rhai::EvalAltResult> {
            format!("artifact will not load: {e}").into()
        })?;
    program
        .verify()
        .map_err(|e| -> Box<rhai::EvalAltResult> { format!("artifact failed verification: {e:?}").into() })?;

    let mut scope = rhai::Scope::new();
    rhaigrain::Vm::new(engine)
        .run(&program, &mut scope)
        .map(|_| ())
}

/// A fully configured engine with stub handlers, for validation.
///
/// Always the full surface: validation must accept anything the docs promise,
/// whatever narrower set the device will actually run it on.
pub fn validation_engine() -> Engine {
    let mut engine = new_engine(Components::all());
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
mod component_tests {
    use super::*;

    /// Exercises one feature from every package `Engine::new()` carries, so a
    /// gap between `Components::all()` and the surface it replaces shows up
    /// here rather than in somebody's script at 2am. Rhai resolves calls at run
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
    fn all_components_cover_the_standard_surface() {
        let mut engine = new_engine(Components::all());
        register_api(&mut engine, Handlers::stubs());
        let got: bool = engine
            .eval(SURFACE)
            .expect("Components::all() must run everything Engine::new() would");
        assert!(got);
    }

    /// The saving is real only if dropping components actually drops functions.
    #[test]
    fn dropping_components_drops_the_surface() {
        let mut engine = new_engine(Components::none());
        register_api(&mut engine, Handlers::stubs());
        assert!(
            engine.eval::<bool>(SURFACE).is_err(),
            "a bare engine still ran the full surface; the components do nothing"
        );
    }

    /// Arithmetic and logic are always on, so the cheapest engine is still able
    /// to count and compare — which is what makes them non-optional.
    /// What each component actually contains, established by probing rather
    /// than by reading rhai's package list — several entries are not where they
    /// look like they should be. Pinned so the README's table cannot rot.
    ///
    /// `core` and `functions` are absent deliberately: nothing reachable from
    /// this language surface could be shown to depend on them, so the docs do
    /// not claim they provide anything specific.
    const PER_COMPONENT: [(&str, &str); 7] = [
        ("array", "let a = [1, 2]; a.len() == 2"),
        ("map", "let m = #{a: 1}; m.len() == 1"),
        ("string", r#""abc".sub_string(1, 1) == "b""#),
        ("math", "(4.0).sqrt() > 1.0"),
        ("iterator", "let t = 0; for i in 0..3 { t += i } t == 3"),
        ("blob", "let b = blob(2); b.len() == 2"),
        ("bitfield", "let x = 6; x.get_bit(1)"),
    ];

    fn runs(c: Components, src: &str) -> bool {
        let mut engine = new_engine(c);
        register_api(&mut engine, Handlers::stubs());
        matches!(engine.eval::<bool>(src), Ok(true))
    }

    /// Each component provides its own entry, and *only* its own — so the docs
    /// can tell someone which one name to add when a call fails.
    #[test]
    fn components_provide_exactly_what_the_docs_say() {
        for (name, src) in PER_COMPONENT {
            let only = Components::from_names([name]).unwrap();
            assert!(runs(only, src), "{name}: {src:?} did not run with only {name}");
            assert!(
                !runs(Components::none(), src),
                "{name}: {src:?} runs with no components, so the docs must not \
                 attribute it to {name}"
            );
            for other in Components::NAMES {
                if other == name || other == "core" || other == "functions" {
                    continue;
                }
                let only_other = Components::from_names([other]).unwrap();
                assert!(
                    !runs(only_other, src),
                    "{name}: {src:?} also runs under {other}, so the docs are wrong"
                );
            }
        }
    }

    /// Field access on a map is core language, not the `map` component — worth
    /// pinning because `dmx_recv` hands scripts a map and reading it must not
    /// require a declaration.
    #[test]
    fn reading_a_map_field_needs_nothing() {
        assert!(runs(Components::none(), "let m = #{a: 1}; m.a == 1"));
    }

    /// `to_float` reads like a conversion but lives in `math`, which is why
    /// scripts/follow.rhai needs that component despite doing no trigonometry.
    #[test]
    fn to_float_needs_math() {
        assert!(!runs(Components::none(), "(1).to_float() > 0.0"));
        assert!(runs(Components::from_names(["math"]).unwrap(), "(1).to_float() > 0.0"));
    }

    /// print and debug format their argument, so they need `string` — not the
    /// `core` name a reader would reach for first.
    #[test]
    fn print_needs_string() {
        assert!(!runs(Components::none(), "print(1); true"));
        assert!(runs(Components::from_names(["string"]).unwrap(), "print(1); true"));
    }

    #[test]
    fn names_round_trip() {
        let all = Components::all();
        assert_eq!(all.names().len(), Components::NAMES.len());
        assert_eq!(Components::from_names(all.names()).unwrap(), all);
        assert_eq!(Components::from_names(Vec::<&str>::new()).unwrap(), Components::none());
    }

    /// A typo must not quietly drop a component: the script would run until it
    /// reached the first call it could not resolve, which could be hours in.
    #[test]
    fn unknown_names_are_rejected_and_say_what_is_valid() {
        let e = Components::from_names(["array", "maths"]).unwrap_err();
        assert!(e.contains("maths"), "{e}");
        assert!(e.contains("math"), "{e}");
    }

    #[test]
    fn the_cheapest_engine_still_counts_and_compares() {
        let mut engine = new_engine(Components::none());
        register_api(&mut engine, Handlers::stubs());
        assert!(engine.eval::<bool>("let n = 7 + 3 * 2; n > 5 && n != 0").unwrap());
    }
}
