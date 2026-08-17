//! Diagnostic probe, not an assertion. Answers "why does the look feel
//! deterministic when nothing repeats verbatim" by measuring what an observer
//! actually models: per-lamp duty, and how well the next slot is predicted from
//! the previous ones plus the position in the beat.
//!
//! Run: cargo test -p script-env --test entropy_probe -- --nocapture

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use script_env::{rhai, DmxFrame, Handlers};

const SCRIPT: &str = include_str!("../../../scripts/pulse.rhai");

const AUDIO_BASE: i64 = 0xFFFE;
const PACKET_MS: i64 = 100;
const DWELL_MS: i64 = 100;
const RUN_MS: i64 = 600_000; // ten minutes, so 3-symbol contexts are populated
const BEAT_MS: i64 = 469; // 128 BPM

/// Fixed bands are what `pulse.rs` sends. Varying bands ask how much of the
/// asymmetry survives music that actually moves between low, mid and high.
#[derive(Clone, Copy)]
enum Bands {
    Fixed,
    Varying,
}

fn bands_at(t: i64, m: Bands) -> (u8, u8, u8) {
    match m {
        Bands::Fixed => (220, 120, 60),
        Bands::Varying => {
            let f = |amp: f64, mid: f64, per: f64, ph: f64| {
                (mid + amp * ((t as f64 / per + ph) * std::f64::consts::TAU).sin())
                    .clamp(0.0, 255.0) as u8
            };
            (
                f(60.0, 170.0, 11_300.0, 0.0),
                f(70.0, 140.0, 7_100.0, 0.3),
                f(80.0, 130.0, 5_300.0, 0.7),
            )
        }
    }
}

fn beat_block(t: i64, period_ms: i64, energy: u8, m: Bands) -> Vec<u8> {
    let to_next = (period_ms - t.rem_euclid(period_ms)).clamp(0, 65_534) as u16;
    let (low, mid, high) = bands_at(t, m);
    let mut b = vec![0u8; 16];
    b[0] = 1;
    b[1] = 0b0000_0011;
    b[2..4].copy_from_slice(&to_next.to_be_bytes());
    b[4..6].copy_from_slice(&(period_ms as u16).to_be_bytes());
    b[6] = ((t / period_ms) % 16) as u8;
    b[7] = 200;
    b[8] = energy;
    b[9] = low;
    b[10] = mid;
    b[11] = high;
    b[13] = 255;
    b[15] = 128;
    b
}

fn run(period_ms: i64, m: Bands, seed: u32) -> Vec<(i64, bool, bool, bool)> {
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
                    let k = last_k.load(Ordering::SeqCst) + 1;
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
                        base: AUDIO_BASE,
                        channels: beat_block(at, period_ms, 200, m),
                    }))
                }
            }),
            lamp_dwell_ms: Box::new(|| DWELL_MS),
            // The stub RNG is fixed-seed on purpose, so a single run measures one
            // draw. The script is now stochastic enough that one draw says little.
            random_u32: Box::new({
                let state = std::sync::atomic::AtomicU32::new(seed | 1);
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
            if clock.load(Ordering::SeqCst) >= RUN_MS {
                Some(rhai::Dynamic::from("done"))
            } else {
                None
            }
        }
    });
    let _ = engine.run(SCRIPT);
    let out = changes.lock().unwrap().clone();
    out
}

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

/// Fraction of wall time each lamp spends lit.
fn duty(changes: &[(i64, bool, bool, bool)]) -> [f64; 3] {
    let mut on = [0i64; 3];
    let mut prev = (0i64, false, false, false);
    for &c in changes {
        for l in 0..3 {
            if [prev.1, prev.2, prev.3][l] {
                on[l] += c.0 - prev.0;
            }
        }
        prev = c;
    }
    let span = (RUN_MS - 10_000).max(1) as f64;
    [
        on[0] as f64 / span,
        on[1] as f64 / span,
        on[2] as f64 / span,
    ]
}

/// The symbol stream an observer sees: lamp state sampled mid-slot on the
/// quarter-beat grid, paired with which quarter of the beat it is.
fn symbols(changes: &[(i64, bool, bool, bool)], period_ms: i64) -> Vec<(u8, usize)> {
    let q = period_ms / 4;
    let first = 10_000 / q;
    let last = RUN_MS / q;
    (first..last)
        .map(|i| (state_at(changes, i * q + q / 2), (i % 4) as usize))
        .collect()
}

fn entropy(counts: &HashMap<u8, u32>) -> f64 {
    let n: u32 = counts.values().sum();
    if n == 0 {
        return 0.0;
    }
    -counts
        .values()
        .map(|&c| {
            let p = c as f64 / n as f64;
            p * p.log2()
        })
        .sum::<f64>()
}

