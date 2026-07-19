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
}

impl Handlers {
    /// No-op handlers for compile-only validation.
    pub fn stubs() -> Self {
        Handlers {
            set_lights: Box::new(|_, _, _| {}),
            sleep: Box::new(|_| {}),
            millis: Box::new(|| 0),
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
    let Handlers { set_lights, sleep, millis } = handlers;
    engine.register_fn("set_lights", move |r: bool, y: bool, g: bool| set_lights(r, y, g));
    engine.register_fn("sleep", move |ms: i64| sleep(ms));
    engine.register_fn("millis", move || millis());
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
    fn rejects_expr_depth_bomb() {
        let engine = validation_engine();
        let bomb = format!("let x = {}1{};", "(".repeat(200), ")".repeat(200));
        assert!(engine.compile(&bomb).is_err());
    }
}
