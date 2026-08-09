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
/// Live allocation count, not bytes. The device's allocator charges a header per
/// allocation, so this is what decides whether a cheaper allocator is worth
/// having — see `allocation_overhead_is_worth_measuring`.
static COUNT: AtomicIsize = AtomicIsize::new(0);
/// Live allocations by size class, `SIZE_CLASSES[i] ..= SIZE_CLASSES[i+1]`.
/// Small classes are what a per-allocation header punishes.
static BUCKETS: [AtomicIsize; 6] = [
    AtomicIsize::new(0),
    AtomicIsize::new(0),
    AtomicIsize::new(0),
    AtomicIsize::new(0),
    AtomicIsize::new(0),
    AtomicIsize::new(0),
];
const SIZE_CLASSES: [usize; 6] = [8, 16, 32, 64, 256, usize::MAX];

fn bucket_of(size: usize) -> usize {
    SIZE_CLASSES.iter().position(|&c| size <= c).unwrap_or(5)
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        LIVE.fetch_add(l.size() as isize, Ordering::Relaxed);
        COUNT.fetch_add(1, Ordering::Relaxed);
        BUCKETS[bucket_of(l.size())].fetch_add(1, Ordering::Relaxed);
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size() as isize, Ordering::Relaxed);
        COUNT.fetch_sub(1, Ordering::Relaxed);
        BUCKETS[bucket_of(l.size())].fetch_sub(1, Ordering::Relaxed);
        System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        LIVE.fetch_add(new as isize - l.size() as isize, Ordering::Relaxed);
        BUCKETS[bucket_of(l.size())].fetch_sub(1, Ordering::Relaxed);
        BUCKETS[bucket_of(new)].fetch_add(1, Ordering::Relaxed);
        System.realloc(p, l, new)
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn live() -> isize {
    LIVE.load(Ordering::Relaxed)
}

fn count() -> isize {
    COUNT.load(Ordering::Relaxed)
}

/// Allocation held by `f`'s return value, which is dropped before returning.
fn cost<T>(f: impl FnOnce() -> T) -> isize {
    let base = live();
    let held = f();
    let bytes = live() - base;
    drop(held);
    bytes
}

/// Bytes *and* allocations held by `f`'s return value.
fn cost_counted<T>(f: impl FnOnce() -> T) -> (isize, isize) {
    let (base_bytes, base_count) = (live(), count());
    let held = f();
    let out = (live() - base_bytes, count() - base_count);
    drop(held);
    out
}

const SCRIPT: &str = include_str!("../../../scripts/follow.rhai");

/// Free heap the device reports while idle on the built-in cycle, and the
/// script thread's stack — both from the running light, not from theory.
const DEVICE_FREE_HEAP: isize = 140_796;
const SCRIPT_STACK: isize = 32 * 1024;
/// Engine registries are pointer-dense, so they do roughly halve on a 32-bit
/// target. This does **not** apply to the AST — see below.
const HOST_TO_DEVICE: isize = 2;
/// Heap per byte of minified source, from the device, not from halving the host
/// figure. Halving gave 7.7 and the board then exhausted 154900 free bytes on a
/// 5054-byte script, so the host number is optimistic by more than 2x: the AST's
/// i64 and f32 fields do not shrink on a 32-bit target, and rhai's parser peaks
/// well above the tree it finally keeps. Must match the firmware's
/// AST_BYTES_PER_SOURCE_BYTE.
const DEVICE_AST_BYTES_PER_SOURCE_BYTE: f64 = 24.0;
/// The full standard library, measured on the light by logging free heap either
/// side of building it. The host reports 135320 for the same thing, so scaling
/// the host number is what produced a guard that let a doomed script through.
const DEVICE_FULL_ENGINE: isize = 95_872;

