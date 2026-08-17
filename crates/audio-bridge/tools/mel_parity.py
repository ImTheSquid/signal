"""Reference mel values for the Rust spectrogram to match.

The numbers this prints are frozen into `melbands.rs`'s test, so the check keeps
working without Python. Re-run it if the feature contract ever changes.

    python3 tools/mel_parity.py
"""

import librosa
import numpy as np

SR = 11025
N_FFT = 1024
HOP = 512
N_MELS = 40
FMIN = 20
FMAX = 5000

# Must match `melbands::probe_signal` exactly.
n = N_FFT + HOP * 4
t = np.arange(n, dtype=np.float32) / SR
y = (0.5 * np.sin(2 * np.pi * 440.0 * t) + 0.2 * np.sin(2 * np.pi * 1000.0 * t)).astype(
    np.float32
)

mel = librosa.feature.melspectrogram(
    y=y, sr=SR, n_fft=N_FFT, hop_length=HOP, power=1, n_mels=N_MELS, fmin=FMIN, fmax=FMAX
)

# librosa pads by default (`center=True`), so its frame 0 is centred on sample 0
# with half a window of zeros in front. The Rust side starts at sample 0 with no
# padding, so its frame 0 is librosa's frame at offset n_fft/2 — which is frame 1.
print("librosa shape:", mel.shape, "(centered)")
for f in (0, 1, 2):
    row = mel[:8, f]
    print(f"  frame {f}: [{', '.join(f'{v:.8f}' for v in row)}]")

nc = librosa.feature.melspectrogram(
    y=y,
    sr=SR,
    n_fft=N_FFT,
    hop_length=HOP,
    power=1,
    n_mels=N_MELS,
    fmin=FMIN,
    fmax=FMAX,
    center=False,
)
print("\ncenter=False shape:", nc.shape, "<- this is what Rust computes")
for f in (0, 1):
    row = nc[:8, f]
    print(f"  frame {f}: [{', '.join(f'{v:.8f}' for v in row)}]")
print("\n  loudest band, frame 0:", int(np.argmax(nc[:, 0])))
