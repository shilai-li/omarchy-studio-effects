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
manifest.json            plugin manifest; barWidget.schema is empty, deliberately
BarWidget.qml            the bar glyph; owns the daemon conversation
Panel.qml                the effect list, a read-out of the widget
Model.js                 pure logic: commands, reply parsing, glyphs
test/model-test.sh       unit tests for Model.js -- plain node, no compositor
models/*.onnx            model source. The IR beside it is built, not committed
tools/convert.py         ONNX -> static FP16 IR
tools/bench.py           per-device inference latency
tools/load.py            per-device CPU cost at a real 30 fps cadence
daemon/src/main.rs       CLI, GStreamer wiring, per-stage timing
daemon/src/nv12.rs       the frame maths, and the tests that pin it
daemon/src/mask.rs       conditioning the model's output into an alpha
daemon/src/background.rs decoding a replacement background once
daemon/src/preview.rs    the JPEG the bar widget shows
daemon/src/segmenter.rs  device choice, model cache, one inference per frame
daemon/src/device.rs     resolving a v4l2 device by card label
daemon/src/control.rs    the unix-socket control protocol and its state
daemon/src/bin/studio-effects.rs   the client that speaks it
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

**Off means the camera is released, not that compositing stopped.** `effect
none` is not an off switch: the daemon still holds the camera open, so the
recording light stays lit, nothing else can open the real camera, and every
frame is still captured. The widget's on/off starts and stops
`studio-effects.service` itself, and the unit is deliberately not enabled at
boot.

This is why the two units are separate, and the split has to stay that way.
`studio-effects-loopback.service` keeps "Studio Camera" present at all times so
an app can select it once and keep that selection, while the daemon behind it
comes and goes. Merging them would mean either a camera that vanishes from
every app's picker when effects are off, or a camera held open all day.

Starting is not instant: systemd returns before the daemon has opened the
camera, so the widget re-reads the state until it agrees rather than concluding
from one silent reply that starting failed, and says "…" while it waits. A
button that looks inert gets pressed again.

**The preview must never open Studio Camera.** It is the obvious way to do it
and it breaks the video call. `v4l2loopback` permits ten openers, but the
second one's `REQBUFS` invalidates the first one's buffer pool: measured here, a
reader joining a device another reader was already streaming produced `Failed to
allocate a buffer` **in the one that was already working**. So the preview
cannot be a `QtMultimedia` `Camera` on the output device, however much shorter
that code would be. The daemon publishes a 320x180 JPEG to `$XDG_RUNTIME_DIR`
instead, which contends with nothing.

Requests for it follow the **daemon appearing**, not the panel opening. Sending
it once from `Panel.open()` is the obvious thing and it is wrong: the power
switch stops and restarts the daemon underneath a panel that stays open the
whole time, so `open()` never runs again and the new daemon is never asked. The
symptom is the honest one -- the panel reports that no preview is coming,
because none was requested -- which reads as the preview being broken rather
than as never having been asked for, and closing and reopening "fixes" it,
which points the investigation at the panel instead of the request.

It is written to a temporary name and `rename(2)`d into place, because the
widget re-reads the file on a timer: rename is atomic within a filesystem, so a
reader gets the previous whole frame or the next whole frame, never half of one.
It is published only while a panel is open — the widget asks on open and again
on close — and the encoder is built on first use and dropped when nothing is
watching, taking the last frame with it, so a widget can never show a still of a
camera that is no longer running. A frame left behind by a daemon that was
killed rather than stopped is removed at the next start.

Cost is inside the noise: 12.90 ms/frame with it off against 12.69-13.08 with it
on, at 1080p, publishing about nine frames a second.

**The widget owns no settings, and that is the point.** The daemon holds the
effect and the blur radius and answers every command with its whole state, so
there is one copy of the truth and the widget only ever shows it.
`manifest.json`'s `barWidget.schema` is empty for that reason -- it once
declared `effect` and `blurStrength`, written before the daemon existed, and
keeping them would have given the widget a second opinion that goes stale the
moment the CLI, a keybinding or another monitor changes anything. The usual rule
about mirroring a setting in three places does not apply to state that belongs
to something else.

**Settings change over a socket, never by restarting.** Restarting the service
to change an effect drops the camera for a second, which on a live call is a
black frame everyone sees. `control.rs` listens on a unix socket in
`$XDG_RUNTIME_DIR` and the frame loop reads the settings once per frame -- once,
because a frame that blurred with one radius and blended with another would
tear.

The client is a separate short-lived binary rather than a library, because the
bar widget runs inside the `omarchy-shell` process and spawning a command whose
stdout is one line of JSON is the shape Omarchy plugins already use. Every reply
carries the full state, so a caller never has to ask twice, and a refusal is
both an `"error"` field and a non-zero exit -- a widget that only checked the
exit status and one that only parsed the JSON would each otherwise miss it.

Two failure modes are handled explicitly and both are worth keeping: a socket
left by a killed daemon is removed, but only after trying to connect to it,
because that is the only way to tell a stale file from a daemon already running.
And a daemon that cannot get its socket still runs -- it is a working camera
that merely cannot be reconfigured, which is not worth dying over.

**A USB camera has exactly one consumer, and the service is usually it.** Once
`studio-effects.service` is running it holds `/dev/video0`, so any manual
`studio-effects-daemon` run against the same camera produces no frames at all.
It does not error -- `v4l2src` simply never delivers a buffer, so the daemon
prints its two startup lines and then sits there looking like it hung, or like
whatever you just changed broke the pipeline. `systemctl --user stop
studio-effects` before testing by hand. This costs at least one debugging
session per person who forgets, so it is worth suspecting early: startup lines
present, no timing lines, no error.

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
bash test/model-test.sh             # Model.js under plain node, no compositor

# Reinstall AND restart, in that order. Reinstalling alone leaves the running
# shell on the cached Model.js, which fails as a widget that does nothing rather
# than as an error -- see below.
git archive HEAD | tar -x -C ~/.config/omarchy/plugins/shilai_li.studio-effects
omarchy-restart-shell
QT_FORCE_STDERR_LOGGING=1 /usr/lib/qt6/bin/qmllint -I /usr/share/omarchy/shell BarWidget.qml
```

Saving under `~/.config/omarchy/plugins/` hot-reloads QML, so the edit loop is
save then open the panel. **Two things do not hot-reload.** An already-bound
`IpcHandler` target belongs to the first handler bound to it for the life of the
shell process, so adding a method and reinstalling leaves
`omarchy-shell <id> <newMethod>` answering `Function not found` while everything
else works; `Model.js`'s imported copy is cached the same way. Both need
`omarchy-restart-shell`. The panel changing while the IPC does not is exactly
what makes this hard to spot -- it reads as the widget failing to do the thing,
not as a stale handler.

This has now cost time twice, and the second time from the other direction: a
plugin reinstalled while the shell was already running kept the old `Model.js`,
so the widget never sent a command that had just been added to it. Nothing
logged, nothing looked broken, and the panel rendered perfectly -- it simply did
not do the new thing. **Compare the shell's start time against the plugin
directory's mtime before believing anything else**:

```bash
ps -o lstart= -p $(pgrep -x quickshell | head -1)
date -r ~/.config/omarchy/plugins/shilai_li.studio-effects/Model.js
```

If the files are newer than the shell, that is the bug, whatever the symptom
looks like.

Reading qmllint output: `qs.Commons` and `qs.Ui` cannot resolve outside
Quickshell, so every file emits a cascade of `[import]`, `[unqualified]`,
`[unresolved-type]` and friends. That is noise; the shipped built-ins emit the
same. Compare *categories* against `omarchy-recent-paths`, not counts -- a
category ours emits that theirs does not is the one worth chasing.

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
6. Before declaring the widget done: `omarchy plugin validate` clean, qmllint
   *categories* matching `omarchy-recent-paths` (not counts — the shipped
   built-ins emit the same noise outside Quickshell), `bash test/model-test.sh`
   green, and the panel actually opened and looked at. All four have caught
   something the others did not.
