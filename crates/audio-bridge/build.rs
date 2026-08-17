//! Get TempoCNN's ONNX, prove it is the right file, and generate Rust from it.
//!
//! The weights are 11.7MB, which is six times the largest blob this repo commits
//! and there is no git-lfs, so the file is fetched rather than tracked. That trades
//! a fat history for a build that can fail on a network, so:
//!
//! - a local copy is always preferred, and `TEMPOCNN_ONNX` names one outright, so
//!   the build can be made fully offline by seeding `models/`;
//! - the cache lives in `models/`, not `target/`, so `cargo clean` does not throw
//!   it away and the Deck can take it over rsync;
//! - the checksum is verified on *every* build, cached or fetched. A truncated
//!   download or a substituted file has to fail here. A model quietly computing
//!   the wrong thing is the failure mode this whole exercise keeps running into.
//!
//! `tools/to_onnx.py` is the recipe that produced the file, and checks the
//! conversion against Keras. The checksum pins this exact artifact; the recipe
//! reproduces an equivalent one, not necessarily a byte-identical one, since
//! tf2onnx does not promise stable node naming.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

const ONNX_SHA256: &str = "8a7e6cccda32f539df7fb6ad67e7b4b49f0987e1e9f4323ce790722c3e0871ff";
const ONNX_BYTES: u64 = 11_704_751;

/// Release asset on this repo. Nothing upstream publishes TempoCNN as ONNX —
/// Essentia has frozen `.pb` and TF.js only — so this is our own artifact.
const ONNX_URL: &str =
    "https://github.com/ImTheSquid/signal/releases/download/tempocnn-v1/tempocnn.onnx";

fn main() {
    println!("cargo:rerun-if-env-changed=TEMPOCNN_ONNX");

    let onnx = resolve();
    verify(&onnx);
    println!("cargo:rerun-if-changed={}", onnx.display());

    burn_import::onnx::ModelGen::new()
        .input(onnx.to_str().expect("model path must be utf-8"))
        .out_dir("model/")
        .run_from_script();
}

fn resolve() -> PathBuf {
    if let Ok(p) = std::env::var("TEMPOCNN_ONNX") {
        let p = PathBuf::from(p);
        assert!(p.is_file(), "TEMPOCNN_ONNX={} is not a file", p.display());
        return p;
    }

    let cached = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("models")
        .join("tempocnn.onnx");
    if cached.is_file() {
        return cached;
    }

    fetch(&cached);
    cached
}

fn fetch(dest: &Path) {
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).expect("could not create models/");
    }
    eprintln!("audio-bridge: fetching {ONNX_URL}");

    let mut body = Vec::with_capacity(ONNX_BYTES as usize);
    let mut reader = ureq::get(ONNX_URL)
        .call()
        .unwrap_or_else(|e| {
            panic!(
                "could not fetch the TempoCNN model: {e}\n\
                 Seed it instead: put tempocnn.onnx in {}, or set TEMPOCNN_ONNX to \
                 a local copy. tools/to_onnx.py regenerates it from PyPI.",
                dest.display()
            )
        })
        .into_body()
        .into_reader();
    std::io::Read::read_to_end(&mut reader, &mut body).expect("could not read the model body");

    // Write to a sibling first: a build interrupted mid-write must not leave a
    // truncated file that later builds treat as a cache hit.
    let tmp = dest.with_extension("onnx.part");
    std::fs::write(&tmp, &body).expect("could not write the model");
    std::fs::rename(&tmp, dest).expect("could not move the model into place");
}

fn verify(path: &Path) {
    let bytes = std::fs::read(path).expect("could not read the model");
    let got = hex(&Sha256::digest(&bytes));
    assert_eq!(
        got,
        ONNX_SHA256,
        "\nTempoCNN model checksum mismatch at {}\n  expected {ONNX_SHA256}\n  got      {got}\n\
         {} bytes, expected {ONNX_BYTES}. Delete it and let the build re-fetch.",
        path.display(),
        bytes.len(),
    );
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
