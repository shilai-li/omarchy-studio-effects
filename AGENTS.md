# AGENTS.md — omarchy-studio-effects

Maintenance notes for anyone (agent or human) editing this repo. User-facing
docs live in `README.md`; don't duplicate them here. This file is for what the
code cannot tell you: the measurements the design rests on, and why the obvious
approach is the wrong one.

## What it is

**Omarchy Studio Effects** — camera background blur and replacement, segmented
on the Intel NPU. Two halves in one repo, because the halves ship by different
routes:

| Half | What | Ships as |
|---|---|---|
| `studio-effects-daemon` | Rust. Reads a camera, segments, composites, writes a v4l2loopback device | PKGBUILD → package |
| `shilai_li.studio-effects` | QML bar widget. Toggles effects, picks one | `omarchy plugin add` |

They are one repo so there is one version and one issue tracker, but they are
**not** one artifact and the plugin must never assume the daemon is installed.

## Why the plugin cannot be the whole thing

From the plugin contract (`/usr/share/omarchy/shell/README.md`):

> The installer never runs plugin code, install hooks, or sudo — it only clones
> files, validates the manifest, and toggles enabled state over shell IPC.

`omarchy plugin add` clones QML and nothing else: no build, no binary, no
systemd unit, no udev rule. On top of that, a plugin runs *inside* the
`omarchy-shell` Quickshell process, which is the last place a 30 fps video
pipeline belongs. So the widget is a control surface over IPC and nothing more.

When the daemon is absent the widget says so, the way Recent Paths reports a
missing zoxide — an explicit "not installed" state, never an empty panel that
looks like a working one with no effects.

## Layout

```
manifest.json            plugin manifest; the bar widget is not written yet
models/*.onnx            model source. The IR beside it is built, not committed
tools/convert.py         ONNX -> static FP16 IR
tools/bench.py           per-device inference latency
tools/load.py            per-device CPU cost at a real 30 fps cadence
daemon/src/main.rs       CLI, GStreamer wiring, per-stage timing
daemon/src/nv12.rs       the frame maths, and the tests that pin it
daemon/src/mask.rs       conditioning the model's output into an alpha
daemon/src/background.rs decoding a replacement background once
daemon/src/segmenter.rs  device choice, model cache, one inference per frame
daemon/src/device.rs     resolving a v4l2 device by card label
packaging/               systemd units, PKGBUILD, example config
```

## Ground truth — read it, don't guess

| What | Path |
|---|---|
| Plugin contract, manifest schema, `kinds` table | `/usr/share/omarchy/shell/README.md` |
| Shell source / UI kit / theme singletons | `/usr/share/omarchy/shell/{,Ui/,Commons/}` |
| Manifest validator | `/usr/share/omarchy/bin/omarchy-plugin-validate` |
| Closest structural reference (bar widget + panel) | `~/.config/omarchy/plugins/shilai_li.recent-paths/` |
| The camera's existing relay | `/etc/v4l2-relayd.d/ipu7.conf`, `v4l2-relayd@ipu7.service` |
| NPU device permissions | `/usr/lib/udev/rules.d/10-intel-npu.rules` |

## The camera path already exists

Omarchy already relays this machine's MIPI camera through GStreamer into a
loopback, and that is the thing to chain onto, not replace:

```
ov08x40 → IPU7 hardware ISP → icamerasrc → [v4l2-relayd@ipu7] → /dev/video50
                                                                     │
                                                        studio-effects-daemon
                                                                     ↓
                                                              /dev/video51
```

`/dev/video50` is NV12 1920x1080@30, labelled "Hardware ISP Camera". The daemon
reads it and writes a second loopback that apps select by name.

**Do not put the effects inside `v4l2-relayd@ipu7.service` instead.** That unit
runs `DevicePolicy=closed` with no `DeviceAllow` for `char-accel`, so the NPU is
unreachable from inside it, plus `LimitNPROC=1` and `InaccessibleDirectories=/home`.
Making effects the *default* camera is a later feature, and it is a drop-in on
that unit plus a device rule — not a reason to start there.

## Measured, not assumed

Every number below is from this machine (Core Ultra X7 358H, Arc B390), 1080p30,
`tools/bench.py` and `gst-launch` wall time. Re-measure before trusting them on
other hardware.

Segmentation, 256x256 FP16 IR, one frame, `tools/bench.py`:

| device | compile (cold) | compile (cached) | mean | p99 |
|---|---|---|---|---|
| CPU | 90 ms | 47 ms | 0.79 ms | 1.07 ms |
| GPU | 1040 ms | 211 ms | 0.39 ms | 0.70 ms |
| NPU | 331 ms | **13 ms** | 0.65 ms | 0.83 ms |

Frame handling, per 1080p frame, CPU, `gst-launch` wall time:

| stage | cost |
|---|---|
| NV12 passthrough (floor) | 0.96 ms |
| NV12 → RGBA → NV12 | 5.84 ms |
| downscale to 256x256 RGB | 4.22 ms |
| blur via 1/8 down+upscale | 6.72 ms |

