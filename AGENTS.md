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
| 1280x720 | 0.24 | 0.76 | 6.41 | 7.34 | **14.74 ms** | 44% |
| 1920x1080 | 0.26 | 0.76 | 15.15 | 17.11 | **33.28 ms** | **100%** |

720p at 30 fps has room to spare. **1080p does not fit** -- it lands exactly on
the budget, which in practice means dropped frames. 720p is therefore the
default, and 1080p is a performance job, not a flag someone can just pass.

Two things this table settles:

- **Converting only what the model needs works.** `prep` turns a 1280x720 NV12
  frame into the model's 256x256 RGB input in 0.24 ms. The equivalent GStreamer
  convert-and-scale measured 4.22 ms. Converting 65k pixels instead of 2M is
  where that 18x came from, and it is why nothing in the daemon ever
  materialises an RGB frame.
- **Inference is now 2% of the frame.** At 1080p the model costs 0.76 ms while
  blur and blend cost 32 ms between them. Choosing a device barely moves the
  total -- NPU 14.74, CPU 16.27, GPU 16.77 at 720p -- because the device only
  ever had 1 ms to win. The device still matters, but for the CPU it frees, not
  the time it saves.

The next optimisation is therefore blur and blend, and both are wide open: they
are scalar single-threaded loops over 2M pixels. Three obvious moves, cheapest
first -- blur the background at quarter resolution and upscale (a blurred
background has no detail worth carrying at full res, so this is ~16x less work
for no visible change), hoist the mask's bilinear sample out of the per-pixel
path, and thread the row loops. None of these need a faster model.

There is also real quality headroom: 256x256 MediaPipe is the cheap end of the
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

**The shipped unit must set `SupplementaryGroups=render`.** Telling a user to
join `render` is not enough and the reason is worth knowing, because it looks
exactly like a bug in the device selection. Supplementary groups are fixed when
a process is created, so joining the group only reaches processes started after
it -- and on Omarchy every terminal descends from the long-lived `systemd --user`
manager, which is not restarted by logging out (`Linger=no` only stops it once
*every* session closes) and does not refresh its credentials on
`daemon-reexec`. A user can therefore join `render`, log out, log back in, and
still have no NPU in any terminal until a reboot. Verified: with the group,
`npu_busy_time_us` climbed 54463 us over nine seconds; without it, zero, and the
daemon fell back to the GPU exactly as designed. A service that declares the
group itself never depends on any of this.

**The daemon owns the loopback, the widget owns nothing.** All state lives in
the daemon; the widget reads and commands it over IPC. A bar surface exists per
monitor, so anything the widget owned is state two monitors could disagree about.

**A dropped frame beats a late frame.** This is a live camera. If segmentation
or compositing overruns, ship the previous mask rather than delaying the frame —
a stale edge for one frame is invisible, a stutter is not.

## Dev workflow

```bash
python3 tools/convert.py            # ONNX → static FP16 IR in models/
python3 tools/bench.py              # per-device latency, re-run after model changes
python3 tools/load.py               # per-device CPU cost at a real 30 fps cadence

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
