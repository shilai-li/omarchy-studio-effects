# Omarchy Studio Effects

Camera background blur and replacement for [Omarchy](https://omarchy.org), with
the segmentation run on your laptop's NPU.

> **Early development.** The daemon works and is packaged: it segments a live
> camera and publishes the result as a second camera your apps can select, at
> 1080p30. The bar widget is not written yet.

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
systemctl --user enable --now studio-effects          # runs the daemon
```

Then pick **Studio Camera** in Zoom, Meet, or any browser.

To point it at a different camera, or change the resolution or blur:

```bash
cp /usr/share/studio-effects/studio-effects.conf.example ~/.config/studio-effects.conf
$EDITOR ~/.config/studio-effects.conf
systemctl --user restart studio-effects
```

`studio-effects-daemon --list-devices` prints every camera with its card label.
Prefer labels over `/dev/videoN` in that file: numbers move between boots.

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