**Inference is not the bottleneck and never was.** It is under 1 ms against a
33 ms budget, and colour conversion costs six times the model. So optimise
conversions, not the network: staying in NV12 and compositing on the Y/UV planes
beats anything faster. A GPU composite path wins because sampling is free there,
not because the GPU infers quicker.

### What the NPU is actually for

Since every device clears 30 fps with room to spare, speed cannot be the reason
to pick one. The reason is what holding that cadence *costs*, `tools/load.py`:

| device | CPU per frame | as % of one core | NPU busy |
|---|---|---|---|
| CPU (1 thread) | 4.12 ms | 12.4% | — |
| GPU | 1.67 ms | 5.0% | — |
| NPU | 0.54 ms | **1.6%** | 2.0% |

The NPU costs about a seventh of the CPU it takes to do the same work on the
CPU, and a third of the GPU path — while leaving the GPU itself untouched for
the video call's encode and render. That is the whole pitch. Effects should be
something a laptop can leave on for an hour, not a reason to hear the fan.

Two honesty notes on that table, because both are easy to get wrong:

- **Watts were never measured.** RAPL's `energy_uj` is root-only on this kernel
  and the machine was on AC, so there is no battery-drain figure. CPU occupancy
  is a proxy — a good one, but if you ever get a real power number, put it here
  and delete this paragraph.
- **The CPU row is single-threaded on purpose.** At its defaults the CPU plugin
  spreads one 256x256 inference across all 16 threads and spin-waits between
  frames, charging **18.67 ms/frame — 56% of a core** — to buy latency that was
  already far inside budget. Quoting that against the NPU would inflate the
  NPU's advantage from 7x to 35x. Never benchmark the CPU plugin at defaults for
  a fixed-cadence workload; pass `INFERENCE_NUM_THREADS: 1` as `load.py` does.

### What the daemon actually costs

`studio-effects-daemon`, per frame, USB camera, NPU, blur radius 12:

| resolution | prep | infer | blur | blend | total | of 33 ms |
|---|---|---|---|---|---|---|
| 1280x720 | 0.24 | 0.72 | 2.38 | 1.94 | **5.28 ms** | 16% |
| 1920x1080 | 0.20 | 0.85 | 4.07 | 1.94 | **7.06 ms** | 21% |

1080p30 fits with room to spare. It did not at first -- the first working
version landed on 33.28 ms, exactly the budget -- and the 4.7x that closed the
gap came entirely from how the pixels are walked, not from the model or the
device:

| | before | after | what changed |
|---|---|---|---|
| blend @1080p | 17.11 ms | 1.94 ms | separable integer mask upscale |
| blur @1080p | 15.15 ms | 4.07 ms | reciprocal multiply, row-major vertical pass |

**Blend.** The mask arrives at 256x256 and the frame is 2M pixels, so the naive
version sampled it bilinearly per pixel: four float loads and half a dozen float
ops, two million times. Separating the axes moves that work off the per-pixel
path -- one horizontal pass over 256 rows (491k operations), then two byte loads
and an integer lerp per pixel.

**Blur.** Two ordinary-looking lines were most of the cost. The window average
was an integer divide, four million times, by a divisor that never changes; it
is a reciprocal multiply now. And the vertical pass walked a column at a time,
which is how a separable blur reads naturally and touches one byte from each of
`h` cache lines. Carrying a running sum per column turns it into sequential row
scans.

None of this touched the model, and neither should the next round: at 1080p the
model is 0.85 ms of 7.06.

### Both hot loops have reference tests

`cargo test` checks the fast paths against slow obvious ones -- a float bilinear
sampler for the upscaler, a naive nested-loop box blur for the blur. They exist
because these two functions were rewritten for speed after being verified only
by looking at a webcam, and a webcam cannot tell you about a bias of one level.

It caught one immediately. `(1 << 24) / 11` truncates, so a flat plane of 200
blurred to 199: every still background darkened by a level the moment effects
came on, uniformly enough that no one would see it and call it a bug. Both
roundings in `box_blur` are load-bearing for that reason.

Note that a snapshot showing everything blurred is usually not a fault. The
model wants a person filling a reasonable part of the frame; with the subject
small or far the mask is legitimately near-empty and the whole frame blurs. Reach
for the tests before the pipeline.

There is also real quality headroomThere is also real quality headroom: 256x256 MediaPipe is the cheap end of the
model range, and the budget would carry something much better. Do not spend it
on a bigger model until the composite is off the CPU.

## Invariants

**Static shapes, cached blobs.** The NPU plugin compiles only static shapes, and
a first compile costs seconds (a second on the GPU already). `tools/convert.py`
pins the batch at conversion time rather than reshaping at load, and the daemon
must set `CACHE_DIR` so a restart is not a stall.

