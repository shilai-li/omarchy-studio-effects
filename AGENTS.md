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
daemon/src/framing.rs    where to crop so the subject stays centred
daemon/src/control.rs    the unix-socket control protocol and its state
daemon/src/bin/studio-effects.rs   the client that speaks it
daemon/examples/preview_cost.rs    how long the preview holds up the frame loop
daemon/examples/frame_cost.rs      each CPU stage of the frame loop, no camera needed
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

### Every number here was measured on AC, and battery is a different machine

On battery with the balanced profile the governor drops to `powersave` and the
cores sit around 1.2 GHz. The CPU stages roughly double; the NPU barely moves:

| stage, 1080p, 2 blur passes | on AC | on battery |
|---|---|---|
| blur | 9.11 ms | 20.01 ms |
| blend | 1.89 ms | 4.65 ms |
| **inference (NPU)** | **0.90 ms** | **1.04 ms** |
| total | 10.48 ms (32%) | 26.21 ms (79%) |

That 2.2x on blur against 1.16x on inference is the clearest evidence for the
whole design: the part running on the NPU is nearly immune to the power limit
that halves everything on the CPU. It is also why 720p is the default -- on
battery, which is when a laptop is on a call, 1080p leaves little headroom and
1080p with framing has none (96%).

Re-measure on battery before believing any budget claim in this file. The table
above predates both rounds of blur work below; the 20 ms it shows is the thing
they were aimed at, and has not been measured on battery since.

### Count the frames; the camera may not be doing 30

In dim light this USB camera lengthens its exposure (`exposure_dynamic_framerate`
is on) and delivers 8-10 fps. A per-frame figure worked out from an assumed 30
is then wrong by that factor, and a percentage of a core is a third of what the
same work costs at 30. Count them: `--stats-every 10` prints a line per ten.

A slow camera also leaves the cores idle between frames, and the clock follows
the load. Making the capture thread cheaper made the frame loop *read* 40%
slower on the same pinned cores -- prep included, which had not changed -- and
the two builds matched again as soon as both captured at the same size. Compare
builds with the same capture work, or not at all.

### What the daemon actually costs

`studio-effects-daemon`, per frame, USB camera, NPU, blur radius 12:

| resolution | prep | infer | blur | blend | total | of 33 ms |
|---|---|---|---|---|---|---|
| 1280x720 | 0.24 | 0.72 | 2.38 | 1.94 | **5.28 ms** | 16% |
| 1920x1080 | 0.20 | 0.85 | 4.07 | 1.94 | **7.06 ms** | 21% |

Measured before the second round below; `frame_cost` gives today's figures.

That is the frame loop alone, which is all the timing line times. A USB camera
also has to be decoded, on GStreamer's capture thread, from MJPEG, on the CPU:
3.5 ms a frame at 720p and 5.2 at 1080p, as much again as the table. It is a
budget for latency, not for CPU -- see "Ask the camera for the size you want".

1080p30 fits with room to spare. It did not at first -- the first working
version landed on 33.28 ms, exactly the budget -- and the 4.7x that closed the
gap came entirely from how the pixels are walked, not from the model or the
device:

| | before | after | what changed |
|---|---|---|---|
| blend @1080p | 17.11 ms | 1.94 ms | separable integer mask upscale |
| blur @1080p | 15.15 ms | 4.07 ms | reciprocal multiply, row-major vertical pass |
| mask prepare @1080p | 0.87 ms | 0.30 ms | quantise 256x256 once, stretch in integers |

The prepare row is an isolated walk of `MaskUpscaler::prepare`, not a full
pipeline re-time: the daemon was holding the camera when this was measured, so
the 1.94 ms blend figure above still includes the old prepare. Re-time the
daemon and fold it in.

**Blend.** The mask arrives at 256x256 and the frame is 2M pixels, so the naive
version sampled it bilinearly per pixel: four float loads and half a dozen float
ops, two million times. Separating the axes moves that work off the per-pixel
path -- one horizontal pass over 256 rows (491k operations), then two byte loads
and an integer lerp per pixel.

