# Omarchy Studio Effects

Camera background blur and replacement for [Omarchy](https://omarchy.org), with
the segmentation run on your laptop's NPU.

> **Early development,** but complete end to end: a daemon that segments your
> camera and publishes the result as a second camera, and a bar widget to
> control it.

The point is not speed. Segmentation costs well under a millisecond against a
33 ms frame budget, so it would run fine on the CPU. The point is what holding
30 fps *costs* — measured on a Core Ultra X7 358H:

| running segmentation on | CPU it burns |
|---|---|
| the CPU | 12.4% of a core |
| the GPU | 5.0% of a core |
| **the NPU** | **1.6% of a core** |

A seventh of the CPU, and it leaves the GPU alone for the encode and render your
video call is already doing. Effects you can leave on for an hour without
hearing the fan.

## Shape

Two halves, one repo:

- **`studio-effects-daemon`** — reads your camera, segments each frame,
  composites, and publishes the result as a second camera your apps can pick.
- **`shilai_li.studio-effects`** — a bar widget to turn effects on and choose
  one.

The daemon ships as a package; the widget installs with `omarchy plugin add`.
They are separate because an Omarchy plugin is cloned files only — the installer
builds nothing and runs nothing.

## Install

```bash
cd packaging && makepkg -si
sudo systemctl enable --now studio-effects-loopback   # creates "Studio Camera"
omarchy plugin add https://github.com/shilai-li/omarchy-studio-effects.git --enable
```

Then pick **Studio Camera** in Zoom, Meet, or any browser, and turn effects on
from the bar.

Only the loopback is enabled at boot. **Studio Camera is always there** — apps
can select it and keep that selection — while the daemon behind it runs only
when you turn effects on. That matters: while the daemon runs it holds your
camera open, so the recording light is lit and nothing else can open the real
camera. Off means off.

```
studio-effects-loopback.service   always up    → "Studio Camera" exists
studio-effects.service            on demand    → camera open, NPU working
```

If you would rather it were always up, `systemctl --user enable --now
studio-effects`.

To point it at a different camera, or change the resolution or blur:

```bash
cp /usr/share/studio-effects/studio-effects.conf.example ~/.config/studio-effects.conf
$EDITOR ~/.config/studio-effects.conf
systemctl --user restart studio-effects
```

`studio-effects-daemon --list-devices` prints every camera with its card label.
Prefer labels over `/dev/videoN` in that file: numbers move between boots.

To put an image behind you instead of a blur:

```conf
EFFECT=replace
BACKGROUND=/home/you/.config/omarchy/backgrounds/catppuccin/wallhaven-1pzdg1.jpg
```

It is cropped to your camera's aspect and scaled to fill, so a wallpaper does
not arrive letterboxed. `EFFECT=none` passes the camera through untouched while
keeping the output device alive, so apps do not lose their selection when you
turn effects off.

## The bar widget

```bash
omarchy plugin add https://github.com/shilai-li/omarchy-studio-effects.git --enable
```

A glyph on the right of the bar shows what the camera is doing — off, blurred,
or replaced — and clicking it opens the panel, which shows a **live preview of
what the far end of your call actually sees**, above the effect list.

The preview runs only while the panel is open. It does not open Studio Camera
to do it — a second reader on that device invalidates the first one's buffers
and would break the call it is previewing — so the daemon publishes a small
frame for the widget instead.

| Key | |
|---|---|
| `↑` `↓` (or `k` `j`) | move |
| `enter` / `space` | choose that effect |
| `←` `→` (or `h` `l`) | adjust the selected setting |
| `c` | camera effects on or off |
| `v` | voice focus on or off |
| `f` | turn effects off, or back on |
| `r` | re-read the daemon |
| `esc` | close |

Below the effects are the settings that apply to whichever one is on:

| | |
|---|---|
| **Auto framing** | tracks you and keeps you centred |
| **Zoom in** | how close it may crop, 100–300%. Raise it if you sit far back |
| **Blur** | radius, 0-200 |
| **Smoothness** | blur repeats, 1-3. One streaks against hard edges; two looks Gaussian |
| **Darken** | dim the background, 0-100, so you stand out |
| **Desaturate** | drain its colour, 0-100 |

Auto framing finds you in the segmentation mask the effects already produce, so
it needs no face detector and no second model. It crops and scales rather than
moving anything, so it costs resolution: about 11 ms a frame at 1080p, the most
expensive thing here, which is why it is off unless you ask for it.

If you sit far back, capture larger than you publish — the crop then has real
pixels to use instead of upscaling:

```conf
WIDTH=1280
HEIGHT=720
CAPTURE_WIDTH=1920
CAPTURE_HEIGHT=1080
```

Zooming to 150% is then a straight 1:1 read with no softness at all, and it
costs about 2 ms a frame because blur and compositing still run at the output
size.

It is built to move as little as possible. You can drift a little without the
camera reacting at all, a real move is followed slowly, and stepping out of shot
holds the frame rather than snapping back — a camera that tracks every twitch
makes the room slide around behind you, which looks broken rather than framed.

Darken and desaturate touch the background only, never you, and work behind a
replaced image as well as a blur. Settings the current effect ignores are not
shown — replace has no blur to soften, so it offers no blur rows.

**Replace background** only appears when the daemon actually has an image
loaded, because offering a choice it would refuse is worse than not offering it.
If the daemon is not running the panel says so, and says which command starts
it.

## Voice Focus

A denoised copy of your microphone, published as a second source called **Voice
Focus**. Turn it on with `v` in the panel, or click its row.

While it runs it becomes your default microphone, and the previous one is put
back when it stops — so applications that only follow "default", which is most
of them, get the filter without being reconfigured one by one. Set
`VOICE_SET_DEFAULT=no` in `~/.config/studio-effects.conf` if you would rather
choose it per application.

It is a separate service from the camera, on purpose: denoising a call you are
on with your camera off is a normal thing to want, and the two fail
independently.

The suppression is RNNoise, on the CPU deliberately. The NPU earns its place in
the video path because segmentation costs 12.4% of a core there against 1.6% on
the NPU. RNNoise is an 85k-parameter network costing about 1% — there is nothing
to move, and only a much heavier model would change that.

The filter is passive, so your microphone is not actually opened until something
records from Voice Focus. Leaving it on does not hold your mic — which is why it
is worth enabling and leaving alone:

```bash
systemctl --user enable --now studio-effects-voice
```

The node only exists while the service runs, so turning it off removes the
device from every application's list. An app that had it selected loses its
microphone. (Studio Camera does not have this problem: its loopback is a
separate always-on service.)

If your voice is not coming through, this says whether the microphone or the
filter is at fault — it records both at once and compares them:

```bash
bash test/voice-check.sh    # then talk for five seconds
```

## Changing things without restarting

Editing the config and restarting drops the camera for a second, which on a
live call is a black frame everyone sees. The `studio-effects` command talks to
the running daemon instead:

```bash
studio-effects                    # or `status` -- what it is doing now, as JSON
studio-effects toggle             # effects off, or back on to the last one
studio-effects effect replace
studio-effects blur 40
```

Every command answers with the daemon's full state, so a caller never has to ask
twice, and a refused change is an error in the JSON *and* a non-zero exit.

A Hyprland bind:

```
bindd = SUPER CTRL, B, Toggle camera effects, exec, studio-effects toggle
```

## Requirements

An Intel Core Ultra with an NPU, though it falls back to the GPU and then the
CPU and works fine on either. On Arch/Omarchy the NPU needs:

```bash
sudo pacman -S intel-npu-driver openvino-intel-npu-plugin
```

If the daemon logs `no NPU available, falling back to GPU`, check the
permissions on `/dev/accel/accel0`. It is world-accessible on a current Arch
system; if yours is `0660 root render`, join that group **and reboot** -- logging
out is not enough, because your terminals inherit their groups from a
`systemd --user` manager that a logout does not restart.

## Development

```bash
python3 tools/convert.py   # fetch-free: ONNX -> static FP16 OpenVINO IR
python3 tools/bench.py     # per-device inference latency
python3 tools/load.py      # per-device CPU cost at a real 30 fps cadence

cd daemon && cargo build --release
./target/release/studio-effects-daemon --input /dev/video0 --snapshot /tmp/x.png
```

Run without `--output` and frames are processed and dropped, which is how the
pipeline gets measured on a machine with no spare loopback device. `--snapshot`
writes one processed frame as a PNG, which is the only way to see what the
composite looks like without one.

Per frame at 720p on the NPU: prep 0.24 ms, inference 0.76 ms, blur 6.41 ms,
blend 7.34 ms. 1080p currently lands on 33.28 ms and so does not hold 30 fps --
see AGENTS.md for why that is a blur-and-blend problem, not a model one.

See [AGENTS.md](AGENTS.md) for the architecture, the measurements it rests on,
and house style.

## License

MIT. See [LICENSE](LICENSE).
