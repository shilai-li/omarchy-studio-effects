#!/usr/bin/env python3
"""Measure what 30 fps segmentation costs the CPU on each device.

bench.py answers "how fast can this go", which is the wrong question here --
everything is far quicker than one frame. This answers the right one: while
holding a real 30 fps camera cadence, how much CPU does each device burn? That
is what decides whether effects cost the user a video call's worth of battery.
"""
import os
import sys
import time
from pathlib import Path

import numpy as np
import openvino as ov

MODEL = Path(sys.argv[1] if len(sys.argv) > 1 else "models/segmentation.xml")
FPS = 30
SECONDS = 8
NPU_BUSY = Path("/sys/class/accel/accel0/device/npu_busy_time_us")
CLK = os.sysconf("SC_CLK_TCK")


def cpu_seconds() -> float:
    """utime + stime for this process, including every worker thread."""
    fields = Path("/proc/self/stat").read_text().rsplit(")", 1)[1].split()
    return (int(fields[11]) + int(fields[12])) / CLK


def npu_busy_us() -> int:
    try:
        return int(NPU_BUSY.read_text())
    except OSError:
        return 0


def main() -> None:
    core = ov.Core()
    core.set_property({"CACHE_DIR": "/tmp/ov-cache"})
    model = core.read_model(MODEL)
    frame = np.random.rand(1, 3, 256, 256).astype(np.float32)
    period = 1.0 / FPS

    print(f"holding {FPS} fps for {SECONDS}s per device\n")
    print(f"{'device':8} {'frames':>7} {'cpu':>9} {'cpu/frame':>11} "
          f"{'1 core':>8} {'npu busy':>10}")

    for device in core.available_devices:
        config = {"INFERENCE_NUM_THREADS": 1} if device == "CPU" else {}
        request = core.compile_model(model, device, config).create_infer_request()
        for _ in range(30):
            request.infer({0: frame})

        cpu0, npu0, t0 = cpu_seconds(), npu_busy_us(), time.perf_counter()
        frames, deadline = 0, t0
        while time.perf_counter() - t0 < SECONDS:
            request.infer({0: frame})
            frames += 1
            deadline += period
            slack = deadline - time.perf_counter()
            if slack > 0:
                time.sleep(slack)
        wall = time.perf_counter() - t0
        cpu = cpu_seconds() - cpu0
        npu = (npu_busy_us() - npu0) / 1e6

        print(f"{device:8} {frames:7} {cpu:8.3f}s {1e3 * cpu / frames:10.2f}ms "
              f"{100 * cpu / wall:7.1f}% {npu:9.3f}s")


if __name__ == "__main__":
    main()