/// ESP-IDF's TLSF charges ~8 bytes of header per allocation. `talc`, the best
/// no_std alternative surveyed, charges one `usize` — 4 bytes on this 32-bit
/// target. So routing the interpreter through an arena is worth
/// `allocations * 4` bytes and nothing more, which is a much smaller number
/// than "the allocator overhead" sounds like.
///
/// This exists so that trade is decided on a measurement. It asserts nothing
/// about the result: it prints what an arena could recover, against a ceiling
/// of about 3,500 source bytes.
fn allocation_overhead_is_worth_measuring() {
    const IDF_HEADER: isize = 8;
    const TALC_HEADER: isize = 4;

    let engine = script_env::new_engine(script_env::Components::all());
    let (engine_bytes, engine_allocs) =
        cost_counted(|| script_env::new_engine(script_env::Components::all()));

    let opts = rhaiper::Options {
        rename: false,
        ..Default::default()
    };
    let minified = rhaiper::minify_with_engine(&engine, SCRIPT, &opts)
        .expect("follow.rhai must minify")
        .text;

    let before = [
        BUCKETS[0].load(Ordering::Relaxed),
        BUCKETS[1].load(Ordering::Relaxed),
        BUCKETS[2].load(Ordering::Relaxed),
        BUCKETS[3].load(Ordering::Relaxed),
        BUCKETS[4].load(Ordering::Relaxed),
        BUCKETS[5].load(Ordering::Relaxed),
    ];
    let (ast_bytes, ast_allocs) =
        cost_counted(|| engine.compile(&minified).expect("minified must compile"));
    let after = [
        BUCKETS[0].load(Ordering::Relaxed),
        BUCKETS[1].load(Ordering::Relaxed),
        BUCKETS[2].load(Ordering::Relaxed),
        BUCKETS[3].load(Ordering::Relaxed),
        BUCKETS[4].load(Ordering::Relaxed),
        BUCKETS[5].load(Ordering::Relaxed),
    ];
    drop(engine);

    let total_allocs = engine_allocs + ast_allocs;
    println!(
        "\n  allocations held, for {} minified source bytes\
         \n    engine (full library)  {engine_allocs:>7} allocs   {engine_bytes:>8} bytes\
         \n    follow.rhai AST        {ast_allocs:>7} allocs   {ast_bytes:>8} bytes\
         \n    together               {total_allocs:>7} allocs\
         \n\
         \n  AST allocations by size (what a per-allocation header punishes)",
        minified.len()
    );
    let mut lo = 0usize;
    for (i, &hi) in SIZE_CLASSES.iter().enumerate() {
        let n = after[i] - before[i];
        let label = if hi == usize::MAX {
            format!("{lo}+")
        } else {
            format!("{lo}-{hi}")
        };
        println!("    {label:>10} bytes  {n:>7}");
        lo = hi.saturating_add(1);
    }

    // The same text compiled on the two engines this crate can produce. These
    // must not diverge: the firmware runs `new_engine`, so if it built a fatter
    // AST than the stock `Engine::new` the device would be paying for the
    // convenience of a shared constructor.
    let stock = script_env::rhai::Engine::new();
    let (stock_ast, stock_allocs) =
        cost_counted(|| stock.compile(&minified).expect("stock compile"));
    drop(stock);
    let mut limited = script_env::rhai::Engine::new();
    script_env::apply_limits(&mut limited);
    let (limited_ast, limited_allocs) =
        cost_counted(|| limited.compile(&minified).expect("limited compile"));
    drop(limited);

    let mut packaged = script_env::rhai::Engine::new_raw();
    {
        use script_env::rhai::packages::Package;
        script_env::rhai::packages::StandardPackage::new().register_into_engine(&mut packaged);
    }
    let (packaged_ast, packaged_allocs) =
        cost_counted(|| packaged.compile(&minified).expect("packaged compile"));
    drop(packaged);

    let ours = script_env::new_engine(script_env::Components::all());
    let (ours_ast, ours_allocs) =
        cost_counted(|| ours.compile(&minified).expect("our compile"));
    drop(ours);
    println!(
        "  same text, isolating what makes the AST bigger\
         \n    Engine::new()                 {stock_ast:>8} bytes  {stock_allocs:>7} allocs\
         \n    Engine::new() + apply_limits  {limited_ast:>8} bytes  {limited_allocs:>7} allocs\
         \n    new_raw + StandardPackage     {packaged_ast:>8} bytes  {packaged_allocs:>7} allocs\
         \n    new_engine(all())             {ours_ast:>8} bytes  {ours_allocs:>7} allocs\n"
    );

    // `new_engine` builds up from `new_raw`, which does not turn on the string
    // interner that `Engine::new` does. Forgetting it cost 18600 bytes and 573
    // allocations on this script and shipped to the device before it was caught,
    // so parity with the stock engine is pinned rather than trusted.
    assert!(
        ours_ast <= stock_ast && ours_allocs <= stock_allocs,
        "new_engine builds a fatter AST than Engine::new ({ours_ast} bytes / \
         {ours_allocs} allocs vs {stock_ast} / {stock_allocs}); the string \
         interner is the usual cause"
    );

    let idf = total_allocs * IDF_HEADER;
    let talc = total_allocs * TALC_HEADER;
    println!(
        "\n  header cost on device, engine + AST\
         \n    ESP-IDF TLSF at {IDF_HEADER}/alloc   {idf:>8} bytes\
         \n    talc at {TALC_HEADER}/alloc          {talc:>8} bytes\
         \n    an arena could recover  {:>8} bytes = about {} more source bytes\n",
        idf - talc,
        ((idf - talc) as f64 / DEVICE_AST_BYTES_PER_SOURCE_BYTE) as isize
    );
}

