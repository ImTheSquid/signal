"""Dump TempoCNN's mel filterbank so Rust does not have to recompute it.

librosa's default mel scale is Slaney with area normalisation (`htk=False,
norm='slaney'`), not the HTK formula most tutorials show. Re-deriving it in Rust
would shift every band by a little and cost accuracy in a way nothing would report,
so the matrix is dumped once here and read as data.

    python3 tools/mel_filters.py data/mel_40x513_f32.bin

Emits row-major f32 little-endian, shape (40, 513) = (n_mels, 1 + n_fft/2).
"""

import sys
import librosa
import numpy as np

# Must match tempocnn/feature.py exactly.
SR = 11025
N_FFT = 1024
N_MELS = 40
FMIN = 20
FMAX = 5000

out = sys.argv[1]
fb = librosa.filters.mel(sr=SR, n_fft=N_FFT, n_mels=N_MELS, fmin=FMIN, fmax=FMAX)
assert fb.shape == (N_MELS, 1 + N_FFT // 2), fb.shape
assert fb.dtype == np.float32, fb.dtype

with open(out, "wb") as f:
    f.write(fb.tobytes(order="C"))

print(f"wrote {out}: {fb.shape} f32, {fb.nbytes} bytes")
print(f"row sums (first 5): {[round(float(v), 6) for v in fb.sum(axis=1)[:5]]}")
print(f"nonzero: {int((fb > 0).sum())} of {fb.size}")
