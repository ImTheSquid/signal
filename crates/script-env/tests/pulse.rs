//! `pulse.rhai` against a synthetic beat block.
//!
//! The daemon does the listening, so what is left to check here is the part the
//! light owns: that it reads the block, runs the grid forward on its own between
//! packets, keeps the relays inside their dwell, and lands on the beat.
//!
//! The block layout is mirrored from `crates/audio-bridge/src/wire.rs`. It lives
//! in two places because the two crates have separate build roots — the daemon
//! needs the macOS libclang that the firmware's espup toolchain would otherwise
//! override.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use script_env::{rhai, DmxFrame, Handlers};

const SCRIPT: &str = include_str!("../../../scripts/pulse.rhai");

/// `AUDIO_BASE` in the daemon: above the 512 DMX channels, so it cannot be
/// confused with a frame from the rekordbox bridge.
const AUDIO_BASE: i64 = 0xFFFE;

/// The daemon's baseline cadence.
const PACKET_MS: i64 = 100;
const DWELL_MS: i64 = 100;
const RUN_MS: i64 = 60_000;

const BEAT_MS: i64 = 469; // 128 BPM
const BEAT_MS_SLOW: i64 = 600; // 100 BPM

struct Run {
    changes: Vec<(i64, bool, bool, bool)>,
}

/// One 16-byte beat block, as the daemon would encode it at time `t`.
fn beat_block(t: i64, period_ms: i64, energy: u8) -> Vec<u8> {
    let to_next = (period_ms - t.rem_euclid(period_ms)).clamp(0, 65_534) as u16;
    let mut b = vec![0u8; 16];
    b[0] = 1; // fmt
    b[1] = 0b0000_0011; // audio_present | tracking
    b[2..4].copy_from_slice(&to_next.to_be_bytes());
    b[4..6].copy_from_slice(&(period_ms as u16).to_be_bytes());
    b[6] = ((t / period_ms) % 16) as u8;
    b[7] = 200; // confidence
    b[8] = energy;
    b[9] = 220; // low
    b[10] = 120; // mid
    b[11] = 60; // high
    b[13] = 255; // no beat age
    b[15] = 128; // flat build
    b
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Tree,
    Artifact,
}

fn run_pulse(period_ms: i64) -> Run {
    run_as(period_ms, |_| true, 200, Mode::Tree)
}

/// `deliver` decides which packets survive the network; `stop_ms` is when the
/// stream goes silent for good.
fn run_as(period_ms: i64, deliver: fn(i64) -> bool, energy: u8, mode: Mode) -> Run {
    let clock = Arc::new(AtomicI64::new(0));
    let changes = Arc::new(Mutex::new(Vec::new()));
    let last_k = Arc::new(AtomicI64::new(-1));

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
                let last_k = last_k.clone();
                move |timeout| {
                    let now = clock.load(Ordering::SeqCst);
                    let end = now + timeout;
                    let mut k = last_k.load(Ordering::SeqCst) + 1;
                    while k * PACKET_MS <= end && !deliver(k) {
                        k += 1;
                    }
                    let at = k * PACKET_MS;
                    if at > end {
                        clock.store(end, Ordering::SeqCst);
                        return Ok(None);
                    }
                    last_k.store(k, Ordering::SeqCst);
                    let at = at.max(now);
                    clock.store(at, Ordering::SeqCst);
                    Ok(Some(DmxFrame {
                        seq: k + 1,
                        base: AUDIO_BASE as i64,
                        channels: beat_block(at, period_ms, energy),
                    }))
                }
            }),
            lamp_dwell_ms: Box::new(|| DWELL_MS),
            ..Handlers::stubs()
        },
    );

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

    let outcome = match mode {
        Mode::Tree => engine.run(SCRIPT),
        Mode::Artifact => {
            let ast = engine.compile(SCRIPT).expect("pulse.rhai must compile");
            let artifact = script_env::lower(&ast).expect("pulse.rhai must lower");
            assert_eq!(artifact.residual, 0, "pulse.rhai must lower whole");
            script_env::run_artifact(&engine, &artifact.program)
        }
    };
    match outcome {
        Ok(()) => panic!("pulse.rhai returned; it must loop"),
        Err(e) => match *e {
            rhai::EvalAltResult::ErrorTerminated(..) => {}
            other => panic!("pulse.rhai failed at runtime: {other}"),
        },
    }

    let changes = changes.lock().unwrap().clone();
    Run { changes }
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

