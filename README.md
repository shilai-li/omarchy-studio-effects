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
omarchy plugin add https://github.com/shilai-li/omarchy-studio-effects.git --enable
```

That is all of it. The daemon is a package, and `omarchy plugin add` builds
nothing, so right after adding the plugin the bar widget knows the daemon is
missing. Open its panel and it says so, with one row: **Set up**. Choosing it
opens a terminal window that builds the daemon from the plugin's own checkout
and installs it -- a few minutes, and it asks for your password there, where you
can see it. The panel changes by itself when the package lands.

The widget never builds anything or runs `sudo` itself; it only opens that
window. To do the same by hand:

```bash
bash ~/.config/omarchy/plugins/shilai_li.studio-effects/packaging/setup.sh
```

Installing the package creates the **Studio Camera** device and starts it at
boot, so there is nothing to enable by hand -- the daemon can only write to a
camera that exists, and making one needs root, so the package does it. Removing
the package removes the device. Then pick **Studio Camera** in Zoom, Meet, or any
browser, and turn effects on from the bar.

If it is missing -- an install inside a container, say -- the daemon says so and
how to make it: `sudo systemctl enable --now studio-effects-loopback`.

Working on it rather than using it? `cd packaging && makepkg -sfi` builds from
the checkout you are in. The `-f` matters: without it makepkg reuses an older
package it built earlier and installs that.

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

(`WIDTH` and `HEIGHT` default to 0, which means the camera's own best up to
1080p, so setting them is how you publish *less* than the camera gives.)

Zooming to 150% is then a straight 1:1 read with no softness at all, and it
costs about 3 ms of CPU a frame with a USB camera -- one to scale the picture,
two more to decode a bigger one -- because blur and compositing still run at
the output size.

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
studio-effects blur 40            # and passes 1-3, dim 0-100, desat 0-100
studio-effects framing on         # and zoom 100-300, how far it may crop in
studio-effects model matting      # `status` lists the installed ones
```

`studio-effects --help` is the full list.

Every command answers with the daemon's full state, so a caller never has to ask
twice, and a refused change is an error in the JSON *and* a non-zero exit.

What you change is remembered. Switching the camera off stops the daemon, so
your choices are written to `~/.local/state/studio-effects/settings.json` and
applied again when it comes back — the switch pauses the camera without undoing
your settings. Delete that file to return to the config's values.

A Hyprland bind:

```
bindd = SUPER CTRL, B, Toggle camera effects, exec, studio-effects toggle
```

## Two models

```conf
MODEL=segmentation   # MediaPipe, 0.8 ms, a hard-edged mask
MODEL=matting        # RobustVideoMatting, 3.4 ms, a true alpha matte
```

Both run on the NPU. Matting gives soft, natural hair edges and holds them still
between frames, because it carries state from one frame to the next rather than
deciding each one afresh. It costs about 4 ms more of a 33 ms budget — which is
what the NPU's spare capacity is for, since it otherwise sits under 1% busy.

The config line only sets which one starts. Switch between them while the camera
is running, from the panel's **Model** row or with:

```bash
studio-effects model matting
```

The swap takes about 15 ms once each model has been compiled, and the frame loop
keeps running on the old one until the new one is ready — so the picture changes
between one frame and the next, with the same face in the same light on either
side of it. That is the only way to actually see the difference. The panel
offers whichever models are installed and hides the row entirely when there is
only one, so a machine with a single model gets no control that does nothing.

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

### After an OpenVINO update, rebuild

If the camera stops working right after a system upgrade and the panel says the
daemon is not answering, this is almost certainly why:

```
$ journalctl --user -u studio-effects -n 5
studio-effects-daemon: error while loading shared libraries:
libopenvino_c.so.2630: cannot open shared object file
```

OpenVINO's soname is its version with the dots removed -- 2026.3.0 is
`libopenvino_c.so.2630`, 2026.3.1 is `.so.2631` -- so *every* release, patch
releases included, breaks a binary linked against the one before it. The daemon
dies in the dynamic loader before `main()`, exits 127, and systemd retries until
it hits the restart limit. Nothing is wrong with the daemon; its library is
gone. Rebuild and reinstall:

```bash
cd packaging
makepkg -fi
systemctl --user reset-failed studio-effects   # clear the restart limit
systemctl --user restart studio-effects
```

`reset-failed` matters: after four crashes systemd refuses to start the unit at
all until that counter is cleared, so without it the reinstall looks like it
did not help.

`pacman` cannot warn about this. The `openvino` package declares no sonames, so
`depends=('openvino')` stays satisfied across the break.

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

The picture is whatever the camera offers, up to 1920x1080, at the fastest rate
it has up to 60 fps -- 1080p at 30 on most laptop cameras. A camera without 1080p
gets its own best mode and is not stretched to fit, and a machine without an
NPU runs the same models on its GPU, or on the CPU, with the same result.

The frame loop's CPU work is 1.1 ms a frame at 720p and about 2 at 1080p (a
P-core, blur at the default radius); with a USB camera the MJPEG decode on the
capture thread costs more than that, 3.5 ms at 720p and about 7 at 1080p. See
AGENTS.md for the per-stage numbers, which were first measured before this got
about five times cheaper.

See [AGENTS.md](AGENTS.md) for the architecture, the measurements it rests on,
and house style.

## Third-party models

The code here is MIT. The models are not mine and carry their own terms.

| model | upstream | licence | shipped how |
|---|---|---|---|
| `segmentation` | [MediaPipe Selfie Segmentation](https://huggingface.co/onnx-community/mediapipe_selfie_segmentation), Google | Apache-2.0 | `models/selfie_segmentation.onnx` is committed here |
| `matting` | [RobustVideoMatting](https://github.com/PeterL1n/RobustVideoMatting), Peter Lin | GPL-3.0 | fetched at build time, not redistributed in this repo |

Both are converted to OpenVINO IR by `tools/convert.py` at package build time;
the `.xml`/`.bin` pairs are build products of whichever model they came from and
inherit its licence.

If you package this for others, note that a package built with the default
`PKGBUILD` contains GPL-3.0 material, which is why its `license` field names
all three. Building with only `segmentation` avoids that — the daemon runs fine
with one model installed, and the panel then hides the Model row.

## License

MIT for the code in this repository. See [LICENSE](LICENSE), and the table
above for the models.