/// What the device would hold if it loaded a compiled artifact instead of
/// parsing a tree.
///
/// The AST is the reason the light can only run ~3.5KB of script: 1879
/// allocations for follow.rhai, each charged an 8-byte header by ESP-IDF, on a
/// heap where wifi and TLS have already taken 168KB of 323KB. An artifact is a
/// flat buffer, so the question is what it costs and whether anything in
/// follow.rhai fails to lower — a residual stays an AST fragment and keeps its
/// AST cost.
fn artifact_is_cheaper_than_the_tree() {
    let engine = script_env::new_engine(script_env::Components::all());
    let opts = rhaiper::Options {
        rename: false,
        ..Default::default()
    };
    let minified = rhaiper::minify_with_engine(&engine, SCRIPT, &opts)
        .expect("follow.rhai must minify")
        .text;
    let ast = engine.compile(&minified).expect("minified must compile");

    let (tree_bytes, tree_allocs) = cost_counted(|| engine.compile(&minified).unwrap());

    let program = rhai::grain::Compiler::new().compile(&ast);
    let residual = program.residual_count();
    let residual_nodes = program.residual_nodes();
    let unsupported = program.first_unsupported();

    let wire = program.write().expect("follow.rhai must serialise");
    // Borrowed against the wire buffer, which is what the device should do: it
    // has to keep the bytes anyway, and `into_owned` copies every pool out of
    // them. Measured both ways because the difference decides which one the
    // firmware holds.
    let (loaded_bytes, loaded_allocs) =
        cost_counted(|| rhai::grain::Program::read(&wire).expect("what we just wrote must load"));
    let (owned_bytes, owned_allocs) = cost_counted(|| {
        rhai::grain::Program::read(&wire)
            .expect("what we just wrote must load")
            .into_owned()
    });

    println!(
        "\n  follow.rhai, {} minified bytes\
         \n    as a tree          {tree_bytes:>8} bytes  {tree_allocs:>6} allocs\
         \n    artifact, borrowed {loaded_bytes:>8} bytes  {loaded_allocs:>6} allocs\
         \n    artifact, owned    {owned_bytes:>8} bytes  {owned_allocs:>6} allocs\
         \n    artifact on the wire {:>6} bytes\
         \n    residual           {residual} ({residual_nodes} nodes){}\n",
        minified.len(),
        wire.len(),
        match unsupported {
            Some((what, pos)) => format!("  <- first unsupported: {what} at {pos:?}"),
            None => String::new(),
        }
    );

    // A residual is an AST fragment handed back to the walker, which keeps the
    // per-node allocation cost this whole change exists to remove. Nothing in
    // follow.rhai should need one.
    assert_eq!(
        residual, 0,
        "follow.rhai does not lower whole; {residual_nodes} nodes stay a tree"
    );
    assert!(
        loaded_allocs < tree_allocs / 10,
        "artifact holds {loaded_allocs} allocations against the tree's {tree_allocs}"
    );
}

