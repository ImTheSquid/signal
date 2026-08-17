//! Turns the master output into a beat grid the traffic light can render.
//!
//! rekordbox's DMX output carries no beat information, so the musical signal is
//! taken from the audio itself: a virtual device cloning the master output on
//! macOS, or an audio interface capturing it on the Steam Deck. This
//! daemon tracks tempo and band energy, and sends the light a prediction —
//! "the next beat is in N ms, the period is P" — rather than beat events.
//! Predictions are what let the light schedule around its 100ms relay dwell
//! instead of chasing a signal it is always a fraction of a beat behind.

mod analysis;
mod bands;
mod capture;
mod downbeat;
mod melbands;
mod meter;
mod metrical;
mod score;
mod tempo;
mod wire;

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use cpal::traits::DeviceTrait;

use crate::analysis::{to_beat, Analyzer, HopResult};
use crate::tempo::HOP;

/// Where the light listens. Matches `dmx-bridge`'s destination, and the two are
/// never run together — the light keeps one sequence number per socket, not per
/// sender, so whichever is numerically ahead starves the other.
const DEFAULT_PORT: u16 = 49500;

/// Same default as `dmx-bridge/cfg.toml`. The light binds `0.0.0.0:49500` and
/// accepts whatever arrives, so nothing needs to know its address — which is
/// the point, since it takes a DHCP lease and has no fixed one.
///
/// Limited broadcast leaves by the default route only. On a machine with both
/// wifi and ethernet up, name the subnet broadcast (`192.168.1.255`) or the
/// light itself to choose the interface.
const DEFAULT_HOST: &str = "255.255.255.255";

/// How long the loop tolerates no callbacks before rebuilding the stream. A
/// device-side sample-rate change is often not reported as an error at all; the
/// callbacks simply stop.
const STREAM_STALL: Duration = Duration::from_millis(500);

/// Settling time after a stream starts, before stalls are believed. CoreAudio
/// delivers a first callback and then pauses while it finishes negotiating the
/// buffer size, which read as a stall and produced one spurious rebuild on
/// every startup.
const STREAM_GRACE: Duration = Duration::from_millis(1500);

/// With no samples arriving, keep the wire alive at the base cadence anyway.
/// Silence on the wire reads as "sender died" to the light, which is a
/// different thing from "the room is quiet".
const IDLE_SEND: Duration = Duration::from_millis(100);

/// Sample rate used for synthesised audio, matching what both capture paths run
/// at — the virtual device on macOS and the interface on the Deck.
const SYNTH_RATE: u32 = 48_000;
const SYNTH_SECONDS: f32 = 600.0;

struct Args {
    host: String,
    port: u16,
    device: Option<String>,
    /// Offline replays to the light only when a host was named explicitly;
    /// analysing a file should not start driving lamps by surprise.
    host_given: bool,
    wav: Option<String>,
    synth_bpm: Option<f32>,
    /// Accented fixture rather than the bare click train, so there is a bar and a
    /// phrase to find.
    synth_pattern: bool,
    offset_ms: f32,
    probe: bool,
    /// A `grid-truth` TSV to score the phrase estimate against.
    score_grid: Option<String>,
}

fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return Ok(());
    }
    if argv.iter().any(|a| a == "--list-devices") {
        return list_devices();
    }

    let args = parse_args(&argv)?;
    match (&args.wav, args.synth_bpm) {
        (Some(path), _) => {
            let (samples, rate) = analysis::read_wav_mono(path)?;
            run_offline(path, samples, rate, &args)
        }
        (None, Some(bpm)) => match args.synth_pattern {
            true => run_offline(
                &format!("{bpm} BPM pattern"),
                analysis::synth_pattern(bpm, SYNTH_SECONDS, SYNTH_RATE),
                SYNTH_RATE,
                &args,
            ),
            false => run_offline(
                &format!("{bpm} BPM metronome"),
                analysis::synth_click_train(bpm, SYNTH_SECONDS, SYNTH_RATE),
                SYNTH_RATE,
                &args,
            ),
        },
        (None, None) => run_live(&args),
    }
}