/// An observer who watches the first half of the run and then predicts the
/// second. Held out on purpose: scoring a high-order context on the same data
/// that built it just measures how many contexts there are.
///
/// Returns (best-guess accuracy, share of test slots whose context was seen
/// during training). An unseen context falls back to the marginal mode.
fn learn_then_predict(seq: &[(u8, usize)], order: usize, use_slot: bool) -> (f64, f64) {
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
    let fallback = marg
        .iter()
        .max_by_key(|(_, &c)| c)
        .map(|(&s, _)| s)
        .unwrap_or(0);
    let mode = |counts: &HashMap<u8, u32>| {
        counts
            .iter()
            .max_by_key(|(_, &c)| c)
            .map(|(&s, _)| s)
            .unwrap_or(fallback)
    };

    let (mut hits, mut seen, mut total) = (0u32, 0u32, 0u32);
    for w in seq[split..].windows(order + 1) {
        let guess = match table.get(&key(w)) {
            Some(counts) => {
                seen += 1;
                mode(counts)
            }
            None => fallback,
        };
        hits += u32::from(guess == w[order].0);
        total += 1;
    }
    (hits as f64 / total as f64, seen as f64 / total as f64)
}

/// Share of gaps between changes in the single commonest 20ms bucket — the same
/// cadence measure `pulse.rs` asserts on, so its bar can be set from a spread
/// rather than from one draw.
fn modal_gap_share(changes: &[(i64, bool, bool, bool)]) -> f64 {
    let mut hist: HashMap<i64, u32> = HashMap::new();
    let mut total = 0;
    for w in changes.windows(2) {
        if w[0].0 < 10_000 {
            continue;
        }
        *hist.entry((w[1].0 - w[0].0) / 20).or_insert(0) += 1;
        total += 1;
    }
    hist.values().copied().max().unwrap_or(0) as f64 / total.max(1) as f64
}

/// Worst single lamp's switching rate, operations per minute.
fn worst_ops_per_min(changes: &[(i64, bool, bool, bool)]) -> f64 {
    let mut n = [0u32; 3];
    let mut prev = [false; 3];
    for &(_, r, y, g) in changes {
        for (i, on) in [r, y, g].into_iter().enumerate() {
            n[i] += u32::from(on != prev[i]);
            prev[i] = on;
        }
    }
    n.iter().copied().max().unwrap_or(0) as f64 / (RUN_MS as f64 / 60_000.0)
}

/// The headline pair: what an observer with `order` slots of memory achieves,
/// and what one with no memory achieves. Their ratio is the quantity of interest
/// — an absolute accuracy moves with the state distribution, the ratio does not.
fn scores(changes: &[(i64, bool, bool, bool)], order: usize) -> (f64, f64, f64) {
    let seq = symbols(changes, BEAT_MS);
    let mut marg: HashMap<u8, u32> = HashMap::new();
    for &(s, _) in &seq {
        *marg.entry(s).or_insert(0) += 1;
    }
    let floor = marg.values().copied().max().unwrap_or(0) as f64 / seq.len() as f64;
    let (acc, _) = learn_then_predict(&seq, order, false);
    (acc, floor, acc / floor)
}

fn spread(v: &mut Vec<f64>) -> (f64, f64, f64) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (v[0], v[v.len() / 2], v[v.len() - 1])
}

fn report(label: &str, m: Bands) {
    const SEEDS: [u32; 9] = [
        0x9E37_79B9,
        0x1234_5678,
        0xDEAD_BEEF,
        0x0BAD_F00D,
        0xA5A5_A5A5,
        0x2718_2818,
        0x3141_5927,
        0x7F4A_7C15,
        0xCAFE_D00D,
    ];

    println!("\n=== {label} ===  {} seeds x 10 min", SEEDS.len());
    let mut acc4 = vec![];
    let mut floors = vec![];
    let mut ratios = vec![];
    let mut duties = vec![];
    let mut hs = vec![];
    let mut darks = vec![];
    let mut cadences = vec![];
    let mut ops = vec![];

    for s in SEEDS {
        let changes = run(BEAT_MS, m, s);
        let (a, f, r) = scores(&changes, 4);
        acc4.push(a * 100.0);
        floors.push(f * 100.0);
        ratios.push(r);
        let d = duty(&changes);
        duties.push((d[0] + d[1] + d[2]) / 3.0);
        let seq = symbols(&changes, BEAT_MS);
        let mut marg: HashMap<u8, u32> = HashMap::new();
        for &(st, _) in &seq {
            *marg.entry(st).or_insert(0) += 1;
        }
        hs.push(entropy(&marg));
        darks.push(marg.get(&0).copied().unwrap_or(0) as f64 / seq.len() as f64);
        cadences.push(modal_gap_share(&changes));
        ops.push(worst_ops_per_min(&changes));
    }

    let row = |name: &str, v: &mut Vec<f64>, unit: &str| {
        let (lo, mid, hi) = spread(v);
        println!("{name:<26} {mid:6.2}{unit}   [{lo:.2} .. {hi:.2}]");
    };
    row("learned order-4", &mut acc4, "%");
    row("memoryless floor", &mut floors, "%");
    row("ratio to floor", &mut ratios, "x");
    row("mean lamp duty", &mut duties, "");
    row("marginal H bits/slot", &mut hs, "");
    row("share of slots fully dark", &mut darks, "");
    row("cadence (modal gap share)", &mut cadences, "");
    row("worst lamp ops/min", &mut ops, "");
}

#[test]
fn probe() {
    report("fixed bands (what pulse.rs sends)", Bands::Fixed);
    report("varying bands", Bands::Varying);
}
