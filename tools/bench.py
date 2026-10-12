#!/usr/bin/env python3
"""Time one segmentation inference on every device OpenVINO can reach.

Compile time is reported separately because it is the NPU's one sharp edge: the
first compile of a model costs seconds, so the daemon must cache blobs rather
than pay it on every start.
"""
import sys
import time
from pathlib import Path

import numpy as np
import openvino as ov

MODEL = Path(sys.argv[1] if len(sys.argv) > 1 else "models/segmentation.xml")
CACHE = Path("/tmp/ov-cache")
RUNS = 200


def main() -> None:
    core = ov.Core()
    core.set_property({"CACHE_DIR": str(CACHE)})
    model = core.read_model(MODEL)
    frame = np.random.rand(1, 3, 256, 256).astype(np.float32)

    print(f"model {MODEL}   devices {core.available_devices}\n")
    print(f"{'device':8} {'compile':>10} {'mean':>9} {'p50':>9} {'p99':>9} {'fps':>8}")

    for device in core.available_devices:
        try:
            t0 = time.perf_counter()
            compiled = core.compile_model(model, device)
            compile_ms = (time.perf_counter() - t0) * 1e3

            request = compiled.create_infer_request()
            for _ in range(20):  # warm up: first calls carry allocation cost
                request.infer({0: frame})

            samples = []
            for _ in range(RUNS):
                t0 = time.perf_counter()
                request.infer({0: frame})
                samples.append((time.perf_counter() - t0) * 1e3)

            s = np.array(samples)
            print(f"{device:8} {compile_ms:9.0f}ms {s.mean():8.2f}ms "
                  f"{np.percentile(s, 50):8.2f}ms {np.percentile(s, 99):8.2f}ms "
                  f"{1e3 / s.mean():8.1f}")
        except Exception as exc:
            print(f"{device:8} FAILED  {str(exc).splitlines()[-1][:60]}")


if __name__ == "__main__":
    main()
