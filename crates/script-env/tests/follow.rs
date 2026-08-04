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

/// The DMX rate. The bridge sends on change plus a keepalive, so what the light
/// actually receives was measured at 9.8-11.5Hz.
const FRAME_MS: i64 = 24;
const FRAME_MS_REAL: i64 = 95;
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

/// A stream matching the *measured* statistics of a real set rather than a
/// convenient one: dark 72% of the time, accents on some beats and not others.
/// `synth` has a kick on every beat, which flatters the beat estimator and hides
/// what happens when onsets are scarce.
fn synth_sparse(t: i64, beat_ms: i64) -> Vec<u8> {
    let beat = t / beat_ms;
    let tb = t % beat_ms;
    // Deterministic per-beat roll, so the stream is reproducible.
    let h = (beat as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 33;

    // A hit on ~55% of beats, 120ms long: ~26% duty, against a measured 27.9%.
    let hit = h % 100 < 55 && tb < 120;
    let decay = if hit { 1.0 - tb as f32 / 120.0 } else { 0.0 };

    // Every 8th beat, a two-beat pad wash on green only — the case that has to
    // keep the light alive without any onset to trigger from.
    let wash = beat % 8 >= 6;

    let r = if hit && h % 3 != 0 { 255.0 * decay } else { 0.0 };
    let b = if hit && h % 3 == 0 { 200.0 * decay } else { 0.0 };
    let g = if wash { 131.0 } else if hit { 90.0 * decay } else { 0.0 };
    vec![r as u8, g as u8, b as u8]
}

struct Run {
    /// (time_ms, r, y, g) for every set_lights the script made.
    changes: Vec<(i64, bool, bool, bool)>,
    end_ms: i64,
}

fn run_follow(beat_ms: i64) -> Run {
    run_with(beat_ms, synth)
}

fn run_with(beat_ms: i64, gen: fn(i64, i64) -> Vec<u8>) -> Run {
    run_at(beat_ms, gen, FRAME_MS)
}

fn run_at(beat_ms: i64, gen: fn(i64, i64) -> Vec<u8>, frame_ms: i64) -> Run {
    run_delivered(beat_ms, gen, frame_ms, |t| t)
}

/// `arrive` maps a frame's generation time to its arrival time, modelling the
/// network. Identity is clean delivery; bunching maps several generation times
/// to one flush instant, which the socket then coalesces to the newest — with
/// `seq` derived from the generation index, exactly like the bridge's per-sent-
/// packet counter, so the script can see how many frames a gap swallowed.
fn run_delivered(
    beat_ms: i64,
    gen: fn(i64, i64) -> Vec<u8>,
    frame_ms: i64,
    arrive: fn(i64) -> i64,
) -> Run {
    let clock = Arc::new(AtomicI64::new(0));
    let changes = Arc::new(Mutex::new(Vec::new()));
    // Newest frame index already delivered.
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
                    let k = last_k.load(Ordering::SeqCst) + 1;
                    if arrive(k * frame_ms) > end {
                        // Nothing within the window.
                        clock.store(end, Ordering::SeqCst);
                        return Ok(None);
                    }
                    let at = arrive(k * frame_ms).max(now);
                    // Coalesce exactly like the socket: everything already
                    // arrived is discarded and only the newest is delivered.
                    let mut newest = k;
                    while arrive((newest + 1) * frame_ms) <= at {
                        newest += 1;
                    }
                    last_k.store(newest, Ordering::SeqCst);
                    clock.store(at, Ordering::SeqCst);
                    Ok(Some(DmxFrame {
                        seq: newest + 1,
                        base: 1,
                        channels: gen(newest * frame_ms, beat_ms),
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
    // Both the synthetic stream and the one measured off the wire, since the
    // hardware runs the latter and its cost is the one that has to fit.
    for (name, run) in [
        ("kick every beat", run_follow(BEAT_MS)),
        ("measured", run_at(BEAT_MS, synth_measured, FRAME_MS_REAL)),
    ] {
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
            "{name} / {lamp}: {} ops in {:.1} min = {per_min:.0}/min, \
             {hours_to_1e5:.0}h to 10^5 electrical operations",
            ops[i], mins
        );
        // Reporting, not a gate: the wear budget is explicitly spent for
        // density. The dwell keeps each relay under its mechanical settle
        // limit; this print is what the cost is.
        let ceiling = 60_000.0 / DWELL_MS as f64;
        assert!(
            per_min <= ceiling,
            "{name} / {lamp}: {per_min:.0} ops/min got past the {DWELL_MS}ms dwell gate"
        );
    }
    }
}

/// Reproduces the complaint: sparse, mostly-dark input. `synth` hides this by
/// putting a kick on every beat.
#[test]
fn stays_dense_on_a_sparse_stream() {
    let run = run_with(BEAT_MS, synth_sparse);
    let lit = run.changes.iter().filter(|c| c.1 || c.2 || c.3).count();
    let per_sec = run.changes.len() as f64 / (run.end_ms as f64 / 1000.0);

    // Longest stretch with no transition at all.
    let mut gap = 0;
    for w in run.changes.windows(2) {
        gap = gap.max(w[1].0 - w[0].0);
    }
    let duty: usize = {
        let mut on = 0;
        for w in run.changes.windows(2) {
            if w[0].1 || w[0].2 || w[0].3 {
                on += (w[1].0 - w[0].0) as usize;
            }
        }
        on * 100 / run.end_ms as usize
    };
    println!(
        "sparse stream: {per_sec:.1} transitions/s, {}% of writes light something, \
         lamps lit {duty}% of the time, longest still gap {gap}ms",
        lit * 100 / run.changes.len()
    );
    assert!(per_sec >= 4.0, "only {per_sec:.1} transitions/s on a sparse stream");
    assert!(gap <= 600, "light stood still for {gap}ms");
}

/// What rekordbox actually sends, measured off the wire: a 3-channel RGB par
/// driven to **one saturated colour at a time**. Two channels sit at zero and the
/// live one rotates every few bars. Colour-faithful mapping onto three coloured
/// lamps therefore lights one lamp at a time, which is inherently sparse — this
/// is the stream that exposed it.
fn synth_saturated(t: i64, beat_ms: i64) -> Vec<u8> {
    let beat = t / beat_ms;
    let tb = t % beat_ms;
    let h = (beat as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 33;
    // Lit 40% of the time, matching the measurement.
    let lit = h % 100 < 62 && tb < 190;
    let level = if lit {
        (255.0 * (1.0 - tb as f32 / 260.0)) as u8
    } else {
        0
    };
    // One channel at a time; the colour changes every 16 beats.
    match (beat / 16) % 3 {
        0 => vec![level, 0, 0],
        1 => vec![0, 0, level],
        _ => vec![0, level, 0],
    }
}

/// The case the hardware is actually in: one saturated colour at a time, arriving
/// at ~10Hz. The light must still use the whole fixture and stay busy.
#[test]
fn stays_dense_on_one_saturated_colour_at_10hz() {
    let run = run_at(BEAT_MS, synth_saturated, FRAME_MS_REAL);
    let per_sec = run.changes.len() as f64 / (run.end_ms as f64 / 1000.0);
    let mut gap = 0;
    for w in run.changes.windows(2) {
        gap = gap.max(w[1].0 - w[0].0);
    }
    let used = [
        run.changes.iter().filter(|c| c.1).count(),
        run.changes.iter().filter(|c| c.2).count(),
        run.changes.iter().filter(|c| c.3).count(),
    ];
    println!(
        "saturated @10Hz: {per_sec:.1} transitions/s, longest still gap {gap}ms, \
         lamp use r={} y={} g={} of {}",
        used[0], used[1], used[2], run.changes.len()
    );
    assert!(per_sec >= 4.0, "only {per_sec:.1} transitions/s");
    assert!(gap <= 600, "light stood still for {gap}ms");
    // A single live colour channel must not reduce the fixture to one lamp.
    for (i, lamp) in ["red", "yellow", "green"].iter().enumerate() {
        assert!(
            used[i] * 20 >= run.changes.len(),
            "{lamp} used in only {}/{} writes",
            used[i],
            run.changes.len()
        );
    }
}

/// The stream as *measured* off the wire with both fixtures patched — an RGB par
/// at 1-3 and an 8-channel moving head at 4-11.
///
/// The measurements that matter, and they contradict what the design assumed:
/// nothing rekordbox sends moves at beat rate. The colour channels fade over more
/// than 9s (0.5 rises/s) and pan/tilt are perfectly smooth 9s sweeps with *zero*
/// rises. The moving head's dimmer is the only real rhythm source at 1.3 rises/s,
/// on a 3350ms cycle — roughly two bars, not a beat. So the pattern's density
/// cannot come from the stream; only its colour, energy and phrase position can.
fn synth_measured(t: i64, _beat_ms: i64) -> Vec<u8> {
    let f = t as f32 / 1000.0;
    let tau = std::f32::consts::TAU;
    // Colour: three slow fades at >9s, peaking where the real ones did.
    let par_r = 92.0 * (0.5 + 0.5 * (tau * f / 11.0).sin()).max(0.0);
    let par_g = 131.0 * (0.5 + 0.5 * (tau * f / 9.5 + 2.0).sin()).max(0.0);
    let par_b = 180.0 * (0.5 + 0.5 * (tau * f / 10.5 + 4.0).sin()).max(0.0);
    // Dimmer: a 3350ms envelope that swells, plateaus, falls and rests.
    let ph = (t % 3350) as f32 / 3350.0;
    let dim = if ph < 0.25 {
        ph / 0.25
    } else if ph < 0.5 {
        1.0
    } else if ph < 0.7 {
        1.0 - (ph - 0.5) / 0.2
    } else {
        0.0
    };
    // Pan/tilt: smooth 9s sweeps, no steps at all.
    let pan = 199.0 * (0.5 + 0.5 * (tau * f / 9.0).sin());
    let tilt = 64.0 * (0.5 + 0.5 * (tau * f / 9.0 + 1.5).sin());
    // The strobe fixture at 12-15: rekordbox toggles its RGB white on a ~4s cycle
    // and never touches its Dimmer/Strobe channel. That toggle is the only
    // section-level gate in the stream.
    let gate = if (t / 4000) % 2 == 0 { 253 } else { 0 };
    vec![
        par_r as u8, par_g as u8, par_b as u8,
        0,                       // MH mode, rekordbox cannot drive it
        (255.0 * dim) as u8,     // MH dimmer
        0,                       // MH strobe, never driven
        pan as u8, tilt as u8,
        0, 0, 255,               // MH rgb: pinned blue
        gate, gate, gate,        // strobe rgb
        0,                       // strobe Dimmer/Strobe, never driven
        0,                       // bar matrix WW1, rekordbox cannot drive it
    ]
}

/// Against real measured input the light must still be busy, even though the
/// stream contains no beat-rate information for it to follow.
#[test]
fn stays_dense_on_the_measured_stream() {
    let run = run_at(BEAT_MS, synth_measured, FRAME_MS_REAL);
    let per_sec = run.changes.len() as f64 / (run.end_ms as f64 / 1000.0);
    let mut gap = 0;
    for w in run.changes.windows(2) {
        gap = gap.max(w[1].0 - w[0].0);
    }
    let used = [
        run.changes.iter().filter(|c| c.1).count(),
        run.changes.iter().filter(|c| c.2).count(),
        run.changes.iter().filter(|c| c.3).count(),
    ];
    let mut width = [0usize; 4];
    for c in &run.changes {
        width[c.1 as usize + c.2 as usize + c.3 as usize] += 1;
    }
    let n = run.changes.len();
    println!(
        "measured stream: {per_sec:.1} transitions/s, longest still gap {gap}ms, \
         lamp use r={} y={} g={} of {n}; width 0/1/2/3 = {}%/{}%/{}%/{}%",
        used[0], used[1], used[2],
        width[0] * 100 / n, width[1] * 100 / n, width[2] * 100 / n, width[3] * 100 / n
    );
    // Widths 1-3 must all appear, and multi-lamp looks must dominate. Width 0
    // is no longer required: blackout rest phases were deliberately removed —
    // the light rests at one lamp, not at nothing.
    for k in 1..4 {
        assert!(
            width[k] * 50 >= n,
            "width {k} occurred in only {}/{n} writes — the light should use all of them",
            width[k]
        );
    }
    assert!(
        (width[2] + width[3]) * 100 >= n * 40,
        "only {}% of writes light 2+ lamps",
        (width[2] + width[3]) * 100 / n
    );
    assert!(per_sec >= 4.0, "only {per_sec:.1} transitions/s on real input");
    assert!(gap <= 700, "light stood still for {gap}ms");
    for (i, lamp) in ["red", "yellow", "green"].iter().enumerate() {
        assert!(
            used[i] * 20 >= run.changes.len(),
            "{lamp} used in only {}/{} writes",
            used[i],
            run.changes.len()
        );
    }
}

/// The stream measured live on 2026-08-04 with a loop on the deck, which
/// contradicts `synth_measured`'s premise: rekordbox *does* move at beat rate
/// for this content. The MH dimmer pumps full-scale at ~13 changes/s, par R/B
/// fade at ~9/s, pan sweeps fast (~10.6/s), the strobe section gate stays cold,
/// and the par goes fully dark for long stretches.
fn synth_live_loop(t: i64, beat_ms: i64) -> Vec<u8> {
    let f = t as f32 / 1000.0;
    let tau = std::f32::consts::TAU;
    let tb = t % beat_ms;

    // Dimmer: a full-scale decay restruck every beat — the rhythm source.
    let dim = 255.0 * (1.0 - 0.8 * tb as f32 / beat_ms as f32);

    // Par: fades over a few seconds, G capped at 131; dead mid-stream.
    let dead = (20_000..38_000).contains(&t);
    let (r, g, b) = if dead {
        (0.0, 0.0, 0.0)
    } else {
        (
            255.0 * (0.5 + 0.5 * (tau * f / 3.1).sin()).max(0.0),
            131.0 * (0.5 + 0.5 * (tau * f / 2.3 + 2.0).sin()).max(0.0),
            255.0 * (0.5 + 0.5 * (tau * f / 3.7 + 4.0).sin()).max(0.0),
        )
    };

    // Pan: a fast full sweep every 2.6s — the shape that used to re-roll the
    // look every PHRASE_MIN and let nothing settle.
    let pan = 100.0 + 99.0 * (tau * f / 2.6).sin();
    let tilt = 64.0 * (0.5 + 0.5 * (tau * f / 3.3).sin());

    vec![
        r as u8, g as u8, b as u8,
        0,
        dim as u8,
        0,
        pan as u8, tilt as u8,
        0, 0, 255,
        0, 0, 0, // strobe RGB cold: the gate must not shrink the width
        0, 0,
    ]
}

/// Delivery as measured: stretches of clean arrival alternating with wifi
/// bunching, where everything generated in a ~300ms window lands at one flush
/// instant and coalesces to a single frame with a seq jump.
fn arrive_bunched(t: i64) -> i64 {
    if (t / 2000) % 2 == 0 {
        t
    } else {
        (t + 299) / 300 * 300
    }
}

/// ~131 BPM, the loop measured on the deck.
const BEAT_MS_LOOP: i64 = 457;
/// The bridge logged 23.2 changed-frames/s on that loop.
const FRAME_MS_LOOP: i64 = 43;

/// The failure measured live: onset intervals clustered at the 300ms network
/// burst gap and the tempo locked to ~107 BPM against ~131.5 BPM playing. The
/// light must beat-lock to the music, not to wifi delivery — burst-boundary
/// energy jumps carry a seq gap and must not vote on the tempo.
#[test]
fn locks_to_the_music_not_the_wifi_bursts() {
    let run = run_delivered(BEAT_MS_LOOP, synth_live_loop, FRAME_MS_LOOP, arrive_bunched);
    let late: Vec<i64> = run
        .changes
        .iter()
        .filter(|c| c.0 > 8000)
        .map(|c| c.0)
        .collect();
    let hit = concentration(&late, BEAT_MS_LOOP as f64 / 4.0);
    let burst = concentration(&late, 300.0 / 2.0);
    let floor = 1.0 / (late.len() as f64).sqrt();
    println!(
        "live loop, bunched delivery: on-beat {hit:.3}, on-burst-grid {burst:.3}, \
         noise floor {floor:.3}, n={}",
        late.len()
    );
    // Half the delivery windows here carry no usable timing at all (their
    // frames land at one flush), so the absolute lock is bounded well below the
    // clean-delivery 0.3+; measured 0.17 against a 0.05 noise floor. The claim
    // is the *relative* one: on the music's grid, not the network's.
    assert!(
        hit > 0.12,
        "concentration {hit:.3} on the musical grid — not locked to the music"
    );
    assert!(
        hit > burst * 2.0,
        "on-beat {hit:.3} vs on-burst {burst:.3}: locked to wifi delivery, not music"
    );
}

/// The other half of the live complaint: width pinned to one lamp by the cold
/// section gate, and a frozen red-first ranking while the par was dark. On this
/// stream the light must run wide, never go still, and keep using all three
/// lamps through the dead-colour stretch.
#[test]
fn stays_wide_and_alive_on_the_live_loop() {
    let run = run_delivered(BEAT_MS_LOOP, synth_live_loop, FRAME_MS_LOOP, arrive_bunched);
    let n = run.changes.len();
    let per_sec = n as f64 / (run.end_ms as f64 / 1000.0);

    let mut width = [0usize; 4];
    for c in &run.changes {
        width[c.1 as usize + c.2 as usize + c.3 as usize] += 1;
    }
    let mut still = 0;
    let mut dark = 0;
    for w in run.changes.windows(2) {
        still = still.max(w[1].0 - w[0].0);
        if !(w[0].1 || w[0].2 || w[0].3) {
            dark = dark.max(w[1].0 - w[0].0);
        }
    }
    // Lamp use during the dead-colour stretch, where the ranking must rotate
    // rather than freeze red-first.
    let dead: Vec<_> = run
        .changes
        .iter()
        .filter(|c| (20_000..38_000).contains(&c.0))
        .collect();
    let dead_used = [
        dead.iter().filter(|c| c.1).count(),
        dead.iter().filter(|c| c.2).count(),
        dead.iter().filter(|c| c.3).count(),
    ];
    println!(
        "live loop: {per_sec:.1} transitions/s; width 0/1/2/3 = {}%/{}%/{}%/{}%; \
         longest still {still}ms, longest dark {dark}ms; \
         dead-colour lamp use r={} y={} g={} of {}",
        width[0] * 100 / n, width[1] * 100 / n, width[2] * 100 / n, width[3] * 100 / n,
        dead_used[0], dead_used[1], dead_used[2], dead.len()
    );
    assert!(per_sec >= 4.0, "only {per_sec:.1} transitions/s");
    assert!(
        (width[2] + width[3]) * 100 >= n * 40,
        "only {}% of writes light 2+ lamps — the one-lamp ceiling is back",
        (width[2] + width[3]) * 100 / n
    );
    assert!(still <= 600, "light stood still for {still}ms");
    assert!(dark <= 500, "light sat dark for {dark}ms");
    for (i, lamp) in ["red", "yellow", "green"].iter().enumerate() {
        assert!(
            dead_used[i] * 5 >= dead.len(),
            "{lamp} used in only {}/{} writes while colour was dead — ranking froze",
            dead_used[i],
            dead.len()
        );
    }
}