The remaining float work in that horizontal pass was converting every sample at
output width: two `clamp * 255` per column, 256 rows, 655k conversions at 720p
for a source that is 65k values. Quantising the mask to bytes once, then
stretching with the integer lerp the pass already used, is the same arithmetic
(the test pins that) and drops prepare from 0.59 ms to 0.22 ms at 720p, 0.87 ms
to 0.30 ms at 1080p.

**Blur.** Two ordinary-looking lines were most of the cost. The window average
was an integer divide, four million times, by a divisor that never changes; it
is a reciprocal multiply now. And the vertical pass walked a column at a time,
which is how a separable blur reads naturally and touches one byte from each of
`h` cache lines. Carrying a running sum per column turns it into sequential row
scans.

None of this touched the model, and neither should the next round: at 1080p the
model is 0.85 ms of 7.06.

### A second round, for the same bytes

`examples/frame_cost.rs`, pinned to a P-core, the package's build flags. The
example uses nothing the commit before it lacked, so "before" is the same
command run there:

| ms per frame | 720p, blur 12 x2 | | 1080p → 720p, blur 108 x3 | |
|---|---|---|---|---|
| | before | after | before | after |
| resample | -- | -- | 1.79 | 1.12 |
| blur | 3.41 | 2.62 | 5.30 | 4.03 |
| blend, with prepare | 1.58 | 0.63 | 1.57 | 0.63 |
| **CPU stages** | **5.19** | **3.44** | **8.83** | **5.94** |

On an E-core the ratios are larger: 1.6x on the blur, 4x on the blend. Every
change produces the bytes the old code did, and each has a test holding it to
the version it replaced (`blur_is_what_it_was`, `blending_is_what_it_was`,
`resampling_is_what_it_was`). What changed is what the compiler can do:

- **The blend in u16.** Every intermediate fits in 16 bits. The package builds
  for baseline x86-64, which has no 32-bit lane multiply, so the u32 version was
  being emulated; 16-bit lanes are native, and twice as many per instruction.
- **The blur's clamps only where they bite.** Keeping the window inside the row
  was a clamp at both ends on every pixel; only the first and last `r + 1` need
  one. The vertical pass also walked its sums twice a row, and its average is
  u32 now, which is exact for any window under 65,793.
- **The resample vertical first.** Bilinear is the same polynomial whichever
  axis goes first, and neither rounds in between: mixing the two source rows as
  one contiguous run vectorises, and leaves two loads a pixel for the
  horizontal half instead of four.

The horizontal blur pass is now most of the blur, and it is a running sum -- a
chain of dependent adds no compiler can widen. Fewer pixels is the lever left.

### Fewer pixels: the blur at half or quarter size

A box blur costs the same at any radius, so a wide blur at full size is paying
to compute detail it then averages away. `nv12::Blur` shrinks the frame by 2x2
averages, blurs that, and stretches it back with a fixed 3:1 lerp -- at half
size from radius 10, quarter from 48, full size below. Same example, same core:

| ms per frame | blur, full size | blur, as run | CPU stages |
|---|---|---|---|
| 720p, blur 12 x2 (half) | 2.60 | 0.95 | 1.77 |
| 1080p → 720p, blur 108 x3 (quarter) | 4.02 | 0.64 | 2.57 |
| 1080p, blur 12 x2 (half) | 5.78 | 2.16 | 3.64 |

Against the full-size blur, on a photograph and on a synthetic 720p room, every
pixel stays within 4 levels at 49 dB PSNR or better, and
`reduced_blur_stays_close_to_full_size` holds that. The thresholds are where
that stops being true: half size at radius 8 reached 5 levels in chroma, and
quarter size at radius 12 reached 9, because the shrink and the stretch add a
little softening of their own that only a wide blur hides. A flat colour comes
back exactly -- `(4v + 2) >> 2` and `(16v + 8) >> 4` are both `v` -- so this
cannot bring back the 199-for-200 darkening below.

