"""The reference numbers the Rust side has to match.

Same deterministic ramp, through ONNX Runtime.
"""

import sys
import numpy as np
import onnxruntime as ort

sess = ort.InferenceSession(sys.argv[1], providers=["CPUExecutionProvider"])
name = sess.get_inputs()[0].name

data = np.array([(i % 97) / 97.0 for i in range(40 * 256)], dtype=np.float32)
x = data.reshape(1, 40, 256, 1)
out = sess.run(None, {name: x})[0][0]

idx = int(np.argmax(out))
print(f"classes: {out.shape[0]}")
print(f"argmax index {idx} = {idx + 30} BPM, p={out[idx]:.6f}")
print(f"softmax sums to {out.sum():.6f}")
print("first 5:", [round(float(v), 8) for v in out[:5]])