fn print_usage() {
    eprintln!(
        "audio-bridge — audio beat source for the traffic light\n\
\n\
USAGE:\n  \
  audio-bridge --list-devices\n  \
  audio-bridge [OPTIONS]                      capture live and drive the light\n  \
  audio-bridge --wav FILE [OPTIONS]           analyse a file instead of a device\n  \
  audio-bridge --synth BPM [OPTIONS]          drive from a perfect metronome\n\
\n\
OPTIONS:\n  \
  --host HOST        default {DEFAULT_HOST}; the light binds 0.0.0.0 and takes\n                     \
                     whatever arrives, so its own address is never needed.\n                     \
                     Name a subnet broadcast or the light to pick an interface.\n                     \
                     With --wav or --synth, giving this also replays to it.\n  \
  --port N           default {DEFAULT_PORT}\n  \
  --device NAME      exact input name; defaults to the first starting\n                     \
                     \"{}\"\n  \
  --offset MS        shift the reported beat, negative fires early (default 0)\n  \
  --probe            per-hop TSV on stdout, for plotting and tuning\n  \
  --pattern          with --synth, an accented fixture (kick every beat, clap\n                     \
                     on 2 and 4, crash every 16) instead of a bare click train\n  \
  --score-grid FILE  score the phrase estimate against a grid-truth TSV;\n                     \
                     see crates/grid-truth\n\
\n\
--synth is the calibration signal: the grid is known-perfect, so any offset\n\
seen on camera belongs to the rig rather than the tracker. Sweep --offset\n\
against it to find the lamp lead. It is deliberately unaccented, so add\n\
--pattern when the thing being tested is the phrase rather than the tempo.\n",
        capture::DEFAULT_DEVICE
    );
}

fn parse_args(argv: &[String]) -> Result<Args> {
    let mut args = Args {
        host: DEFAULT_HOST.to_string(),
        host_given: false,
        port: DEFAULT_PORT,
        device: None,
        wav: None,
        synth_bpm: None,
        synth_pattern: false,
        offset_ms: 0.0,
        probe: false,
        score_grid: None,
    };
    let mut i = 0;
    while i < argv.len() {
        let take = |i: &mut usize| -> Result<String> {
            *i += 1;
            argv.get(*i)
                .cloned()
                .with_context(|| format!("{} needs a value", argv[*i - 1]))
        };
        match argv[i].as_str() {
            "--host" => {
                args.host = take(&mut i)?;
                args.host_given = true;
            }
            "--port" => args.port = take(&mut i)?.parse().context("--port must be a number")?,
            "--device" => args.device = Some(take(&mut i)?),
            "--wav" => args.wav = Some(take(&mut i)?),
            "--synth" => {
                args.synth_bpm = Some(take(&mut i)?.parse().context("--synth must be a BPM")?)
            }
            "--offset" => {
                args.offset_ms = take(&mut i)?.parse().context("--offset must be a number")?
            }
            "--probe" => args.probe = true,
            "--pattern" => args.synth_pattern = true,
            "--score-grid" => args.score_grid = Some(take(&mut i)?),
            other => bail!("unknown argument: {other}"),
        }
        i += 1;
    }
    Ok(args)
}

fn list_devices() -> Result<()> {
    let devices = capture::list_inputs()?;
    if devices.is_empty() {
        println!("no input devices");
        return Ok(());
    }
    println!("{:<32} {:>4}  {:>8}  {}", "INPUT", "CH", "DEFAULT", "OFFERS");
    for d in &devices {
        let offers = if d.supported_rates.is_empty() {
            "-".to_string()
        } else {
            d.supported_rates
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",")
        };
        println!(
            "{:<32} {:>4}  {:>8}  {}",
            d.name, d.channels, d.default_rate, offers
        );
    }
    Ok(())
}

