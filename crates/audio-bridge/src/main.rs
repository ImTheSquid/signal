//! Turns the deck's audio into a beat grid the traffic light can render.
//!
//! rekordbox's DMX output carries no beat information, so the musical signal
//! comes from a virtual audio device cloning the master output instead. This
//! daemon tracks tempo and band energy, and sends the light a prediction —
//! "the next beat is in N ms, the period is P" — rather than beat events.
//! Predictions are what let the light schedule around its 100ms relay dwell
//! instead of chasing a signal it is always a fraction of a beat behind.

mod bands;
mod capture;
mod tempo;

use anyhow::Result;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--list-devices") => list_devices(),
        Some("--help" | "-h") | None => {
            print_usage();
            Ok(())
        }
        Some(other) => {
            eprintln!("unknown argument: {other}\n");
            print_usage();
            std::process::exit(2);
        }
    }
}

fn print_usage() {
    eprintln!(
        "audio-bridge — audio beat source for the traffic light\n\n\
         USAGE:\n  \
           audio-bridge --list-devices    show every input device CoreAudio offers\n"
    );
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
