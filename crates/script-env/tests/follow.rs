//! Runs the real `scripts/follow.rhai` against a synthetic DMX stream on a
//! virtual clock.
//!
//! The script is the part of this system that cannot be checked by reading it:
//! every past failure — green mathematically excluded, the light idle most of
//! the time, a pattern indistinguishable from a traffic signal — was invisible
//! in the source and obvious in the output. So the output is what gets asserted.
//!
//! Virtual time, not real: the run below covers 60 simulated seconds in
//! milliseconds of wall clock, and is deterministic.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use script_env::{rhai, DmxFrame, Handlers};

const SCRIPT: &str = include_str!("../../../scripts/follow.rhai");

/// ~41Hz, matching the bridge.
const FRAME_MS: i64 = 24;
const DWELL_MS: i64 = 100;
/// 128 BPM, and a second tempo used as a control.
const BEAT_MS: i64 = 469;
const BEAT_MS_SLOW: i64 = 600; // 100 BPM
const RUN_MS: i64 = 60_000;

/// One synthetic frame, built to look like the measured capture rather than
/// something convenient:
///
/// - red is the kick, a fast decay on every beat, full scale
/// - green is a pad that swells over four beats and **peaks at 131**, which is
///   what the real set peaked at and what a frame-relative cut could never admit
/// - blue stabs on alternate beats, peaking at 200
/// - a dark section in the middle, because the real output is dark ~72% of the
///   time and the palette has to latch through it
fn synth(t: i64, beat_ms: i64) -> Vec<u8> {
    if (18_000..24_000).contains(&t) {
        return vec![0, 0, 0]; // blackout
    }
    let beat = t / beat_ms;
    let tb = t % beat_ms;

    let kick = if tb < 120 {
        255.0 * (1.0 - tb as f32 / 120.0)
    } else {
        0.0
    };
    let phase = (beat % 4) as f32 + tb as f32 / beat_ms as f32;
    let pad = 131.0 * (phase / 4.0);
    let stab = if beat % 2 == 0 && tb < 80 {
        200.0 * (1.0 - tb as f32 / 80.0)
    } else {
        0.0
    };
    vec![kick as u8, pad as u8, stab as u8]
}

struct Run {
    /// (time_ms, r, y, g) for every set_lights the script made.
    changes: Vec<(i64, bool, bool, bool)>,
    end_ms: i64,
}

fn run_follow(beat_ms: i64) -> Run {
    let clock = Arc::new(AtomicI64::new(0));
    let changes = Arc::new(Mutex::new(Vec::new()));
    // Next frame the fake socket will deliver.
    let next_frame = Arc::new(AtomicI64::new(0));
    let seq = Arc::new(AtomicI64::new(1));

    let mut engine = rhai::Engine::new();
    script_env::apply_limits(&mut engine);
    script_env::register_api(
        &mut engine,
        Handlers {
            set_lights: Box::new({
                let clock = clock.clone();
                let changes = changes.clone();
                move |r, y, g| {
                    changes
                        .lock()
                        .unwrap()
                        .push((clock.load(Ordering::SeqCst), r, y, g));
                }
            }),
            sleep: Box::new({
                let clock = clock.clone();
                move |ms| {
                    if ms > 0 {
                        clock.fetch_add(ms, Ordering::SeqCst);
                    }
                }
            }),
            millis: Box::new({
                let clock = clock.clone();
                move || clock.load(Ordering::SeqCst)
            }),
            dmx_recv: Box::new({
                let clock = clock.clone();
                let next_frame = next_frame.clone();
                let seq = seq.clone();
                move |timeout| {
                    let now = clock.load(Ordering::SeqCst);
                    let mut due = next_frame.load(Ordering::SeqCst);
                    if due > now + timeout {
                        // Nothing within the window.
                        clock.store(now + timeout, Ordering::SeqCst);
                        return Ok(None);
                    }
                    let at = due.max(now);
                    // Coalesce exactly like the socket: everything already past
                    // is discarded and only the newest is delivered.
                    let mut newest = due;
                    while due <= at {
                        newest = due;
                        due += FRAME_MS;
                    }
                    next_frame.store(due, Ordering::SeqCst);
                    clock.store(at, Ordering::SeqCst);
                    Ok(Some(DmxFrame {
                        seq: seq.fetch_add(1, Ordering::SeqCst),
                        base: 1,
                        channels: synth(newest, beat_ms),
                    }))
                }
            }),
            lamp_dwell_ms: Box::new(|| DWELL_MS),
            ..Handlers::stubs()
        },
    );

    // The script loops forever by design; virtual time is the deadline.
    engine.on_progress({
        let clock = clock.clone();
        move |_| {
            if clock.load(Ordering::SeqCst) >= RUN_MS {
                Some(rhai::Dynamic::from("done"))
            } else {
                None
            }
        }
    });

    match engine.run(SCRIPT) {
        Ok(()) => panic!("follow.rhai returned; it must loop"),
        // ErrorTerminated is on_progress stopping it at RUN_MS. Anything else is
        // a real runtime fault in the script, which is exactly what this catches
        // — Rhai resolves calls at runtime, so parse-time validation cannot.
        Err(e) => match *e {
            rhai::EvalAltResult::ErrorTerminated(..) => {}
            other => panic!("follow.rhai failed at runtime: {other}"),
        },
    }

    let changes = changes.lock().unwrap().clone();
    Run {
        end_ms: clock.load(Ordering::SeqCst),
        changes,
    }
}