Only factors both planes divide into exactly are used, so the stretch lands on
every row and column; anything else keeps the full-size blur.

### Both hot loops have reference tests

`cargo test` checks the fast paths against slow obvious ones -- a float bilinear
sampler for the upscaler, a naive nested-loop box blur for the blur. They exist
because these two functions were rewritten for speed after being verified only
by looking at a webcam, and a webcam cannot tell you about a bias of one level.

It caught one immediately. `(1 << 24) / 11` truncates, so a flat plane of 200
blurred to 199: every still background darkened by a level the moment effects
came on, uniformly enough that no one would see it and call it a bug. Both
roundings in `box_blur` are load-bearing for that reason.

It missed a bigger one, because every test handed it one channel. The chroma
plane is U and V interleaved, and `main.rs` passed it to `box_blur` as if it
were a row of one thing, so each U was averaged with the Vs beside it: a red
wall (U,V 90,240) came out 159,171, skin tones nearly grey, every coloured
background pulled toward magenta-grey. On a webcam that reads as the blur being
a bit washed out, which nobody files. `box_blur_uv` keeps the two apart, at
0.08 ms a pass over the version that mixed them at 720p, and
`chroma_blur_keeps_u_and_v_apart` feeds it a colour rather than a grey.

Note that a snapshot showing everything blurred is usually not a fault. The
model wants a person filling a reasonable part of the frame; with the subject
small or far the mask is legitimately near-empty and the whole frame blurs. Reach
for the tests before the pipeline.

There is also real quality headroom: 256x256 MediaPipe is the cheap end of the
model range, and the budget would carry something much better. Do not spend it
on a bigger model until the composite is off the CPU.

A few CPU leftovers were measured and left alone, because they do not move the
needle once LLVM has seen them:

- `blend_*`'s `/ 255` is already a multiply at `opt-level=3`. A hand-written
  reciprocal is 5% at the compiler's default CPU, and changes mid-alpha by a
  level.
- Reusing `box_blur`'s per-call `Vec<u32>` column sums is ~1%.
- Blurring at half resolution was measured here once as not cheaper, "the scale
  costs what the smaller box saves". That was with the general crop-anywhere
  resampler doing both scalings. With fixed factors it is 2.7x cheaper at the
  default radius -- see "Fewer pixels" above. The scaling has to be cheap, not
  absent.

After both rounds the frame loop's CPU stages are under 2 ms at the default and
2.6 at this machine's 1080p-capture config, and the largest CPU cost left is
not in the frame loop at all: it is the capture thread decoding the camera's
MJPEG, 3.5 ms a frame at 720p. A GPU composite would still make sampling free,
but it now saves about 2 ms rather than 10.

## Invariants

**Two models, and the NPU's spare capacity is what pays for the second.**
`segmentation` is MediaPipe selfie segmentation at 0.8 ms; `matting` is
RobustVideoMatting at 3.4 ms, a true alpha matte with recurrent state, so hair
reads as hair and edges hold still instead of shimmering. Both output
`[1,1,256,256]`, which is why the second is a drop-in: the upscaler, the subject
box and the blend are untouched.

The model is switched live, not only at startup: the frame loop compares the
requested name against the loaded one each pass and reloads on a change, keeping
the old segmenter until the new one has compiled. A load that fails puts the old
name back in the settings rather than leaving the panel showing a model that is
not running. Which names exist is a directory scan -- installed models first,
and the checkout's only when nothing is installed, never merged, or a working
tree's half-converted leftovers become entries the panel offers.