fn probe_header() {
    // Raw levels alongside the normalised ones: the normalised values are
    // relative by construction and so can never show a bass kill, which is
    // exactly what you go looking for when tuning.
    println!(
        "t\tlow\tmid\thigh\tenergy\traw_low\traw_mid\traw_high\tflux\tbuild\tbpm\tconf\tnext_ms\tbeat\tflags\tphase\tsigma"
    );
}

fn probe_row(t: f64, r: &HopResult, flags: u8) {
    let bpm = r.grid.period_ms.map(|p| 60_000.0 / p).unwrap_or(0.0);
    println!(
        "{t:.3}\t{:.3}\t{:.3}\t{:.3}\t{:.3}\t{:.5}\t{:.5}\t{:.5}\t{:.3}\t{:+.3}\t{bpm:.2}\t{:.3}\t{}\t{}\t{flags:#06b}\t{}\t{:.2}",
        r.levels.low,
        r.levels.mid,
        r.levels.high,
        r.levels.energy,
        r.levels.raw_low,
        r.levels.raw_mid,
        r.levels.raw_high,
        r.levels.flux,
        r.levels.build,
        r.grid.confidence,
        r.grid
            .ms_to_next_beat
            .map(|v| format!("{v:.1}"))
            .unwrap_or_else(|| "-".into()),
        u8::from(r.beat),
        r.phrase.phase_of(r.grid.beat_index),
        r.phrase.excess_sigma,
    );
}

/// Analyse a file. Sends too, if a host is given, paced to real time so the
/// light sees the same cadence it would at a gig.
fn run_offline(label: &str, samples: Vec<f32>, sample_rate: u32, args: &Args) -> Result<()> {
    let mut analyzer = Analyzer::new(sample_rate)?;
    let mut sender = match args.host_given {
        true => {
            let s = wire::Sender::new(&args.host, args.port)?;
            eprintln!("replaying {label} to {} in real time", s.dest());
            Some(s)
        }
        false => None,
    };

    if args.probe {
        probe_header();
    }

    let mut score = match &args.score_grid {
        Some(path) => Some(score::Score::new(score::Truth::read(path)?)),
        None => None,
    };

    // Only when paced to real time. Running a file at full speed would redraw
    // hundreds of times a second and show nothing legible.
    let mut meter = sender.is_some().then(meter::Meter::new);

    let started = Instant::now();
    let mut confidence_sum = 0.0f64;
    let mut tracking_hops = 0u64;
    let mut hops = 0u64;

    for hop in samples.chunks_exact(HOP) {
        let result = analyzer.push_hop(hop)?;
        let audio_time = analyzer.elapsed_s();

        if let Some(sender) = &mut sender {
            // Pace to the audio clock, so the light is driven at the rate the
            // music actually plays rather than as fast as the file reads.
            let target = Duration::from_secs_f64(audio_time);
            if let Some(wait) = target.checked_sub(started.elapsed()) {
                std::thread::sleep(wait);
            }
            let beat = to_beat(&result, hop, args.offset_ms);
            sender.maybe_send(&beat, result.beat, Instant::now())?;
            if let Some(meter) = &mut meter {
                meter.update(&result.levels, &result.grid, result.beat, beat.audio_present);
            }
        }

        if args.probe {
            let beat = to_beat(&result, hop, args.offset_ms);
            probe_row(audio_time, &result, wire::encode_block(&beat)[1]);
        }

        if let Some(score) = &mut score {
            score.observe(audio_time, &result);
        }

        confidence_sum += result.grid.confidence as f64;
        tracking_hops += u64::from(result.grid.tracking);
        hops += 1;
    }

    if let Some(meter) = &mut meter {
        meter.finish();
    }
    if hops == 0 {
        bail!("{label} is shorter than one {HOP}-sample hop");
    }
    let grid = analyzer.push_hop(&vec![0.0; HOP])?.grid;
    eprintln!(
        "{label}: {:.1}s at {sample_rate}Hz\n\
         final tempo   {}\n\
         mean conf     {:.3}\n\
         tracking      {:.1}% of hops",
        analyzer.elapsed_s(),
        grid.period_ms
            .map(|p| format!("{:.2} BPM ({p:.1}ms)", 60_000.0 / p))
            .unwrap_or_else(|| "never established".into()),
        confidence_sum / hops as f64,
        100.0 * tracking_hops as f64 / hops as f64,
    );
    if let Some(score) = &mut score {
        score.report();
    }
    Ok(())
}