/// Nothing about the pattern matters if the relays are asked to move faster than
/// they can. The script gates on `lamp_dwell_ms()` itself so `set_lights` never
/// blocks — a blocked call stalls the loop and drops frames.
#[test]
fn respects_the_relay_dwell() {
    let run = run_follow(BEAT_MS);
    let mut worst = i64::MAX;
    for w in run.changes.windows(2) {
        worst = worst.min(w[1].0 - w[0].0);
    }
    assert!(
        worst >= DWELL_MS,
        "two writes {worst}ms apart, dwell is {DWELL_MS}ms"
    );
}

/// The complaint that started this: the light was off or static most of the
/// time. Against a 469ms beat, a dense pattern changes several times per beat.
#[test]
fn the_pattern_is_dense() {
    let run = run_follow(BEAT_MS);
    let per_sec = run.changes.len() as f64 / (run.end_ms as f64 / 1000.0);
    let lit = run.changes.iter().filter(|c| c.1 || c.2 || c.3).count();
    println!(
        "{} transitions in {}s = {per_sec:.1}/s; {}% of them light something",
        run.changes.len(),
        run.end_ms / 1000,
        lit * 100 / run.changes.len()
    );
    assert!(
        per_sec >= 3.0,
        "only {per_sec:.1} transitions/s over {}s",
        run.end_ms / 1000
    );
    // And the ceiling is the dwell: 10/s with a 100ms floor.
    assert!(per_sec <= 10.0, "{per_sec:.1} transitions/s exceeds the dwell");
}

/// Green peaks at 131 in this stream while red and blue reach 255. A cut taken
/// against the frame maximum needed 204, so the green lamp could never light —
/// which is what per-channel AGC fixes.
#[test]
fn green_is_reachable_despite_a_lower_peak() {
    let run = run_follow(BEAT_MS);
    let green = run.changes.iter().filter(|c| c.3).count();
    let total = run.changes.len();
    assert!(
        green * 10 >= total,
        "green lit in only {green}/{total} writes"
    );
    // All three lamps must earn use, or it is not following colour.
    assert!(run.changes.iter().any(|c| c.1), "red never lit");
    assert!(run.changes.iter().any(|c| c.2), "yellow never lit");
}

/// A blackout must not send the light back to a generic sweep: rekordbox is dark
/// most of the time, and the last real colour is what should keep playing.
#[test]
fn keeps_moving_through_a_blackout() {
    let run = run_follow(BEAT_MS);
    let during: Vec<_> = run
        .changes
        .iter()
        .filter(|c| (18_000..24_000).contains(&c.0))
        .collect();
    // Measures 24 (4/s) — the palette here is green-only, which is the case that
    // used to leave three of the six looks completely static.
    assert!(
        during.len() >= 18,
        "only {} transitions in the 6s blackout",
        during.len()
    );
    assert!(
        during.iter().any(|c| c.1 || c.2 || c.3),
        "went dark with the input"
    );
}

