use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde_json::json;
use wasm_bindgen::prelude::*;

/// Compile-check a script and lower it to the artifact the device runs.
///
/// Returns JSON: {"ok":true,"script":...,"artifact":...,"positions":...,"residual":N,
/// "bytes":N,"artifactBytes":M} or {"ok":false,"error":...,"line":...,"col":...}
///
/// Compiles with the engine the firmware builds, so what validates is what runs.
/// Error positions are rhai's own, against the text as submitted.
#[wasm_bindgen]
pub fn prepare(script: &str) -> String {
    if script.len() > script_env::MAX_RAW_SCRIPT_BYTES {
        return reject(&format!(
            "script is {} bytes; limit is {}",
            script.len(),
            script_env::MAX_RAW_SCRIPT_BYTES
        ));
    }

    let ast = match script_env::validation_engine().compile(script) {
        Ok(ast) => ast,
        Err(e) => {
            let pos = e.position();
            return json!({
                "ok": false,
                "error": e.to_string(),
                "line": pos.line(),
                "col": pos.position(),
            })
            .to_string();
        }
    };

    // The parser, the optimiser and the tree all stay here; what ships is a flat
    // buffer, which is the difference between ~1069 allocations on the light and ~65.
    let artifact = match script_env::lower(&ast) {
        Ok(a) => a,
        Err(e) => return reject(&format!("cannot lower the script: {e}")),
    };
    let program = BASE64.encode(&artifact.program);

    // The artifact is what the device loads, so it is the size that decides
    // whether the light can run this at all.
    if program.len() > script_env::MAX_ARTIFACT_BYTES {
        return reject(&format!(
            "compiles to {} bytes; the device's limit is {}",
            program.len(),
            script_env::MAX_ARTIFACT_BYTES
        ));
    }

    json!({
        "ok": true,
        "script": script,
        // Base64 because the frame carrying this is JSON.
        "artifact": program,
        "positions": serde_json::to_string(&artifact.sidecar).unwrap_or_default(),
        // Nodes the compiler could not lower, which run on rhai's walker and keep
        // the tree's per-node cost. Reported rather than rejected: it is a cost,
        // not an error.
        "residual": artifact.residual,
        "bytes": script.len(),
        "artifactBytes": program.len(),
    })
    .to_string()
}

/// A size rejection rather than a parse failure: no position to report, and the
/// API answers 413 rather than 422.
fn reject(error: &str) -> String {
    json!({ "ok": false, "error": error, "line": null, "col": null, "tooBig": true }).to_string()
}