Getting RVM onto the NPU needed two things, and both failures looked like the
model being unsuitable rather than the export being wrong. Its
`downsample_ratio` is a runtime input, so the sizes inside its encoder depend on
a value the compiler cannot see and the NPU refuses the whole model, reporting a
dimension of -9223372036854775808 -- OpenVINO's marker for "dynamic".
`tools/convert.py` freezes it. Its recurrent states have no declared size, so
they are discovered by one CPU run and pinned.

**Zero the recurrent state tensors.** `Tensor::new` returns uninitialised
memory, and in a recurrent model whatever is in it is fed back as the next
frame's input forever: one NaN on the first frame and the model returns NaN for
the life of the daemon. It presents as a mask of zero -- every frame blurred,
subject included -- with nothing logged and no error. The comment claiming the
states start at zero was there before the code that made it true.

**Swap the model before writing the frame into it, not after.** The loop wrote
the camera into `seg`, then replaced `seg`, then inferred -- so the swap frame
segmented the new segmenter's unwritten input tensor, which is `Tensor::new`'s
uninitialised memory. On `segmentation` that is one bad frame. On `matting` it
is every frame after, because the garbage becomes the recurrent state and is fed
back forever, and the only cure is a restart: switching models looked like it
did nothing until the camera was turned off and on. Same failure as the
un-zeroed recurrent state, one call site along. Anything holding a borrow of the
segmenter's buffers has to be re-done after a swap.

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

It must never wait for the encoder, either. `offer` collects the JPEG finished
since the last interval and hands over this frame for next time, so the preview
runs a tenth of a second behind and the frame loop does not stop for it.
Waiting, which is what it did first, held the loop 1.1 ms at 720p and 2.8 ms at
1080p on every third frame the panel was open. This file once called that
"inside the noise", measured with the timing line -- which does not time the
preview at all. `cargo run --release --example preview_cost` times `offer`
itself: 0.2 ms held now, and 1.2 ms of CPU per encode on GStreamer's thread,
down from 2.7 at 1080p, because the frame is scaled before anything else touches
it and `jpegenc` takes NV12 as it is.

**Voice focus is on the CPU on purpose, and that is the interesting part.** The
NPU thesis does not transfer to audio. Segmentation was worth moving because it
costs 12.4% of a core on the CPU and 1.6% on the NPU; RNNoise is an
85k-parameter GRU costing about 1%, so there is nothing to save and no power
story. Only a much heavier model -- DeepFilterNet, say -- would change that, and
it is not currently obtainable as ONNX. Do not put speech on the NPU here
without first measuring what it would actually buy.

It is also a separate unit and a separate process, and must stay that way. Voice
and camera are independent -- denoising a call with the camera off is normal --
and they fail independently, so a camera that will not start must not take the
microphone filter down with it. The widget therefore asks systemd about it
rather than the camera daemon, which knows nothing about audio.

The filter-chain runs as its own PipeWire instance rather than as a drop-in
under `~/.config/pipewire/pipewire.conf.d/`, because a drop-in is always loaded:
the filter would exist from login whether or not anyone wanted it. And
`capture.props` sets `node.passive = true`, so the microphone is only opened
when something records from Voice Focus -- otherwise leaving it on would hold
the mic, which is the audio version of leaving the camera light on.

**Voice Focus takes the default microphone, and must give it back.** Most
applications offer only "default", so a filter nobody can select is a filter
nobody uses. `voice-default claim` runs from `ExecStartPost` and `release` from
`ExecStopPost`, and the release half is the important one: the `voice_focus`
node exists only while the service does, so a default left pointing at it after
a stop points every one of those applications at a device that is gone.

The default is set **by node name**, through `pw-metadata`'s
`default.configured.audio.source`, not by the numeric id `wpctl set-default`
takes. Ids are handed out afresh whenever a node appears, so one saved at start
means nothing by the time it is read at stop.

