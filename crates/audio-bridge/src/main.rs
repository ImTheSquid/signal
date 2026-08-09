use bliss_audio_aubio_rs::Tempo;

fn main() {
    let t = Tempo::new(bliss_audio_aubio_rs::OnsetMode::SpecFlux, 1024, 256, 48_000);
    println!("tempo constructed: {}", t.is_ok());
}
