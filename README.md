# Omarchy Studio Effects

Camera background blur and replacement for [Omarchy](https://omarchy.org), with
the segmentation run on your laptop's NPU.

> **Early development.** The model pipeline is converted and benchmarked; the
> daemon and the bar widget are not written yet. Nothing here is installable.

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

## Requirements

An Intel Core Ultra with an NPU, though it falls back to the GPU and then the
CPU. On Arch/Omarchy:

```bash
sudo pacman -S openvino openvino-intel-npu-plugin openvino-intel-gpu-plugin \
               python-openvino intel-npu-driver intel-npu-compiler
sudo usermod -aG render $USER   # then log out and back in
```

That last line is not optional. `intel-npu-driver` ships a udev rule putting
`/dev/accel/*` in group `render`, and without it OpenVINO reports no NPU at all
rather than an error.

## Development

```bash
python3 tools/convert.py   # fetch-free: ONNX -> static FP16 OpenVINO IR
python3 tools/bench.py     # per-device timing
```

See [AGENTS.md](AGENTS.md) for the architecture, the measurements it rests on,
and house style.

## License

MIT. See [LICENSE](LICENSE).
