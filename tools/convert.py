#!/usr/bin/env python3
"""Convert the ONNX selfie-segmentation export into an OpenVINO IR pair.

The NPU plugin compiles only static shapes, so the dynamic batch dimension the
ONNX export carries is pinned to 1 here rather than at load time -- a model that
reshapes on the fly would recompile, and an NPU compile costs seconds.
"""
import sys
from pathlib import Path

import openvino as ov


def main() -> None:
    src = Path(sys.argv[1] if len(sys.argv) > 1 else "models/selfie_segmentation.onnx")
    dst = Path(sys.argv[2] if len(sys.argv) > 2 else "models/selfie_segmentation.xml")

    core = ov.Core()
    model = core.read_model(src)
    model.reshape({model.input(0): ov.PartialShape([1, 3, 256, 256])})
    ov.save_model(model, dst, compress_to_fp16=True)

    print(f"{src.name} -> {dst.name}")
    for port in (*model.inputs, *model.outputs):
        print(f"  {port.any_name:14} {port.element_type} {port.partial_shape}")


if __name__ == "__main__":
    main()
