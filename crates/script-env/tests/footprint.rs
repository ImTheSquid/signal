//! What the interpreter costs, and how big a script the device can actually run.
//!
//! The light has ~140KB free at idle and a script has to fit in what the engine
//! leaves. The standard library is lazy — functions stay in static manifests and
//! are looked up on first call — so the engine is a small constant and the
//! budget is the script's own artifact.
//!
//! Host figures are an upper bound on the device's — this is a 64-bit target and
//! rhai's registry is pointer-dense, so absolute bytes run roughly 2x the
//! ESP32's. The ratios are what transfer.
//!
//! One test, not several: the allocation counter is global, so parallel tests
//! would measure each other.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

static LIVE: AtomicIsize = AtomicIsize::new(0);
/// Live allocation count, not bytes. The device's allocator charges a header per
/// allocation, so this is what decides how much a flat artifact saves.
static COUNT: AtomicIsize = AtomicIsize::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        LIVE.fetch_add(l.size() as isize, Ordering::Relaxed);
        COUNT.fetch_add(1, Ordering::Relaxed);
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size() as isize, Ordering::Relaxed);
        COUNT.fetch_sub(1, Ordering::Relaxed);
        System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        LIVE.fetch_add(new as isize - l.size() as isize, Ordering::Relaxed);
        System.realloc(p, l, new)
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Bytes and allocations held by `f`'s return value, which is dropped before
/// returning.
fn cost_counted<T>(f: impl FnOnce() -> T) -> (isize, isize) {
    let (base_bytes, base_count) = (LIVE.load(Ordering::Relaxed), COUNT.load(Ordering::Relaxed));
    let held = f();
    let out = (
        LIVE.load(Ordering::Relaxed) - base_bytes,
        COUNT.load(Ordering::Relaxed) - base_count,
    );
    drop(held);
    out
}

const SCRIPT: &str = include_str!("../../../scripts/pulse.rhai");

/// Free heap the device reports while idle on the built-in cycle, and the
/// script thread's stack — both from the running light, not from theory.
const DEVICE_FREE_HEAP: isize = 140_796;
const SCRIPT_STACK: isize = 32 * 1024;
/// Engine registries are pointer-dense, so they do roughly halve on a 32-bit
/// target. This does **not** apply to the AST: its i64 and f32 fields do not
/// shrink.
const HOST_TO_DEVICE: isize = 2;
/// Matches ARTIFACT_BYTES_PER_WIRE_BYTE in firmware/src/script.rs.
const ARTIFACT_HEAP_PER_WIRE_BYTE: isize = 5;

#[test]
fn interpreter_footprint() {
    // Eager `StandardPackage`, which the lazy one stands in for.
    let eager = cost_counted(|| {
        use script_env::rhai::packages::Package;
        let mut e = script_env::rhai::Engine::new_raw();
        script_env::rhai::packages::StandardPackage::new().register_into_engine(&mut e);
        e
    });

    // The lazy module is built once and shared by pointer, so the first engine
    // pays for it and every later one pays a refcount. The device builds an
    // engine per run, so it is the second number it lives with.
    let first = cost_counted(script_env::new_engine);
    let later = cost_counted(script_env::new_engine);

    println!(
        "\n  host engine cost\
         \n    eager StandardPackage {:>8} bytes  {:>6} allocs\
         \n    new_engine, first     {:>8} bytes  {:>6} allocs\
         \n    new_engine, later     {:>8} bytes  {:>6} allocs\n",
        eager.0, eager.1, first.0, first.1, later.0, later.1
    );
    assert!(
        later.0 * 4 < eager.0,
        "the lazy engine ({} bytes) is not much smaller than the eager one ({} bytes)",
        later.0,
        eager.0
    );

    // The same text compiled on the stock engine and ours must not diverge: the
    // firmware runs `new_engine`, so a fatter AST than `Engine::new` would be
    // the device paying for the convenience of a shared constructor. Building up
    // from `new_raw` drops the string interner `Engine::new` turns on, which once
    // cost 18600 bytes and 573 allocations on this script.
    let stock = script_env::rhai::Engine::new();
    let ours = script_env::new_engine();
    let (stock_ast, stock_allocs) = cost_counted(|| stock.compile(SCRIPT).expect("stock compile"));
    let (ours_ast, ours_allocs) = cost_counted(|| ours.compile(SCRIPT).expect("our compile"));
    assert!(
        ours_ast <= stock_ast && ours_allocs <= stock_allocs,
        "new_engine builds a fatter AST than Engine::new ({ours_ast} bytes / \
         {ours_allocs} allocs vs {stock_ast} / {stock_allocs}); the string \
         interner is the usual cause"
    );

    // What the device holds if it loads a compiled artifact instead of parsing a
    // tree. A residual stays an AST fragment and keeps its AST cost.
    let ast = ours.compile(SCRIPT).expect("pulse.rhai must compile");
    let artifact = script_env::lower(&ast).expect("pulse.rhai must lower");
    let (loaded_bytes, loaded_allocs) = cost_counted(|| {
        rhai::grain::Program::read(&artifact.program).expect("what we just wrote must load")
    });

    println!(
        "  pulse.rhai, {} source bytes\
         \n    as a tree          {ours_ast:>8} bytes  {ours_allocs:>6} allocs\
         \n    artifact, borrowed {loaded_bytes:>8} bytes  {loaded_allocs:>6} allocs\
         \n    artifact on wire   {:>8} bytes\
         \n    residual           {}\n",
        SCRIPT.len(),
        artifact.program.len(),
        artifact.residual
    );
    assert_eq!(artifact.residual, 0, "pulse.rhai does not lower whole");
    assert!(
        loaded_allocs < ours_allocs / 10,
        "artifact holds {loaded_allocs} allocations against the tree's {ours_allocs}"
    );

    // The device loads artifacts, so the limit that has to be honourable is the
    // artifact one, against the lazy engine.
    let engine_dev = later.0 / HOST_TO_DEVICE;
    let room = DEVICE_FREE_HEAP - engine_dev - SCRIPT_STACK;
    let serviceable = room / ARTIFACT_HEAP_PER_WIRE_BYTE;
    println!(
        "  artifact ceiling: {room} bytes of heap / {ARTIFACT_HEAP_PER_WIRE_BYTE} \
         = {serviceable} wire bytes, against a {} limit\n",
        script_env::MAX_ARTIFACT_BYTES
    );
    assert!(
        serviceable >= script_env::MAX_ARTIFACT_BYTES as isize,
        "MAX_ARTIFACT_BYTES is {} but the lazy engine only leaves room for about \
         {serviceable} bytes of artifact",
        script_env::MAX_ARTIFACT_BYTES
    );
}
