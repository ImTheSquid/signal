//! What the interpreter costs, and how big a script the device can actually run.
//!
//! The light has ~140KB free at idle and OOM-aborts partway into
//! `scripts/follow.rhai`, so the question is how the budget divides between the
//! engine (paid once per run, whatever the script) and the AST (paid per source
//! byte).
//!
//! **The engine's cost is only discretionary if the submitter says so.** Every
//! key holder submits arbitrary Rhai and the admin idle script runs on the same
//! engine, and the documented surface promises the standard library — so a
//! script gets all of it unless it declares otherwise (see `Components` and the
//! README). Rhai resolves calls at run time, so nothing can infer the set: an
//! under-declared script fails at the first call it cannot resolve. `new_raw`
//! is the floor, not a default.
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

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        LIVE.fetch_add(l.size() as isize, Ordering::Relaxed);
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size() as isize, Ordering::Relaxed);
        System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        LIVE.fetch_add(new as isize - l.size() as isize, Ordering::Relaxed);
        System.realloc(p, l, new)
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn live() -> isize {
    LIVE.load(Ordering::Relaxed)
}

/// Allocation held by `f`'s return value, which is dropped before returning.
fn cost<T>(f: impl FnOnce() -> T) -> isize {
    let base = live();
    let held = f();
    let bytes = live() - base;
    drop(held);
    bytes
}

const SCRIPT: &str = include_str!("../../../scripts/follow.rhai");

/// Free heap the device reports while idle on the built-in cycle, and the
/// script thread's stack — both from the running light, not from theory.
const DEVICE_FREE_HEAP: isize = 140_796;
const SCRIPT_STACK: isize = 32 * 1024;
/// Host allocations are pointer-dense and this target is 64-bit.
const HOST_TO_DEVICE: isize = 2;

#[test]
fn interpreter_footprint() {
    let full = cost(script_env::rhai::Engine::new);
    let raw = cost(script_env::rhai::Engine::new_raw);
    let validation = cost(script_env::validation_engine);

    let engine = script_env::rhai::Engine::new();
    let ast = cost(|| engine.compile(SCRIPT).expect("follow.rhai must compile"));

    // The device is sent the minified form, so that is what the per-byte cost
    // has to be measured against.
    // Same options `/v1/script` minifies with, so the byte count is the one the
    // limit is actually enforced against.
    let opts = rhaiper::Options {
        rename: false,
        ..Default::default()
    };
    let minified = rhaiper::minify_with_engine(&engine, SCRIPT, &opts)
        .expect("follow.rhai must minify")
        .text;
    let ast_min = cost(|| engine.compile(&minified).expect("minified must compile"));
    drop(engine);

    let per_byte = ast_min as f64 / minified.len() as f64;
    let engine_dev = validation / HOST_TO_DEVICE;
    let headroom = DEVICE_FREE_HEAP - engine_dev - SCRIPT_STACK;
    let serviceable = (headroom as f64 / (per_byte / HOST_TO_DEVICE as f64)) as isize;

    println!(
        "\n  host bytes\
         \n    Engine::new()        {full:>8}\
         \n    Engine::new_raw()    {raw:>8}   (floor only — see the module docs)\
         \n    validation_engine()  {validation:>8}\
         \n    follow.rhai AST      {ast:>8}   from {} source bytes\
         \n    minified AST         {ast_min:>8}   from {} minified bytes\
         \n    AST per source byte  {per_byte:>8.1}\
         \n\
         \n  device budget (host/2), against {DEVICE_FREE_HEAP} free\
         \n    engine               {engine_dev:>8}\
         \n    script stack         {SCRIPT_STACK:>8}\
         \n    leaves for the AST   {headroom:>8}\
         \n    = about {serviceable} source bytes; follow.rhai is {}; the API \
             advertises {}\n",
        SCRIPT.len(),
        minified.len(),
        minified.len(),
        script_env::MAX_SCRIPT_BYTES,
    );

    // What the script actually needs, rather than the whole library. The engine
    // cannot be mutated during evaluation (`run` takes `&self`), so the set has
    // to be chosen before the run — but choosing it wrong is only an
    // `ErrorFunctionNotFound`, which is a history row, not a reboot.
    use script_env::rhai::packages::Package;
    let sets: [(&str, fn(&mut script_env::rhai::Engine)); 4] = [
        ("arithmetic", |e| {
            script_env::rhai::packages::ArithmeticPackage::new().register_into_engine(e);
        }),
        ("+ logic", |e| {
            script_env::rhai::packages::ArithmeticPackage::new().register_into_engine(e);
            script_env::rhai::packages::LogicPackage::new().register_into_engine(e);
        }),
        ("array + math (follow)", |e| {
            script_env::rhai::packages::ArithmeticPackage::new().register_into_engine(e);
            script_env::rhai::packages::LogicPackage::new().register_into_engine(e);
            script_env::rhai::packages::BasicArrayPackage::new().register_into_engine(e);
            // follow.rhai calls to_float, which lives in math.
            script_env::rhai::packages::BasicMathPackage::new().register_into_engine(e);
        }),
        ("everything (today)", |e| {
            script_env::rhai::packages::StandardPackage::new().register_into_engine(e);
        }),
    ];

    println!("  selective registration — script bytes the device could then run\n");
    println!("  package set              engine(dev)   for AST   source bytes");
    for (name, build) in sets {
        let bytes = cost(|| {
            let mut e = script_env::rhai::Engine::new_raw();
            build(&mut e);
            script_env::apply_limits(&mut e);
            script_env::register_api(&mut e, script_env::Handlers::stubs());
            e
        });
        let dev = bytes / HOST_TO_DEVICE;
        let room = DEVICE_FREE_HEAP - dev - SCRIPT_STACK;
        let bytes_per = per_byte / HOST_TO_DEVICE as f64;
        println!(
            "  {name:<22} {dev:>9}  {room:>8}   {:>8}",
            (room as f64 / bytes_per) as isize
        );
    }
    println!("\n  follow.rhai needs {} bytes\n", minified.len());

    // The advertised limit has to be one the device can actually honour, or the
    // server accepts scripts that reboot the light. This is the regression that
    // matters: it is what shipped.
    assert!(
        serviceable >= script_env::MAX_SCRIPT_BYTES as isize,
        "MAX_SCRIPT_BYTES is {} but the device can only run about {serviceable} \
         bytes of script (engine {engine_dev} + stack {SCRIPT_STACK} of \
         {DEVICE_FREE_HEAP} free, then {per_byte:.1} bytes of AST per source byte)",
        script_env::MAX_SCRIPT_BYTES
    );
}