Making it the default does **not** cause the filter to capture from itself,
despite appearances. That was seen once and chased for a long time; the cause
was several `pipewire -c` test instances running at once, each publishing its
own `voice_focus`, so one instance's capture linked to another's output. With a
single instance WirePlumber's `node.link-group` prevents the self-link, verified
by restarting the service with `voice_focus` already default and watching the
capture land on the hardware microphone. **Kill stray instances before
concluding anything about routing.**

**Measuring audio here is harder than it looks, and three separate mistakes
each read as "silence".** Record with `timeout -s INT`, not plain `timeout`:
SIGTERM leaves `pw-record` a file with no data chunk, which reads as silence
rather than as a broken file. Target nodes by **name**, never by the id from
`wpctl status`: ids are reassigned constantly and a stale one records from
whatever now holds it. And never test a denoiser with a sine wave -- RNNoise is
built to remove exactly that, so a pure tone proves nothing either way. Use a
broadband, amplitude-modulated signal, and always record the raw microphone in
the *same run* to prove the sound reached it at all.

**Ports existing does not mean the graph runs.** A filter-chain publishes its
ports from the config before the graph is verified, so `pw-link` showed
`voice_focus:capture_FL` and `capture_FR` for a graph that was refusing to start
and producing pure silence. Checking the port list looked like verification and
was not. Read the log: `pipewire -c <conf>` with `log.level = 2` says exactly
what is wrong, and a working graph reports no error at all.

**Ask the plugin for stereo; do not widen a mono graph afterwards.** RNNoise
ships `noise_suppressor_mono` and `noise_suppressor_stereo`, and the mono one is
the obvious choice for a microphone -- but a mono source is monitored through
one speaker only, and every way of widening it afterwards fails. Measured
against a tone the microphone heard at 10%:

| graph | result |
|---|---|
| mono plugin, one output | passes audio, ch1 exactly 0.000% |
| mono plugin, output named twice | refused: "already used as output 0, use copy" |
| mono plugin + `copy` node, two outputs | silence |
| mono plugin, stereo `playback.props` | silence |
| **stereo plugin** | **two ports in and out, audio on both channels** |

So the config uses `noise_suppressor_stereo` and lets the graph work out its own
ports. Do not set `audio.channels` or `audio.position` on `capture.props` to
help: that made the capture node adopt the microphone array's four channels
instead of negotiating the two the plugin wants.

**Framing is a problem about holding still, not about tracking.** Finding the
subject is free -- the mask is already a per-pixel map of them, so `subject_box`
is a scan of 65k values and no second inference. A face detector would cost
another model and could disagree with the one doing the compositing, which shows
as the frame drifting away from the cut-out.

Everything in `framing.rs` exists to keep the crop still: a dead zone the subject
may drift inside before anything moves, heavy easing once it does, a zoom cap
because the crop is scaled back up and past a point the picture is visibly soft,
and a hold when the subject is lost rather than a snap to the full frame. A frame
that follows every twitch is worse than one that never moves -- the viewer sees
the room sliding behind a subject who appears pinned, and reads it as a broken
camera. The tests are written against that, not against tracking accuracy.

How far it crops is the one thing people reach for, because it depends entirely
on how far away they sit -- so the limit is live-adjustable and shown as a slider
while framing is on, not fixed at startup. It defaults to 2x rather than
something tighter for the same reason: someone well back from the camera needs
the room, and a cap that never binds for a close subject is invisible to them.

**Capture can be larger than output, and the composite still runs at output
size.** Framing crops and scales back up, so cropping from a frame the same size
as the output always costs sharpness. Capturing 1920x1080 for a 1280x720 output
gives the crop real pixels: zooming to 150% is then a straight 1:1 read.

The order this demands is resample **first**, composite after. Blurring at the
capture size and scaling down afterwards would run the expensive stages on the
bigger frame for nothing -- 26.21 ms against 12.26 for the same output. The
resample costs about 1 ms (1.1 on a P-core, 1.7 on an E-core, 1080p to 720p),
and only when it is actually doing something: an uncropped frame at matching
sizes takes a row copy rather than a bilinear identity. A USB camera adds its
own share on the capture thread, since the bigger picture has to be decoded:
5.2 ms of MJPEG at 1080p against 3.5 at 720p.