fn run_live(args: &Args) -> Result<()> {
    let mut meter = meter::Meter::new();
    let outcome = capture_loop(args, &mut meter);
    // The loop only ends by failing, which is precisely when a half-drawn
    // status line would swallow the reason.
    meter.finish();
    outcome
}

fn capture_loop(args: &Args, meter: &mut meter::Meter) -> Result<()> {
    let mut sender = wire::Sender::new(&args.host, args.port)?;
    eprintln!("sending to {}", sender.dest());

    let device = capture::pick_input(args.device.as_deref())?;
    let name = device.name().unwrap_or_else(|_| "<unnamed>".into());
    let mut config = capture::choose_config(&device)?;
    let mut cap = capture::start(&device, &config)?;
    let mut stream_started = Instant::now();
    eprintln!(
        "capturing {name} at {}Hz, {} channels",
        cap.sample_rate, config.channels
    );

    let mut analyzer = Analyzer::new(cap.sample_rate)?;
    if args.probe {
        probe_header();
    }

    let mut hop = vec![0.0f32; HOP];
    let mut last_result: Option<HopResult> = None;
    let mut last_activity = Instant::now();

    loop {
        // Drain whatever has arrived, in whole hops.
        while cap.samples.slots() >= HOP {
            for slot in hop.iter_mut() {
                *slot = cap.samples.pop().unwrap_or(0.0);
            }
            let result = analyzer.push_hop(&hop)?;
            let beat = to_beat(&result, &hop, args.offset_ms);
            sender.maybe_send(&beat, result.beat, Instant::now())?;
            if args.probe {
                probe_row(analyzer.elapsed_s(), &result, wire::encode_block(&beat)[1]);
            }
            meter.update(&result.levels, &result.grid, result.beat, beat.audio_present);
            last_result = Some(result);
            last_activity = Instant::now();
        }

        // Keep the wire alive even with nothing arriving. The light reads
        // silence as a dead sender, so a stalled device must still be
        // reported rather than merely producing nothing.
        if last_activity.elapsed() >= IDLE_SEND {
            if let Some(result) = &last_result {
                let mut beat = to_beat(result, &[], args.offset_ms);
                beat.audio_present = false;
                sender.maybe_send(&beat, false, Instant::now())?;
            }
        }

        if cap.health.split_legs() {
            meter.note("clone legs are out of polarity; using the left channel only");
        }

        let stalled = stream_started.elapsed() > STREAM_GRACE
            && cap
                .health
                .since_last_callback()
                .is_some_and(|d| d > STREAM_STALL);
        if stalled || cap.health.errored() {
            meter.note("audio stream stopped; rebuilding");
            drop(cap);
            std::thread::sleep(Duration::from_millis(250));
            config = capture::choose_config(&device)?;
            cap = capture::start(&device, &config)?;
            stream_started = Instant::now();
            // A rate change invalidates every filter state and the tracker's
            // whole time base, so start clean rather than reinterpreting old
            // history at a new rate.
            if cap.sample_rate != analyzer.sample_rate() {
                meter.note(&format!(
                    "sample rate changed {} -> {}; resetting the tracker",
                    analyzer.sample_rate(),
                    cap.sample_rate
                ));
                analyzer = Analyzer::new(cap.sample_rate)?;
            }
            last_activity = Instant::now();
        }

        std::thread::sleep(Duration::from_millis(2));
    }
}
