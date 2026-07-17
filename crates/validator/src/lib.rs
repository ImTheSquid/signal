use serde_json::json;
use wasm_bindgen::prelude::*;

/// Compile-check a script against the shared engine configuration.
/// Returns JSON: {"ok":true} or {"ok":false,"error":...,"line":...,"col":...}
#[wasm_bindgen]
pub fn validate(script: &str) -> String {
    let result = if script.len() > script_env::MAX_SCRIPT_BYTES {
        json!({
            "ok": false,
            "error": format!(
                "script is {} bytes; limit is {}",
                script.len(),
                script_env::MAX_SCRIPT_BYTES
            ),
            "line": null,
            "col": null,
        })
    } else {
        match script_env::validation_engine().compile(script) {
            Ok(_) => json!({ "ok": true }),
            Err(e) => {
                let pos = e.position();
                json!({
                    "ok": false,
                    "error": e.to_string(),
                    "line": pos.line(),
                    "col": pos.position(),
                })
            }
        }
    };
    result.to_string()
}