The consequence is that the mask must be read over the crop rather than the
whole frame, which is what `MaskUpscaler::aim` is for. Get that wrong and the
cut-out drifts away from the person.

The crop runs **last** in the sense that matters -- segmentation still sees the
whole captured frame, so both always see
the whole frame. A subject who walks outside the crop still has to be findable,
or the camera could never follow them back.

**Run the daemon the way the unit runs it, not the way you would type it.** A
`bool` field in clap is a bare flag, and a unit has no way to omit an argument,
so `--framing=${FRAMING}` met `--framing` and the service exited
2/INVALIDARGUMENT in a restart loop. Every manual test passed, because every
manual test typed `--framing` by hand. Switches therefore take a value
(`on`/`off`/empty) and need `action = clap::ArgAction::Set` -- a `value_parser`
alone does not stop clap inferring a flag, and that failure is at runtime in the
service rather than at compile time. `test/unit-args-test.sh` runs the daemon
with the unit's own ExecStart so this cannot come back.

**Adding a setting means four places, and the parser is the one that gets
forgotten.** A new knob needs: the daemon's `Settings` and its socket command,
the `json()` reply, one of the widget's row lists, and **`parseStatus`**.
Skipping the last one is silent: the panel renders a row, the daemon accepts
changes, and the value sits at its minimum while the daemon plainly uses
something else. It showed up as Smoothness reading 1 against a daemon reporting
2. A test now walks `PARAMS` against a parsed reply so that fails instead.

`panelRows` walks three lists, not one, and which you want is decided by how the
row is operated rather than by what it holds: `PARAMS` is stepped with the arrow
keys, `TOGGLES` is flipped with enter, `CHOICES` is one of a set the daemon
supplies. A choice is five places, not four, because the daemon reports both the
current value and the list of them -- `supportedParams` wants both before it
will show the row, which is what makes a machine with one model installed get no
Model row instead of a row that cycles back to itself.

**One glyph, and state lives in the tooltip.** The bar icon started as a camera
and stopped being honest once the same widget also switched a microphone
filter -- half of what it controls is not a camera. It is sparkles now, which
say "effects" without claiming a device.

The glyph deliberately does not change with state. The bar already dims an
inactive widget and accents an active one, so a second encoding in the glyph is
a third source of truth that will eventually disagree with the other two. What
the glyph can no longer say, the tooltip does, and it has to name both halves:
with one icon, someone whose camera is off and whose microphone filter is on has
no other way to tell why the widget looks active.

**The widget owns no settings, and that is the point.** The daemon holds the
effect and the blur radius and answers every command with its whole state, so
there is one copy of the truth and the widget only ever shows it.
`manifest.json`'s `barWidget.schema` is empty for that reason -- it once
declared `effect` and `blurStrength`, written before the daemon existed, and
keeping them would have given the widget a second opinion that goes stale the
moment the CLI, a keybinding or another monitor changes anything. The usual rule
about mirroring a setting in three places does not apply to state that belongs
to something else.

**The camera switch stops the process, so live settings have to be written
down.** Turning the camera off is not a pause: it stops the daemon, and every
setting the panel changed lived only in that process. Coming back from the
config file meant the switch quietly undid the last several things the user had
done. `state.rs` saves the panel-changeable settings to
`~/.local/state/studio-effects/settings.json` on every change and applies them
over the config at startup.

On change, not on exit -- systemd stops the daemon with a signal, and anything
written only on the way out is the thing that never runs. Written to a temporary
name and renamed, so a daemon killed mid-write leaves the previous choices
rather than half of the new ones. And a remembered `replace` is refused when no
image is configured, exactly as the socket refuses it: restoring a state the
socket would not have allowed is how the camera comes back showing nothing.