**Device is a fallback chain, never an assertion.** NPU → GPU → CPU, decided at
startup from what `Core.available_devices` actually reports. A missing NPU is a
quieter machine, not a broken one — and it is missing more often than you would
think, because the driver's udev rule puts `/dev/accel/*` in group `render` and
a user is not in `render` by default. The failure is silent: OpenVINO reports
`available_devices` without `NPU` rather than raising a permission error, so the
daemon must say "no NPU, using GPU" out loud instead of quietly degrading.

**NPU access is a permissions question, not a group question.** An earlier note
here said the shipped unit should set `SupplementaryGroups=render`. That was
wrong twice over, and the correction is worth keeping because the symptom --
"the daemon says it fell back to the GPU" -- looks exactly like a bug in device
selection, which is the one place it is not.

`SupplementaryGroups=` does not work in a **user** unit at all: changing
credentials needs privilege the per-user systemd manager does not have. And the
daemon is a user service by design, because it reads the user's camera and
follows their session.

What actually governs access is the mode on `/dev/accel/accel0`, and two udev
rules disagree about it. Intel's `10-intel-npu.rules` asks for `GROUP="render",
MODE="0660"`; systemd's `50-udev-default.rules` asks for `MODE="0666"`, sorts
later, and wins. So on a current Arch/Omarchy system the NPU is world-accessible
and no group is involved -- verified by running the daemon under `systemd --user`
with no special credentials, where it reports `segmenting on NPU`.

Do not lean on that either. If a system does end up at 0660, the fix is to join
`render` **and reboot**, and the reboot is the part everyone skips. Supplementary
groups are fixed when a process is created; every terminal descends from the
long-lived `systemd --user` manager; logging out does not restart it (`Linger=no`
only stops it once *every* session closes) and `daemon-reexec` does not refresh
it. A user can join `render`, log out, log back in, and still have no NPU
anywhere. Measured both ways: with access `npu_busy_time_us` climbed 54463 us
over nine seconds; without it, zero, and the daemon announced its fallback
exactly as designed.

**The model's output is a probability, not an alpha.** Using it directly is what
made a waving hand look transparent: the camera motion-blurs it, the model is
honestly unsure, and 0.5 composites the hand half-way into its own blurred copy.
A viewer does not read that as uncertainty. `mask.rs` steepens the curve with a
smoothstep so confident pixels go solid, and keeps a narrow soft band at the
silhouette because that band is what makes hair look like hair -- a hard
threshold fixes the ghosting and produces a cut-out instead.

Temporal smoothing is deliberately mild. It steadies edges while someone sits
still, but it is a lag: turned up, it smears the silhouette behind anyone who
moves, which is a worse version of the problem it was added to fix.

**Devices are found by card label, never by number.** A loopback takes whatever
number is free when it is created, and that changes: the same machine with the
same setup gave /dev/video51 one boot and /dev/video10 the next. The unit does
not request a number, `device.rs` resolves the label, and Omarchy's own camera
relay does the same thing for the same reason. Anything that hardcodes
/dev/videoN works until the next reboot.

**The daemon owns the loopback, the widget owns nothing.** All state lives in
the daemon; the widget reads and commands it over IPC. A bar surface exists per
monitor, so anything the widget owned is state two monitors could disagree about.

**A dropped frame beats a late frame.** This is a live camera. If segmentation
or compositing overruns, ship the previous mask rather than delaying the frame —
a stale edge for one frame is invisible, a stutter is not.

## Dev workflow

```bash
# Use /usr/bin/python3 explicitly: a version manager (mise) shims a python3
# on PATH that has no openvino, and the import error looks like a missing package.
/usr/bin/python3 tools/convert.py   # ONNX → static FP16 IR in models/
/usr/bin/python3 tools/bench.py     # per-device latency, re-run after model changes
/usr/bin/python3 tools/load.py      # per-device CPU cost at a real 30 fps cadence

cd daemon && cargo test             # reference tests for the hot loops and the mask

omarchy plugin validate .           # manifest + entry points
```

Model provenance: MediaPipe Selfie Segmentation, taken as the ONNX export from
`onnx-community/mediapipe_selfie_segmentation` on Hugging Face. **Not** the
upstream `.tflite` — it carries a MediaPipe custom op
(`Convolution2DTransposeBias`) that OpenVINO's tflite frontend has no translator
for, so `read_model` fails on it outright. The `.tflite` is kept out of the repo
for that reason; don't re-add it as "the original".

## Rules for agents

1. Read the real file before writing. Recent Paths answers most "how does a bar
   widget…" questions; `/usr/share/omarchy/shell/README.md` answers the rest.
2. Measure before optimising, and put the number in this file. Every performance
   claim here is reproducible with a command in the repo.
3. The plugin half runs unsandboxed inside the shell. No network, no `sudo`, and
   nothing written outside `~/.local/state/omarchy/studio-effects/`.
4. Keep settings mirrored: `manifest.json`'s `barWidget.schema`, the daemon's
   config, and whatever clamps them.
5. Never enable effects on a camera the user did not point the daemon at, and
   never leave the loopback holding a frame after the daemon exits — a frozen
   last frame on a live call is worse than no device.