/// The device runs bytecode, so the script has to survive lowering whole.
#[test]
fn lowers_to_bytecode_and_still_drives_the_lamps() {
    let run = run_as(BEAT_MS, |_| true, 200, Mode::Artifact);
    assert!(
        run.changes.len() > 50,
        "artifact produced only {} changes",
        run.changes.len()
    );
}

/// The script gates on `lamp_dwell_ms()` itself rather than letting `set_lights`
/// block, because a blocked call stalls the loop and drops packets.
#[test]
fn respects_the_relay_dwell() {
    let run = run_pulse(BEAT_MS);
    let mut worst = i64::MAX;
    for w in run.changes.windows(2) {
        worst = worst.min(w[1].0 - w[0].0);
    }
    assert!(
        worst >= DWELL_MS,
        "changed lamps {worst}ms apart, dwell is {DWELL_MS}ms"
    );
}

/// The point of the whole exercise. Transitions must cluster on the grid of the
/// tempo actually playing, and not on another one — a free-running pattern would
/// score alike against both.
///
/// For reference, `follow.rhai` on rekordbox DMX measures 0.364 here.
#[test]
fn lands_on_the_beat() {
    for (playing, other) in [(BEAT_MS, BEAT_MS_SLOW), (BEAT_MS_SLOW, BEAT_MS)] {
        let run = run_pulse(playing);
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
            hit > 0.85,
            "{playing}ms beat: concentration {hit:.3} on its own grid"
        );
        assert!(
            hit > miss * 1.5,
            "{playing}ms beat: {hit:.3} on-grid vs {miss:.3} off-grid is not a lock"
        );
    }
}

/// Sending a prediction rather than an event is what makes loss cheap: the
/// script knows the period and runs the grid forward itself. Three packets in
/// four are dropped here and the lock must survive.
#[test]
fn rides_through_packet_loss() {
    let run = run_as(BEAT_MS, |k| k % 4 == 0, 200, Mode::Tree);
    let late: Vec<i64> = run
        .changes
        .iter()
        .filter(|c| c.0 > 4000)
        .map(|c| c.0)
        .collect();
    let hit = concentration(&late, BEAT_MS as f64 / 4.0);
    println!("75% loss: on-grid {hit:.3}, n={}", late.len());
    // A shade under the clean bar, because level and colour still only update
    // when a packet lands and those feed the pattern width. The grid itself is
    // untouched, which is the property being tested — the noise floor is ~0.05.
    assert!(hit > 0.80, "lost the grid under packet loss: {hit:.3}");
}

/// Lamp states as a 3-bit code, at an instant.
fn state_at(changes: &[(i64, bool, bool, bool)], t: i64) -> u8 {
    let mut s = 0;
    for &(at, r, y, g) in changes {
        if at > t {
            break;
        }
        s = u8::from(r) | (u8::from(y) << 1) | (u8::from(g) << 2);
    }
    s
}

/// What a single beat renders, sampled across its length.
fn beat_shape(run: &Run, period_ms: i64, beat: i64) -> Vec<u8> {
    const SUB: i64 = 8;
    (0..SUB)
        .map(|k| state_at(&run.changes, beat * period_ms + k * period_ms / SUB))
        .collect()
}

