//! What `prepare` promises the API: the text it returns still parses under the
//! engine the firmware runs, positions in errors survive the round trip, and the
//! byte counts it reports are the ones the device pays.

use serde_json::Value;

const FOLLOW: &str = include_str!("../../../scripts/follow.rhai");

fn prepared(script: &str) -> Value {
    serde_json::from_str(&validator::prepare(script)).expect("prepare returned invalid JSON")
}

fn ok(script: &str) -> Value {
    let v = prepared(script);
    assert_eq!(v["ok"], Value::Bool(true), "prepare rejected: {v}");
    v
}

fn text(v: &Value) -> String {
    v["script"].as_str().unwrap().to_string()
}

/// Valid under the restricted engine: no closures, modules, or f64 literals.
const SCRIPTS: &[&str] = &[
    "set_lights(true, false, false);\n",
    "// a comment\nlet x = 1; /* and another */\nsleep(x);\n",
    "loop {\n    set_lights(true, false, false);\n    sleep(500);\n    break;\n}\n",
    "let i = 0;\nwhile i < 3 {\n    i += 1;\n}\n",
    "let a = 5;\nlet b = a - -1;\nsleep(b);\n",
    "for i in 0..3 {\n    sleep(i);\n}\n",
    "try {\n    sleep(1);\n} catch (e) {\n    sleep(2);\n}\n",
];

#[test]
fn output_still_compiles_under_the_device_engine() {
    let engine = script_env::validation_engine();
    for src in SCRIPTS.iter().chain(std::iter::once(&FOLLOW)) {
        let out = text(&ok(src));
        engine
            .compile(&out)
            .unwrap_or_else(|e| panic!("minified output does not compile: {e}\n{out}"));
    }
}

#[test]
fn output_never_grows_and_is_a_fixed_point() {
    for src in SCRIPTS.iter().chain(std::iter::once(&FOLLOW)) {
        let once = text(&ok(src));
        assert!(once.len() <= src.len(), "{src:?} grew to {once:?}");
        assert_eq!(once, text(&ok(&once)), "not idempotent for {src:?}");
    }
}

#[test]
fn reported_bytes_match_the_text() {
    let v = ok(FOLLOW);
    assert_eq!(v["rawBytes"].as_u64().unwrap() as usize, FOLLOW.len());
    assert_eq!(v["bytes"].as_u64().unwrap() as usize, text(&v).len());
    assert_eq!(v["minified"], Value::Bool(true));
}

/// The number that justifies the whole pipeline. Printed rather than pinned to an
/// exact value, which would break on any rhaiper or script change.
#[test]
fn follow_rhai_shrinks_by_at_least_a_quarter() {
    let v = ok(FOLLOW);
    let (raw, min) = (FOLLOW.len(), text(&v).len());
    let saved = 100.0 * (raw - min) as f64 / raw as f64;
    println!(
        "follow.rhai  {raw} -> {min} bytes ({} saved, {saved:.1}%), map {} bytes",
        raw - min,
        v["map"].as_str().unwrap().len()
    );
    assert!(saved > 25.0, "only {saved:.1}% saved");
}

#[test]
fn parse_errors_point_at_the_submitted_script() {
    // Line 3 is where the stray brace is; a minified position would say line 1.
    let v = prepared("let x = 1;\nsleep(x);\n}\n");
    assert_eq!(v["ok"], Value::Bool(false));
    assert_eq!(v["line"].as_u64(), Some(3), "got {v}");
}

#[test]
fn undeclared_variables_are_still_rejected() {
    // strict_variables is part of the shared engine, so this must not become
    // reachable just because minification now sits in front of it.
    let v = prepared("sleep(nope);\n");
    assert_eq!(v["ok"], Value::Bool(false), "got {v}");
}

#[test]
fn oversized_raw_input_is_rejected_before_minifying() {
    let huge = format!("// {}\nsleep(1);\n", "x".repeat(script_env::MAX_RAW_SCRIPT_BYTES));
    let v = prepared(&huge);
    assert_eq!(v["ok"], Value::Bool(false));
    assert!(
        v["error"].as_str().unwrap().contains("limit is"),
        "got {v}"
    );
}

/// A script whose comments push it past the device limit but whose code fits is
/// exactly the case moving the cap after minification was meant to allow.
#[test]
fn comments_no_longer_count_against_the_device_limit() {
    let filler = "// filler filler filler filler filler filler\n".repeat(500);
    let src = format!("{filler}set_lights(true, false, false);\n");
    assert!(src.len() > script_env::MAX_SCRIPT_BYTES);

    let v = ok(&src);
    let out = text(&v);
    assert!(!out.contains("filler"), "comments survived: {out}");
    assert!(out.starts_with("set_lights(true,false,false)"), "got {out}");
    assert!(out.len() < script_env::MAX_SCRIPT_BYTES);
}

#[test]
fn remap_recovers_the_authored_line() {
    let src = "let a = 1;\nlet b = 2;\nlet c = 3;\nset_lights(true);\n";
    let v = ok(src);
    let (min, map) = (text(&v), v["map"].as_str().unwrap());

    // Everything collapses onto one line, so the authored line is only recoverable
    // through the map. All-ASCII, so byte offset and character column coincide.
    let col = min.find("set_lights").expect("call survived minification") + 1;
    let message = format!("Function not found: set_lights (line 1, position {col})");

    let out = validator::remap(map, &min, &message);
    assert_eq!(
        out,
        "Function not found: set_lights (line 4, position 1)",
        "minified was {min:?}"
    );
}

#[test]
fn remap_leaves_what_it_cannot_resolve_alone() {
    let v = ok("let a = 1;\nsleep(a);\n");
    let (min, map) = (text(&v), v["map"].as_str().unwrap());

    for message in [
        "no positions here at all",
        "line 9999, position 1", // past the end of the output
        "the word line on its own",
        "line 1, position", // no column follows
    ] {
        assert_eq!(validator::remap(map, &min, message), message);
    }
}

/// A column past the end of a line resolves to the last mapping at or before it,
/// which is how Source Map v3 lookup is defined. Worth pinning because it is also
/// what a column saturated by rhai's `u16` `Position` degrades to.
#[test]
fn remap_clamps_a_column_past_the_end_of_a_line() {
    let v = ok("let a = 1;\nsleep(a);\n");
    let (min, map) = (text(&v), v["map"].as_str().unwrap());

    let out = validator::remap(map, &min, "line 1, position 99999");
    assert_eq!(out, "line 2, position 8", "minified was {min:?}");
}

#[test]
fn remap_survives_a_map_it_cannot_parse() {
    let message = "Function not found: f (line 1, position 1)";
    assert_eq!(validator::remap("", "f()", message), message);
    assert_eq!(validator::remap("{}", "f()", message), message);
}
