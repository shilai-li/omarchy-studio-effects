# Model attribution and source materials

The repository's MIT licence covers its daemon, widget, tools, and packaging.
It does not relicense either third-party model. Keep the relevant model licence
and this attribution with redistributed models, including converted OpenVINO IR.

## MediaPipe Selfie Segmentation — Apache-2.0

MediaPipe Selfie Segmentation is a Google MediaPipe model. The ONNX export is
provided by ONNX Community / Xenova:

- Model card: <https://huggingface.co/onnx-community/mediapipe_selfie_segmentation>
- Verified revision: `be49485c8e027524be38591817fc5cd31bd9d00e`
- Export: <https://huggingface.co/onnx-community/mediapipe_selfie_segmentation/resolve/be49485c8e027524be38591817fc5cd31bd9d00e/onnx/model.onnx>
- Local name: `selfie_segmentation.onnx`
- SHA-256: `3241ac4ad8aa35bdaf33946776db29f7c283a413aa0b0dacb9483594b4531aad`
- Licence copy: [LICENSE-Apache-2.0.txt](LICENSE-Apache-2.0.txt)

The committed ONNX is byte-identical to that export; only the filename differs.
The reviewed ONNX Community distribution contains no NOTICE file. This document
supplies project attribution; it is not an upstream NOTICE. Preserve any
additional upstream notices if the model source is changed in a future release.

**Modification notice:** Omarchy Studio Effects converts the ONNX into
`segmentation.xml` / `segmentation.bin`, pins its input to `[1,3,256,256]`, and
compresses floating-point weights to FP16. The installed build record gives the
conversion date and tool versions. The converted model remains Apache-2.0.

## RobustVideoMatting — GPL-3.0

RobustVideoMatting (RVM) is by Peter Lin and the upstream contributors, developed
at ByteDance. Upstream distributes its code and pretrained models under GPL-3.0.
The full upstream release licence is preserved in
[LICENSE-RVM-GPL-3.0.txt](LICENSE-RVM-GPL-3.0.txt).

- Project: <https://github.com/PeterL1n/RobustVideoMatting>
- Pretrained model release: `v1.0.0`
- ONNX input: <https://github.com/PeterL1n/RobustVideoMatting/releases/download/v1.0.0/rvm_mobilenetv3_fp32.onnx>
- SHA-256: `88d4531297118f595bf2fd60f6f566aec2e559393802d1f436c380f0cbbd2828`
- Original PyTorch weights: <https://github.com/PeterL1n/RobustVideoMatting/releases/download/v1.0.0/rvm_mobilenetv3.pth>
- SHA-256: `3c7c1d92033f7c38d6577c481d13a195d7d80a159b960f4f3119ac7b534cf4f8`
- Release source revision: `17d1774b032fd503bfe53c57d295db719f9e3da1`
- Source archive: <https://github.com/PeterL1n/RobustVideoMatting/archive/17d1774b032fd503bfe53c57d295db719f9e3da1.tar.gz>
- Archive SHA-256: `cb39aed8388bbde802a77a86aa5a8b9af4607538064ec904e077f439affc881a`
- ONNX exporter revision: `ebead27cb683e157b2bea7ca869daa820a07ba8f`
- Exporter archive: <https://github.com/PeterL1n/RobustVideoMatting/archive/ebead27cb683e157b2bea7ca869daa820a07ba8f.tar.gz>
- Archive SHA-256: `ddfb4e85872342a8a32ed4703b5d0bc2ab2a23b9db885384b0e625e1f9c79062`

**Modification notice:** Omarchy Studio Effects converts the release ONNX into
`matting.xml` / `matting.bin`, freezes `downsample_ratio` at `0.5`, pins the image
input to `[1,3,256,256]`, discovers and fixes recurrent-state shapes with a CPU
inference, and compresses floating-point weights to FP16. The installed build
record gives the conversion date and tool versions. The converted model remains
GPL-3.0. We do not change the upstream PyTorch weights or source archives.

### Materials included with the binary package

Every default package includes these files under
`/usr/share/doc/omarchy-studio-effects/model-source/`:

- `rvm_mobilenetv3.onnx`: the exact release ONNX used for conversion.
- `rvm_mobilenetv3.pth`: original editable pretrained weights.
- `rvm-source.tar.gz`: release model, training, and inference source with its
  upstream licence and notices intact.
- `rvm-onnx-source.tar.gz`: upstream ONNX exporter, adapted model code, dependency
  list, and export instructions.
- `convert.py` and `PKGBUILD`: the exact local conversion and packaging scripts.
- `MODEL-SOURCES.md` and `build-info.txt`: attribution, modification notices,
  conversion instructions, conversion date, and OpenVINO/NumPy versions.

The licences, including MIT for our conversion script, are installed under
`/usr/share/licenses/omarchy-studio-effects/`. Retain these source materials and
licences when redistributing the default binary package. An upstream URL alone
is not a replacement for the source materials included here.

### Convert or modify the model

Copy the installed source directory to a writable working directory. With
`python-openvino` installed on Arch, convert the supplied ONNX:

```bash
/usr/bin/python3 convert.py rvm_mobilenetv3.onnx matting.xml
```

This writes `matting.xml` and `matting.bin`; the build record identifies the
OpenVINO and NumPy versions used for the packaged conversion. OpenVINO versions
can produce different IR files.

To edit the network or re-export the pretrained weights, unpack the source:

```bash
tar -xzf rvm-source.tar.gz
tar -xzf rvm-onnx-source.tar.gz
cd RobustVideoMatting-ebead27cb683e157b2bea7ca869daa820a07ba8f
```

Follow that directory's `README.md` and `requirements.txt` for the exporter
environment, including its notes for older PyTorch versions. Its model code is
adapted for ONNX export. An FP32 CPU export uses:

```bash
python export_onnx.py --model-variant mobilenetv3 \
  --checkpoint ../rvm_mobilenetv3.pth --precision float32 \
  --opset 12 --device cpu --output ../modified-rvm.onnx
cd ..
/usr/bin/python3 convert.py modified-rvm.onnx matting.xml
```

For training and inference changes, the release source archive contains the
upstream code and dependency lists. Follow its documentation for external
training datasets. Add notices describing your changes and retain GPL-3.0 when
redistributing modified RVM models or source.