Only what the panel can change is saved. Resolution, model and camera stay with
the config, because remembering those would make editing that file look broken.

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

**Ask the camera for the size you want, ahead of decodebin.** `decodebin`
accepts anything, so `v4l2src` behind it never learns what size is wanted
downstream and opens the camera's largest mode. The 720p default spent its life
decoding 1080p MJPEG and scaling it down: 6.35 ms of CPU a frame, where asking
for 720p costs 3.52. Nothing showed it, because the decode runs on GStreamer's
capture thread and the timing line times only the frame loop -- a third of the
daemon's CPU, and in no number in this file.

The capture caps now list the size first, raw before MJPEG because raw needs no
decode, then anything, so a camera without that size still opens: asked for
1024x576, this one gives its nearest mode, 960x540, and `videoscale` makes up
the difference. The daemon says what it got, once, on the first frame --
`camera delivers image/jpeg 1280x720 at 30/1 fps` -- and that line is the one to
read before believing a capture setting did what it says. Measured over counted
frames, with a capture-only run subtracted:

```bash
time gst-launch-1.0 -q v4l2src device=/dev/video0 num-buffers=90 \
  ! decodebin ! videoconvert ! videoscale \
  ! video/x-raw,format=NV12,width=1280,height=720,framerate=30/1 ! fakesink sync=false
# and again with the preference ahead of decodebin:
#   v4l2src ... ! 'image/jpeg,width=1280,height=720,framerate=30/1;image/jpeg;video/x-raw' ! decodebin ...
```

Hardware MJPEG decode is not the next step. Through VA-API, via ffmpeg because
GStreamer's `va` plugin is a separate package, it cost more than twice the CPU
of `jpegdec`: downloading the decoded surface to system memory dominates.

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

**`bar` is a facade, not the Bar.** What gets injected is a `PluginBarApi`
(`shell/Ui/PluginBarApi.qml`): presentation state mirrored as plain properties,
operations delegated through scoped callbacks. First-party panels get the real
`Bar.qml` and can write its properties; a plugin cannot. Anything shared and
mutable is exposed there **readonly** with a `setX()` beside it —
`centerHoverRevealSuppressed` / `setCenterHoverRevealSuppressed()` is the one
this plugin touches. Assigning to a readonly QML property throws a `TypeError`
rather than failing quietly, and the throw takes out the rest of the calling
function, so **call the setter and feature-test it with `typeof … ===
"function"`**, never `"name" in bar` — the `in` check passes on a readonly
property and tells you nothing.

**Closing may not depend on anything.** `close()` hides first and does the rest
after. The panel is a full-screen layer-shell surface holding keyboard focus:
anything that throws ahead of `controller.hide()` strands the user behind a
surface that eats every key and click, including the escape and the
outside-click that would have dismissed it, and the bar reads as frozen.
Omarchy 4.0.3 turned `centerHoverRevealSuppressed` readonly and did exactly
that. Order the function so the release is unconditional.

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
cargo run --release --example preview_cost   # the preview's cost, which the timing line cannot see
cargo build --release --example frame_cost && taskset -c 2 target/release/examples/frame_cost 1920x1080 1280x720 108 3
bash test/unit-args-test.sh         # the systemd unit's arguments, against the real daemon

# After an OpenVINO update the daemon dies in the loader (exit 127, "cannot open
# libopenvino_c.so.<version with the dots removed>"). Cargo will NOT fix it on
# its own: no source changed, so it reuses the linked binary and prints
# `Finished` over a broken one. Force the relink, and check what you got.
touch daemon/src/main.rs && cargo build --release --manifest-path daemon/Cargo.toml
ldd daemon/target/release/studio-effects-daemon | grep openvino_c

omarchy plugin validate .           # manifest + entry points
bash test/model-test.sh             # Model.js under plain node, no compositor
bash test/voice-check.sh            # does Voice Focus pass your voice? (talk into it)

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
