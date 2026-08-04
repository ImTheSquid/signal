use rhaiper::map::{MapOptions, SourceMap};
use rhaiper::{minify_with_engine, Options};
use serde_json::json;
use wasm_bindgen::prelude::*;

/// Compile-check a script and minify it for the device.
///
/// Returns JSON: {"ok":true,"script":...,"map":...,"rawBytes":N,"bytes":M,"minified":bool}
/// or {"ok":false,"error":...,"line":...,"col":...}
///
/// `minify_with_engine` compiles both its input and its own output with the engine it
/// is given, so passing the validation engine makes one call cover the parse check the
/// API needs and a conformance check on the text the device will actually run.
#[wasm_bindgen]
pub fn prepare(script: &str) -> String {
    if script.len() > script_env::MAX_RAW_SCRIPT_BYTES {
        return reject(&format!(
            "script is {} bytes; limit is {}",
            script.len(),
            script_env::MAX_RAW_SCRIPT_BYTES
        ));
    }

    let engine = script_env::validation_engine();
    let opts = Options {
        rename: false,
        ..Default::default()
    };

    match minify_with_engine(&engine, script, &opts) {
        Ok(out) => {
            if out.text.len() > script_env::MAX_SCRIPT_BYTES {
                return reject(&format!(
                    "script is {} bytes minified; limit is {}",
                    out.text.len(),
                    script_env::MAX_SCRIPT_BYTES
                ));
            }
            let map = out.to_source_map(
                script,
                &MapOptions {
                    // A copy of the original would roughly double what the map costs
                    // to store, and nothing reads it back — only positions are wanted.
                    include_sources_content: false,
                    // Submitted scripts have no filename. Only external source-map
                    // tooling would ever see this.
                    source_name: "script".into(),
                    ..Default::default()
                },
            );
            json!({
                "ok": true,
                "script": out.text,
                "map": map,
                "rawBytes": script.len(),
                "bytes": out.text.len(),
                "minified": true,
            })
            .to_string()
        }

        // The submitted script does not parse. Positions are rhai's own, against the
        // text the author wrote, which is why minification happens after this check.
        Err(rhaiper::Error::InvalidInput(e)) => {
            let pos = e.position();
            json!({
                "ok": false,
                "error": e.to_string(),
                "line": pos.line(),
                "col": pos.position(),
            })
            .to_string()
        }

        // rhaiper could not produce output it trusts. Running the script unminified is
        // what happened before minification existed, so degrade to that rather than
        // refuse to run a script that parses.
        Err(e) => {
            if script.len() > script_env::MAX_SCRIPT_BYTES {
                return reject(&format!(
                    "script is {} bytes and could not be minified ({e}); \
                     limit without minification is {}",
                    script.len(),
                    script_env::MAX_SCRIPT_BYTES
                ));
            }
            json!({
                "ok": true,
                "script": script,
                "map": "",
                "rawBytes": script.len(),
                "bytes": script.len(),
                "minified": false,
                "warning": e.to_string(),
            })
            .to_string()
        }
    }
}

/// Rewrite the `line N, position M` positions in a device-reported error so they
/// refer to the submitted script rather than the minified text.
///
/// `minified` is the text the device ran: Source Map v3 counts columns in UTF-16
/// units while rhai counts characters, and converting between them needs that line.
/// Anything that does not resolve is left exactly as it arrived.
#[wasm_bindgen]
pub fn remap(map_json: &str, minified: &str, message: &str) -> String {
    const LINE: &str = "line ";
    const POS: &str = ", position ";

    let Ok(map) = SourceMap::from_json(map_json) else {
        return message.to_string();
    };

    let mut out = String::with_capacity(message.len());
    let mut rest = message;

    while let Some(at) = rest.find(LINE) {
        // The marker goes out with the text before it, so a position that does not
        // resolve cannot rescan it.
        let (head, tail) = rest.split_at(at + LINE.len());
        out.push_str(head);
        rest = tail;

        let (Some(line), after) = leading_u32(rest) else {
            continue;
        };
        let Some(after) = after.strip_prefix(POS) else {
            continue;
        };
        let (Some(col), after) = leading_u32(after) else {
            continue;
        };
        let Some(origin) = map.resolve(minified, line, col) else {
            continue;
        };

        out.push_str(&format!("{}{POS}{}", origin.line, origin.column));
        rest = after;
    }

    out.push_str(rest);
    out
}

/// Splits a leading run of ASCII digits off `s`.
fn leading_u32(s: &str) -> (Option<u32>, &str) {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    (s[..end].parse().ok(), &s[end..])
}

/// A size rejection rather than a parse failure: no position to report, and the
/// API answers 413 rather than 422.
fn reject(error: &str) -> String {
    json!({ "ok": false, "error": error, "line": null, "col": null, "tooBig": true }).to_string()
}
