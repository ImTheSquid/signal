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

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
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

/// The stub RNG is fixed-seed, so one run of a stochastic script measures one
/// draw. Anything asserted on the *look* is checked across these instead.
const SEEDS: [u32; 5] = [0x9E37_79B9, 0x1234_5678, 0xDEAD_BEEF, 0xA5A5_A5A5, 0x3141_5927];

/// Long enough to populate a one-beat context table. At 60s the order-4 table is
/// undertrained and scores *below* the floor whatever the script does, so the
/// assertion built on it had no teeth — the six-look version passed it.
const OBSERVE_MS: i64 = 300_000;

fn run_pulse_seeded(period_ms: i64, seed: u32) -> Run {
    run_seeded(period_ms, |_| true, 200, Mode::Tree, seed, OBSERVE_MS)
}

fn run_as(period_ms: i64, deliver: fn(i64) -> bool, energy: u8, mode: Mode) -> Run {
    run_seeded(period_ms, deliver, energy, mode, SEEDS[0], RUN_MS)
}

/// `deliver` decides which packets survive the network.
fn run_seeded(
    period_ms: i64,
    deliver: fn(i64) -> bool,
    energy: u8,
    mode: Mode,
    seed: u32,
    run_ms: i64,
) -> Run {
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
            random_u32: Box::new({
                let state = AtomicU32::new(seed | 1);
                move || {
                    let mut x = state.load(Ordering::Relaxed);
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    state.store(x, Ordering::Relaxed);
                    x
                }
            }),
            ..Handlers::stubs()
        },
    );

    engine.on_progress({
        let clock = clock.clone();
        move |_| {
            if clock.load(Ordering::SeqCst) >= run_ms {
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

/// How often a run of `w` beats renders exactly what the previous run of `w`
/// beats did.
///
/// Distinct from [`repeat_rate`], and the one that matters for whether a look is
/// readable: beat-to-beat repetition is what makes a pattern feel deliberate, so
/// some of it is wanted. A whole sentence repeating verbatim is what lets the eye
/// run ahead of the light.
fn window_repeat_rate(run: &Run, period_ms: i64, w: i64) -> f64 {
    let first = 10_000 / period_ms;
    let last = RUN_MS / period_ms - 1;
    let mut same = 0;
    let mut total = 0;
    for beat in first + w..last {
        let here: Vec<Vec<u8>> = (0..w).map(|k| beat_shape(run, period_ms, beat + k)).collect();
        let prev: Vec<Vec<u8>> = (0..w)
            .map(|k| beat_shape(run, period_ms, beat - w + k))
            .collect();
        if here == prev {
            same += 1;
        }
        total += 1;
    }
    same as f64 / total as f64
}

/// Share of gaps between changes that fall in the single commonest bucket.
///
/// This is the cadence, and it is what gives a look away. A pattern that changes
/// something at every quarter of every beat is metronomic whatever the lamps are
/// doing: the eye stops predicting *which* and starts predicting *when*, and it
/// is right every time. Spread across several gap lengths is syncopation, space,
/// and density that moves.
fn modal_gap_share(run: &Run) -> f64 {
    let mut hist = std::collections::HashMap::new();
    let mut total = 0;
    for w in run.changes.windows(2) {
        if w[0].0 < 10_000 {
            continue;
        }
        *hist.entry((w[1].0 - w[0].0) / 20).or_insert(0) += 1;
        total += 1;
    }
    let top = hist.values().copied().max().unwrap_or(0);
    top as f64 / total.max(1) as f64
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

/// Lamp state on the quarter-beat grid, paired with which quarter it is — the
/// symbol stream an observer actually sees.
fn symbols(run: &Run, period_ms: i64) -> Vec<(u8, usize)> {
    let q = period_ms / 4;
    ((10_000 / q)..(OBSERVE_MS / q))
        .map(|i| (state_at(&run.changes, i * q + q / 2), (i % 4) as usize))
        .collect()
}

/// An observer who watches half the run and predicts the rest, against one who
/// only ever guesses the commonest state. Their **ratio** is what matters: an
/// absolute accuracy moves with the state distribution, so a look that simply
/// stayed lit more would score better without being less readable.
///
/// Held out on purpose. Scoring a high-order context on the data that built it
/// measures how many contexts there are, not how predictable the light is.
fn learned_over_floor(run: &Run, period_ms: i64, order: usize, use_slot: bool) -> (f64, f64) {
    let seq = symbols(run, period_ms);
    let split = seq.len() / 2;
    let key = |w: &[(u8, usize)]| -> (Vec<u8>, usize) {
        (
            w[..order].iter().map(|x| x.0).collect(),
            if use_slot { w[order].1 } else { 0 },
        )
    };

    let mut table: HashMap<(Vec<u8>, usize), HashMap<u8, u32>> = HashMap::new();
    let mut marg: HashMap<u8, u32> = HashMap::new();
    for w in seq[..split].windows(order + 1) {
        *table.entry(key(w)).or_default().entry(w[order].0).or_insert(0) += 1;
        *marg.entry(w[order].0).or_insert(0) += 1;
    }
    let mode = |c: &HashMap<u8, u32>| c.iter().max_by_key(|(_, &n)| n).map(|(&s, _)| s);
    let fallback = mode(&marg).unwrap_or(0);

    let (mut hits, mut total) = (0u32, 0u32);
    for w in seq[split..].windows(order + 1) {
        let guess = table.get(&key(w)).and_then(mode).unwrap_or(fallback);
        hits += u32::from(guess == w[order].0);
        total += 1;
    }

    let mut all: HashMap<u8, u32> = HashMap::new();
    for &(s, _) in &seq[split..] {
        *all.entry(s).or_insert(0) += 1;
    }
    let floor = all.values().copied().max().unwrap_or(0) as f64 / seq[split..].len() as f64;
    (hits as f64 / total as f64, floor)
}

/// The one that answers the complaint. Everything else here is a marginal
/// statistic — how often something happens; this is conditional — how much the
/// last thing tells you about the next, which is the only thing an observer is
/// doing.
///
/// The six fixed looks measured 1.80x the floor: three or four slots identified
/// which look was running and the look determined the rest, so nothing repeated
/// verbatim and it was still readable. One parametric generator whose rhythm,
/// subset and walk are re-drawn per cycle measures 1.01x.
#[test]
fn an_observer_learns_nothing_by_watching() {
    let mut worst = 0.0f64;
    for seed in SEEDS {
        let run = run_pulse_seeded(BEAT_MS, seed);
        let (acc, floor) = learned_over_floor(&run, BEAT_MS, 4, false);
        // Timing separately: a fixed cadence shows up here and nowhere else.
        let (with_slot, _) = learned_over_floor(&run, BEAT_MS, 3, true);
        let ratio = acc / floor;
        println!(
            "seed {seed:#010x}: order-4 {:.0}% vs floor {:.0}% = {ratio:.2}x, \
             +slot {:.0}%",
            acc * 100.0,
            floor * 100.0,
            with_slot * 100.0
        );
        worst = worst.max(ratio);
    }
    assert!(
        worst < 1.35,
        "an observer with one beat of memory beats the floor by {worst:.2}x"
    );
}

/// Measured, so "it gets bland" stops being a matter of opinion.
///
/// Beat-to-beat repetition is deliberately **not** the thing held down. Whole
/// sentences never repeat, so verbatim repetition was never what made the look
/// readable, and some of it is what makes a pattern feel deliberate rather than
/// twitchy — re-drawing the rhythm every single cycle took it to 0.000, which
/// reads as flicker. It rides one to three cycles now.
///
/// The cadence bar is deliberately looser than the 0.52 it was set at. It was a
/// proxy for predictability adopted when there was no direct measure; there is
/// one now, in `an_observer_learns_nothing_by_watching`, and the two pull against
/// each other — a sparser rhythm spreads the gaps and simultaneously makes the
/// next slot easier to guess, because holding still is predictable. Measured:
/// biasing toward sparse moved cadence 0.51 -> 0.49 and predictability 1.01x ->
/// 1.17x. Optimising the proxy at the direct measure's expense is the wrong
/// trade, so this stays as a guard against a genuine metronome and no more. The
/// concern it encoded — that the eye gives up on *which* and predicts *when* —
/// is now tested directly by the `+slot` context, which buys nothing.
#[test]
fn the_pattern_breaks_itself_up() {
    let run = run_pulse(BEAT_MS);
    let repeat = repeat_rate(&run, BEAT_MS);
    let hold = longest_hold_beats(&run, BEAT_MS);
    let s4 = window_repeat_rate(&run, BEAT_MS, 4);
    let s8 = window_repeat_rate(&run, BEAT_MS, 8);
    let cadence = modal_gap_share(&run);
    println!(
        "repeat {repeat:.3}  4-beat {s4:.3}  8-beat {s8:.3}  hold {hold:.2}  cadence {cadence:.3}"
    );
    assert!(cadence < 0.55, "{cadence:.3} of gaps are the same length");
    // Loose, and only to catch a look that has stopped saying anything at all.
    assert!(repeat < 0.40, "every {repeat:.3} of beats repeats the last one");
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
    // The cap is the dwell gate, not a target: 300/min is what the relay's
    // mechanical rating allows, and the printed per-lamp figures are what the
    // wear budget in the README is priced from.
    let worst = r.max(y).max(g) as f64 / minutes;
    assert!(worst < 300.0, "{worst:.0} operations/minute per lamp");
}
