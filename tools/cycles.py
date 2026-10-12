#!/usr/bin/env python3
"""Count the daemon's CPU cycles and instructions per frame, every thread included.

    taskset -c 4-7 /usr/bin/python3 tools/cycles.py 20 label -- \\
        daemon/target/release/studio-effects-daemon --input /dev/video0 ...

CPU time is the wrong ruler for comparing two builds here. The camera often
delivers 10-15 fps, the cores idle between frames, and their clock follows the
load: when one build does less work, the clock drops and the same remaining
work reads as more CPU time. Making the frame loop half as expensive made the
capture thread's unchanged decode read 60% dearer. Cycles and instructions do
not move with the clock.

Opens inherited counters on itself and then starts the command, so the counts
cover it and every thread it creates; an inherited count reaches the parent's
counter when the child exits, which is why this stops the daemon itself. It
counts on the E-core PMU only (cpu_atom), so pin the command to E-cores, and
user space only, which is what an unprivileged process may count at the
default perf_event_paranoid of 2. Startup is included, the same for both builds.
"""
import ctypes
import os
import signal
import struct
import subprocess
import sys
import time

SYS_PERF_EVENT_OPEN = 298
HW_CPU_CYCLES, HW_INSTRUCTIONS = 0, 1
INHERIT, EXCLUDE_KERNEL, EXCLUDE_HV = 1 << 1, 1 << 5, 1 << 6

libc = ctypes.CDLL(None, use_errno=True)


def counter(config: int) -> int:
    pmu = int(open("/sys/bus/event_source/devices/cpu_atom/type").read())
    # A hybrid CPU's hardware events name their PMU in the top half of config.
    attr = struct.pack("IIQQQQQIIQ", 0, 64, (pmu << 32) | config, 0, 0, 0,
                       INHERIT | EXCLUDE_KERNEL | EXCLUDE_HV, 0, 0, 0)
    fd = libc.syscall(SYS_PERF_EVENT_OPEN, ctypes.c_char_p(attr), 0, -1, -1, 0)
    if fd < 0:
        sys.exit(f"perf_event_open: {os.strerror(ctypes.get_errno())}")
    return fd


def read(fd: int) -> int:
    return struct.unpack("Q", os.read(fd, 8))[0]


def main() -> None:
    seconds, label = float(sys.argv[1]), sys.argv[2]
    command = sys.argv[sys.argv.index("--") + 1:]
    cycles, instructions = counter(HW_CPU_CYCLES), counter(HW_INSTRUCTIONS)
    own = read(cycles), read(instructions)

    # One timing line per ten frames, so frames are counted rather than assumed.
    log = f"/tmp/cycles-{label}.log"
    daemon = subprocess.Popen(command + ["--stats-every", "10"],
                              stdout=open(log, "w"), stderr=subprocess.STDOUT)
    time.sleep(seconds)
    daemon.send_signal(signal.SIGINT)
    daemon.wait(timeout=10)

    frames = 10 * sum(1 for line in open(log) if "ms/frame" in line)
    if frames == 0:
        sys.exit(f"no frames counted; see {log}")
    c, i = read(cycles) - own[0], read(instructions) - own[1]
    print(f"{label:12} {frames} frames: {c / frames / 1e6:6.1f} M cycles/frame, "
          f"{i / frames / 1e6:6.1f} M instructions/frame")


if __name__ == "__main__":
    main()
