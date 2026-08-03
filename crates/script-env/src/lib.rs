//! Shared Rhai configuration for the traffic-light script environment.
//!
//! The server-side validator and the ESP32 firmware both build their engines
//! through this crate, so the language surface and safety limits cannot drift
//! between validation and execution.

pub use rhai;

use rhai::Engine;

/// Maximum script source size in bytes, enforced at the API, the websocket,
/// and on-device before compilation.
pub const MAX_SCRIPT_BYTES: usize = 16 * 1024;

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
        }
    }
}

/// Apply the sandbox limits. Parse-time limits (expression depth) also make
/// `Engine::compile` reject pathological inputs during validation.
pub fn apply_limits(engine: &mut Engine) {
    engine.set_max_operations(5_000_000);
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
    let Handlers { set_lights, sleep, millis, dmx_recv } = handlers;
    engine.register_fn("set_lights", move |r: bool, y: bool, g: bool| set_lights(r, y, g));
    engine.register_fn("sleep", move |ms: i64| sleep(ms));
    engine.register_fn("millis", move || millis());
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

/// A fully configured engine with stub handlers, for validation.
pub fn validation_engine() -> Engine {
    let mut engine = Engine::new();
    apply_limits(&mut engine);
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
    fn rejects_expr_depth_bomb() {
        let engine = validation_engine();
        let bomb = format!("let x = {}1{};", "(".repeat(200), ")".repeat(200));
        assert!(engine.compile(&bomb).is_err());
    }
}