/// How often a beat renders exactly what the beat before it did.
///
/// Predictability is a property of a lighting look, not only a matter of taste:
/// a pattern the eye can finish for itself stops being watched. Skips the first
/// ten seconds, which are startup rather than the look.
fn repeat_rate(run: &Run, period_ms: i64) -> f64 {
    let first = 10_000 / period_ms;
    let last = RUN_MS / period_ms - 1;
    let mut same = 0;
    let mut total = 0;
    for beat in first + 1..last {
        if beat_shape(run, period_ms, beat) == beat_shape(run, period_ms, beat - 1) {
            same += 1;
        }
        total += 1;
    }
    same as f64 / total as f64
}

/// The longest any one lamp stays continuously lit, in beats.
fn longest_hold_beats(run: &Run, period_ms: i64) -> f64 {
    let mut worst = 0;
    for lamp in 0..3 {
        let mut on_since: Option<i64> = None;
        for &(at, r, y, g) in &run.changes {
            let on = [r, y, g][lamp];
            match (on, on_since) {
                (true, None) => on_since = Some(at),
                (false, Some(start)) => {
                    worst = worst.max(at - start);
                    on_since = None;
                }
                _ => {}
            }
        }
    }
    worst as f64 / period_ms as f64
}

/// Measured, so "it gets bland" stops being a matter of opinion.
///
/// The pattern cycle at 128 BPM is exactly one beat long, so without the
/// four-beat group every look repeats itself once a beat: 0.337 of beats
/// rendered the previous beat exactly, and the eye finishes a one-beat sentence
/// after two of them. The group took that to 0.163.
#[test]
fn the_pattern_breaks_itself_up() {
    let run = run_pulse(BEAT_MS);
    let repeat = repeat_rate(&run, BEAT_MS);
    let hold = longest_hold_beats(&run, BEAT_MS);
    println!("repeat rate {repeat:.3}   longest hold {hold:.2} beats");
    assert!(repeat < 0.25, "every {repeat:.3} of beats repeats the last one");
    // Uniform business is most of what reads as mechanical. Something has to sit
    // still for the rest to move against.
    assert!(hold > 2.0, "no lamp ever holds longer than {hold:.2} beats");
}

/// A stalled daemon must not leave the light frozen.
#[test]
fn keeps_moving_when_the_stream_never_starts() {
    let run = run_as(BEAT_MS, |_| false, 200, Mode::Tree);
    assert!(
        run.changes.len() > 20,
        "only {} changes with no packets at all",
        run.changes.len()
    );
    let mut worst = 0;
    for w in run.changes.windows(2) {
        worst = worst.max(w[1].0 - w[0].0);
    }
    assert!(worst < 2_000, "went {worst}ms without moving");
}

/// Relay wear. The lamps are switched by Songle SRD-05VDC-SL-C: 10^7 mechanical
/// operations but only 10^5 electrical at rated load, and the datasheet caps
/// electrical switching at 30 operations/minute. A lighting pattern spends that
/// budget, so it gets measured like one.
#[test]
fn relay_operations_per_lamp() {
    let run = run_pulse(BEAT_MS);
    let (mut r, mut y, mut g) = (0, 0, 0);
    let (mut pr, mut py, mut pg) = (false, false, false);
    for &(_, cr, cy, cg) in &run.changes {
        r += i32::from(cr != pr);
        y += i32::from(cy != py);
        g += i32::from(cg != pg);
        pr = cr;
        py = cy;
        pg = cg;
    }
    let minutes = RUN_MS as f64 / 60_000.0;
    println!(
        "operations per minute: R {:.0}  Y {:.0}  G {:.0}",
        r as f64 / minutes,
        y as f64 / minutes,
        g as f64 / minutes
    );
    // follow.rhai measures 216/90/240. Staying near that keeps the wear budget
    // where it already was rather than quietly spending more of it.
    let worst = r.max(y).max(g) as f64 / minutes;
    assert!(worst < 300.0, "{worst:.0} operations/minute per lamp");
}
