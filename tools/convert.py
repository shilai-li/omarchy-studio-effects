#!/usr/bin/env python3
"""Convert a segmentation ONNX into an OpenVINO IR the NPU will accept.

Handles both models this project can use:

  selfie_segmentation.onnx  one image in, one mask out
  rvm_mobilenetv3.onnx      RobustVideoMatting: an image plus four recurrent
                            state tensors in, an alpha matte and four new
                            states out

The NPU plugin compiles only static shapes, so the dynamic batch the ONNX
exports carry is pinned here rather than at load time -- a model that reshapes
on the fly would recompile, and an NPU compile costs seconds.

RVM needs two extra things for the same reason. Its `downsample_ratio` is a
runtime input, so the sizes inside its encoder depend on a value the compiler
cannot see; left alone the NPU refuses the model outright, complaining about a
dimension of -9223372036854775808, which is OpenVINO's marker for "dynamic".
Freezing it to a constant resolves every shape. And the recurrent states have
no declared size at all, so they are discovered by running the model once on
the CPU and then pinned too.
"""
import sys
from pathlib import Path

import numpy as np
import openvino as ov
from openvino import opset13 as ops
from openvino.utils import replace_node

# How much RVM downsamples before its recurrent encoder. Half costs 4.90 ms on
# this NPU against 8.88 at full, for a matte that is still 256x256.
RVM_RATIO = 0.5
SIDE = 256


def is_recurrent(model) -> bool:
    return any(p.get_friendly_name() == "downsample_ratio" for p in model.get_parameters())


def state_shapes(core, src: Path) -> dict:
    """Run once on the CPU to find out how big the recurrent states are."""
    request = core.compile_model(core.read_model(src), "CPU").create_infer_request()
    feed = {
        "src": np.zeros((1, 3, SIDE, SIDE), dtype=np.float32),
        "downsample_ratio": np.array([RVM_RATIO], dtype=np.float32),
    }
    for i in range(1, 5):
        feed[f"r{i}i"] = np.zeros((1, 1, 1, 1), dtype=np.float32)
    request.infer(feed)
    return {f"r{i}i": list(request.get_tensor(f"r{i}o").shape) for i in range(1, 5)}


def main() -> None:
    src = Path(sys.argv[1] if len(sys.argv) > 1 else "models/selfie_segmentation.onnx")
    dst = Path(sys.argv[2] if len(sys.argv) > 2 else "models/selfie_segmentation.xml")

    core = ov.Core()
    model = core.read_model(src)

    if is_recurrent(model):
        shapes = state_shapes(core, src)
        for parameter in model.get_parameters():
            if parameter.get_friendly_name() == "downsample_ratio":
                replace_node(parameter, ops.constant(np.array([RVM_RATIO], dtype=np.float32)))
                model.remove_parameter(parameter)
                break
        shapes["src"] = [1, 3, SIDE, SIDE]
        model.reshape({k: ov.PartialShape(v) for k, v in shapes.items()})
    else:
        model.reshape({model.input(0): ov.PartialShape([1, 3, SIDE, SIDE])})

    ov.save_model(model, dst, compress_to_fp16=True)

    print(f"{src.name} -> {dst.name}")
    for port in (*model.inputs, *model.outputs):
        print(f"  {port.any_name:18} {port.element_type} {port.partial_shape}")


if __name__ == "__main__":
    main()