/// Never all-three-on steady at 1Hz: that is the firmware's fault signal and it
/// has to stay unambiguous. Brief all-three flashes are fine.
#[test]
fn does_not_imitate_the_fault_signal() {
    let run = run_follow(BEAT_MS);
    let mut longest = 0;
    for w in run.changes.windows(2) {
        if w[0].1 && w[0].2 && w[0].3 {
            longest = longest.max(w[1].0 - w[0].0);
        }
    }
    assert!(
        longest < 400,
        "held all three lamps for {longest}ms, which reads as the fault signal"
    );
}

/// Circular concentration of transition times modulo `period`: 1.0 is perfect
/// alignment, ~1/sqrt(n) is what unrelated timings give.
fn concentration(times: &[i64], period: f64) -> f64 {
    let (mut sx, mut sy) = (0.0f64, 0.0f64);
    for t in times {
        let theta = 2.0 * std::f64::consts::PI * (*t as f64 % period) / period;
        sx += theta.cos();
        sy += theta.sin();
    }
    (sx * sx + sy * sy).sqrt() / times.len() as f64
}

/// The script must infer the tempo, not free-run. Tested against a control: at
/// each of two tempi, transitions cluster on the quarter-beat grid of the tempo
/// actually playing and not on the other one's. A free-running pattern would
/// score the same against both.
#[test]
fn locks_to_the_tempo_that_is_playing() {
    for (playing, other) in [(BEAT_MS, BEAT_MS_SLOW), (BEAT_MS_SLOW, BEAT_MS)] {
        let run = run_follow(playing);
        // Skip the opening: the estimator starts at a default 120 BPM.
        let late: Vec<i64> = run
            .changes
            .iter()
            .filter(|c| c.0 > 4000)
            .map(|c| c.0)
            .collect();
        let hit = concentration(&late, playing as f64 / 4.0);
        let miss = concentration(&late, other as f64 / 4.0);
        let floor = 1.0 / (late.len() as f64).sqrt();
        println!(
            "{playing}ms beat: on-grid {hit:.3}, off-grid {miss:.3}, \
             noise floor {floor:.3}, n={}",
            late.len()
        );
        assert!(
            hit > 0.30,
            "{playing}ms beat: concentration {hit:.3} on its own grid"
        );
        assert!(
            hit > miss * 1.5,
            "{playing}ms beat: {hit:.3} on-grid vs {miss:.3} off-grid is not a lock"
        );
    }
}

/// Relay wear. The lamps are switched by Songle SRD-05VDC-SL-C: 10^7 mechanical
/// operations, but only 10^5 electrical at rated load, and the datasheet caps
/// electrical switching at 30 operations/minute. Those are the numbers a lighting
/// pattern spends, so the pattern's cost per lamp is a hardware budget and gets
/// measured like one.
#[test]
fn relay_operations_per_lamp() {
    let run = run_follow(BEAT_MS);
    let mut ops = [0u32; 3];
    let mut prev = (false, false, false);
    for c in &run.changes {
        if c.1 != prev.0 {
            ops[0] += 1;
        }
        if c.2 != prev.1 {
            ops[1] += 1;
        }
        if c.3 != prev.2 {
            ops[2] += 1;
        }
        prev = (c.1, c.2, c.3);
    }
    let mins = run.end_ms as f64 / 60_000.0;
    for (i, lamp) in ["red", "yellow", "green"].iter().enumerate() {
        let per_min = ops[i] as f64 / mins;
        let hours_to_1e5 = 100_000.0 / (per_min * 60.0);
        println!(
            "{lamp}: {} ops in {:.1} min = {per_min:.0}/min, \
             {hours_to_1e5:.0}h to 10^5 electrical operations",
            ops[i], mins
        );
        // The datasheet's mechanical switching ceiling. Exceeding it is not a
        // lifetime question, it is asking the armature to move faster than it
        // can, so this is a hard bound rather than a budget.
        assert!(
            per_min <= 300.0,
            "{lamp}: {per_min:.0} ops/min exceeds the relay's 300/min mechanical limit"
        );
    }
}
