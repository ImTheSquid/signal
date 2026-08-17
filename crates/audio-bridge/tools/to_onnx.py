"""Convert TempoCNN's bundled Keras model to ONNX, and check it still agrees.

Converting is only half the job: a conversion that silently changes the numbers is
worse than none, so this runs the same input through both and compares.
"""

import sys
import numpy as np
import tensorflow as tf
import tf2onnx
import onnxruntime as ort

SRC = sys.argv[1]
DST = sys.argv[2]

model = tf.keras.models.load_model(SRC, compile=False)
print("loaded:", SRC)
print("  input :", model.input_shape)
print("  output:", model.output_shape)
print("  params:", model.count_params())

# TempoCNN takes [batch, 40, 256, 1] internally (mel bands x frames x channel).
shape = [1 if d is None else d for d in model.input_shape]
spec = (tf.TensorSpec(shape, tf.float32, name="input"),)

proto, _ = tf2onnx.convert.from_keras(model, input_signature=spec, opset=17, output_path=DST)
print("wrote:", DST)

ops = sorted({n.op_type for n in proto.graph.node})
print("ops used:", ", ".join(ops))

rng = np.random.default_rng(0)
x = rng.random(shape, dtype=np.float32)
keras_out = model.predict(x, verbose=0)
sess = ort.InferenceSession(DST, providers=["CPUExecutionProvider"])
onnx_out = sess.run(None, {sess.get_inputs()[0].name: x})[0]

delta = float(np.abs(keras_out - onnx_out).max())
print(f"max abs difference keras vs onnx: {delta:.3e}")
print("AGREES" if delta < 1e-4 else "DIVERGES")
