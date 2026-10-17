//! What `prepare` promises the API: the script it accepts compiles under the
//! engine the firmware runs, lowers to an artifact that runs there, and errors
//! point at the submitted text.

use serde_json::Value;

const PULSE: &str = include_str!("../../../scripts/pulse.rhai");

fn prepared(script: &str) -> Value {
    serde_json::from_str(&validator::prepare(script)).expect("prepare returned invalid JSON")
}

fn ok(script: &str) -> Value {
    let v = prepared(script);
    assert_eq!(v["ok"], Value::Bool(true), "prepare rejected: {v}");
    v
}

#[test]
fn accepted_scripts_are_returned_as_submitted() {
    let src = "// a comment\nlet x = 1;\nsleep(x);\n";
    assert_eq!(ok(src)["script"].as_str(), Some(src));
}

#[test]
fn the_artifact_runs_on_the_device_engine() {
    use base64::Engine as _;
    let v = ok(PULSE);
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(v["artifact"].as_str().unwrap())
        .unwrap();
    assert_eq!(v["residual"].as_u64(), Some(0));
    assert_eq!(v["artifactBytes"].as_u64().unwrap() as usize, v["artifact"].as_str().unwrap().len());
    // Loads and verifies; running it would block on the script's own sleeps.
    let program = script_env::rhai::grain::Program::read(&bytes).expect("artifact loads");
    program.verify().expect("artifact verifies");
}

#[test]
fn parse_errors_point_at_the_submitted_script() {
    let v = prepared("let x = 1;\nsleep(x);\n}\n");
    assert_eq!(v["ok"], Value::Bool(false));
    assert_eq!(v["line"].as_u64(), Some(3), "got {v}");
}

#[test]
fn undeclared_variables_are_rejected() {
    let v = prepared("sleep(nope);\n");
    assert_eq!(v["ok"], Value::Bool(false), "got {v}");
}

#[test]
fn oversized_input_is_rejected_as_too_big() {
    let huge = format!(
        "// {}\nsleep(1);\n",
        "x".repeat(script_env::MAX_RAW_SCRIPT_BYTES)
    );
    let v = prepared(&huge);
    assert_eq!(v["ok"], Value::Bool(false));
    assert_eq!(v["tooBig"], Value::Bool(true));
    assert!(v["error"].as_str().unwrap().contains("limit is"), "got {v}");
}