#[test]
fn interpreter_footprint() {
    // Called, not a #[test] of its own: the allocation counters are global, so a
    // second test running in parallel measures this one's allocations too.
    allocation_overhead_is_worth_measuring();
    artifact_is_cheaper_than_the_tree();

    // The standard-library modules are built once and shared by pointer, so the
    // first engine pays for them and every later one pays a refcount. The device
    // builds an engine per run, so it is the second number that it lives with.
    let first_engine = cost(|| script_env::new_engine(script_env::Components::all()));
    let later_engine = cost(|| script_env::new_engine(script_env::Components::all()));

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

    // Renaming as well. rhai stores variable names as SmartString<LazyCompact>,
    // which inlines up to `sizeof(String) - 1` — 23 bytes here but only **11 on
    // the 32-bit device** — and `Identifier` is a plain SmartString, not a
    // refcounted one, so a name past that threshold allocates at every use site.
    // Short names therefore cost the device nothing at all, which is a saving
    // this 64-bit host mostly cannot see.
    let renamed = rhaiper::minify_with_engine(
        &engine,
        SCRIPT,
        &rhaiper::Options {
            rename: true,
            ..Default::default()
        },
    )
    .expect("follow.rhai must minify with renaming")
    .text;
    let ast_renamed = cost(|| engine.compile(&renamed).expect("renamed must compile"));
    drop(engine);

    // How much of follow.rhai is names the device has to allocate for.
    let mut long: Vec<&str> = Vec::new();
    for tok in SCRIPT.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if tok.len() > 11 && !tok.chars().next().is_some_and(|c| c.is_numeric()) {
            long.push(tok);
        }
    }
    let distinct: std::collections::BTreeSet<&&str> = long.iter().collect();
    println!(
        "\n  renaming\
         \n    minified             {:>8} bytes\
         \n    minified + renamed   {:>8} bytes\
         \n    AST minified         {ast_min:>8}\
         \n    AST renamed          {ast_renamed:>8}\
         \n    identifiers over 11 chars: {} uses of {} distinct names\
         \n      (each such use allocates on the device; on this host they inline)\n",
        minified.len(),
        renamed.len(),
        long.len(),
        distinct.len(),
    );

    let per_byte = ast_min as f64 / minified.len() as f64;
    // The device figures, not the host ones scaled: the engine measured 95872
    // on the light against 135320 here, and the AST does not shrink at all.
    let engine_dev = DEVICE_FULL_ENGINE;
    let headroom = DEVICE_FREE_HEAP - engine_dev - SCRIPT_STACK;
    let serviceable = (headroom as f64 / DEVICE_AST_BYTES_PER_SOURCE_BYTE) as isize;

    println!(
        "\n  host bytes\
         \n    Engine::new()        {full:>8}\
         \n    Engine::new_raw()    {raw:>8}   (floor only — see the module docs)\
         \n    validation_engine()  {validation:>8}\
         \n    follow.rhai AST      {ast:>8}   from {} source bytes\
         \n    minified AST         {ast_min:>8}   from {} minified bytes\
         \n    AST per source byte  {per_byte:>8.1}\
         \n\
         \n    new_engine, first   {first_engine:>8}   (builds the shared modules)\
         \n    new_engine, later   {later_engine:>8}   (what each run costs)\
         \n\
         \n  device budget, measured on the light, against {DEVICE_FREE_HEAP} free\
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

    // The device loads artifacts, so the limit that has to be honourable is the
    // artifact one. Against the narrowest engine, which is the best case; a
    // script declaring nothing gets less, and the firmware's `heap_check`
    // refuses it against the heap it actually has rather than a constant.
    let floor_engine = cost(|| {
        let mut e = script_env::rhai::Engine::new_raw();
        script_env::rhai::packages::ArithmeticPackage::new().register_into_engine(&mut e);
        script_env::apply_limits(&mut e);
        script_env::register_api(&mut e, script_env::Handlers::stubs());
        e
    }) / HOST_TO_DEVICE;
    // Matches ARTIFACT_BYTES_PER_WIRE_BYTE in firmware/src/script.rs.
    const ARTIFACT_HEAP_PER_WIRE_BYTE: isize = 5;
    let room = DEVICE_FREE_HEAP - floor_engine - SCRIPT_STACK;
    let serviceable_artifact = room / ARTIFACT_HEAP_PER_WIRE_BYTE;
    println!(
        "  artifact ceiling: {room} bytes of heap / {ARTIFACT_HEAP_PER_WIRE_BYTE} \
         = {serviceable_artifact} wire bytes, against a {} limit\n",
        script_env::MAX_ARTIFACT_BYTES
    );
    assert!(
        serviceable_artifact >= script_env::MAX_ARTIFACT_BYTES as isize,
        "MAX_ARTIFACT_BYTES is {} but the narrowest engine only leaves room for \
         about {serviceable_artifact} bytes of artifact",
        script_env::MAX_ARTIFACT_BYTES
    );
}
