//! Model loading, device selection and one inference per frame.

use anyhow::{Context, Result};
use openvino::{Core, DeviceType, ElementType, InferRequest, RwPropertyKey, Shape, Tensor};

use crate::nv12::NET;

/// Devices in the order we want them, best first.
const PREFERENCE: [&str; 3] = ["NPU", "GPU", "CPU"];

/// The recurrent tensors a matting model carries between frames.
///
/// RobustVideoMatting is not a per-frame classifier: it remembers what it saw,
/// which is what makes its edges hold still instead of shimmering. That memory
/// is four tensors handed back in on the next frame, and dropping them would
/// leave a model that is merely slower than a per-frame one.
struct Recurrent {
    input: String,
    output: String,
    tensor: Tensor,
}

pub struct Segmenter {
    request: InferRequest,
    input: Tensor,
    /// Which output carries the mask. Named, because a matting model also
    /// returns a foreground image and its own next states.
    mask_output: String,
    states: Vec<Recurrent>,
    mask: Vec<f32>,
    pub device: String,
    pub model: String,
}

/// Pick the best device present, saying out loud what was chosen and why.
///
/// A missing NPU is invisible: the driver's udev rule puts /dev/accel/* in group
/// `render`, and a user outside that group gets an `available_devices` list with
/// no NPU in it rather than a permission error. Falling back silently would
/// leave someone wondering why their battery is worse than promised, so the
/// reason is said out loud.
fn choose(core: &Core, requested: Option<&str>) -> Result<String> {
    let available: Vec<String> = core
        .available_devices()
        .context("listing OpenVINO devices")?
        .iter()
        .map(|d| d.as_ref().to_string())
        .collect();

    if let Some(want) = requested {
        if !available.iter().any(|a| a == want) {
            let hint = if want == "NPU" {
                "\nAn NPU missing from that list is usually permissions rather than \
                 hardware: check /dev/accel/accel0 exists, that you are in the 'render' \
                 group, and that you have logged out and back in since joining it."
            } else {
                ""
            };
            anyhow::bail!(
                "device {want} was requested but OpenVINO reports only [{}]{hint}",
                available.join(", ")
            );
        }
        return Ok(want.to_string());
    }

    let chosen = PREFERENCE
        .iter()
        .find(|p| available.iter().any(|a| a == *p))
        .context("no OpenVINO device at all, not even CPU")?
        .to_string();

    if chosen != "NPU" {
        eprintln!(
            "no NPU available, falling back to {chosen}. If this machine has one, check \
             that you are in the 'render' group and have logged back in since joining."
        );
    }
    Ok(chosen)
}

impl Segmenter {
    pub fn new(model_xml: &str, cache_dir: &str, requested: Option<&str>) -> Result<Self> {
        let mut core = Core::new().context("initialising OpenVINO")?;
        let device = choose(&core, requested)?;
        let device_type: DeviceType = device.parse().expect("DeviceType parsing is infallible");

        // Without this an NPU compile costs 331 ms on every start; with it, 13.
        core.set_property(&device_type, &RwPropertyKey::CacheDir, cache_dir)
            .context("setting the model cache directory")?;

        let weights = model_xml.strip_suffix(".xml").unwrap_or(model_xml).to_owned() + ".bin";
        let model = core
            .read_model_from_file(model_xml, &weights)
            .with_context(|| format!("reading model {model_xml}"))?;

        // A matting model announces itself by its inputs: an image plus four
        // recurrent states, rather than an image alone.
        let inputs: Vec<String> = (0..model.get_inputs_len()?)
            .filter_map(|i| model.get_input_by_index(i).ok())
            .filter_map(|n| n.get_name().ok())
            .collect();
        let recurrent = inputs.iter().any(|n| n == "r1i");

        let mut compiled = core
            .compile_model(&model, device.parse().unwrap())
            .with_context(|| format!("compiling the model for {device}"))?;
        let mut request = compiled.create_infer_request()?;

        // Allocated once and refilled per frame; the model's shape is static
        // precisely so this never has to be rebuilt.
        let input = Tensor::new(ElementType::F32, &Shape::new(&[1, 3, NET as i64, NET as i64])?)?;
        let image_input = if recurrent { "src" } else { inputs.first().map_or("", |s| s.as_str()) };
        request.set_tensor(image_input, &input)?;

        // The states start at zero, which is what "no previous frame" means to
        // the model, and are then carried forward for the life of the daemon.
        //
        // Zeroing them is not a formality. `Tensor::new` hands back
        // uninitialised memory, and whatever happens to be in it goes straight
        // into a recurrence: one NaN in the first frame's state is fed back as
        // the next frame's input forever, and the model returns NaN for the
        // rest of the daemon's life. It looks exactly like a mask of zero --
        // every frame blurred, subject included -- with nothing logged.
        let mut states = Vec::new();
        if recurrent {
            for (name, shape) in [
                ("r1", [1, 16, 64, 64]),
                ("r2", [1, 20, 32, 32]),
                ("r3", [1, 40, 16, 16]),
                ("r4", [1, 64, 8, 8]),
            ] {
                let dims: Vec<i64> = shape.iter().map(|&d| d as i64).collect();
                let mut tensor = Tensor::new(ElementType::F32, &Shape::new(&dims)?)?;
                tensor.get_data_mut::<f32>()?.fill(0.0);
                let input = format!("{name}i");
                request.set_tensor(&input, &tensor)?;
                states.push(Recurrent {
                    input,
                    output: format!("{name}o"),
                    tensor,
                });
            }
        }

        Ok(Self {
            request,
            input,
            mask_output: if recurrent { "pha".into() } else { String::new() },
            states,
            mask: vec![0.0; NET * NET],
            device,
            model: if recurrent { "matting".into() } else { "segmentation".into() },
        })
    }

    /// Borrow the input tensor's buffer to write the next frame into.
    pub fn input_buffer(&mut self) -> Result<&mut [f32]> {
        Ok(self.input.get_data_mut::<f32>()?)
    }

    /// Run one inference and hand back the 256x256 foreground mask.
    ///
    /// The result is copied out rather than borrowed from the request: the
    /// output tensor is owned by the request, and handing out a slice into it
    /// would either fight the borrow checker for the rest of the frame or need
    /// an `unsafe` that outlives the next infer(). A 256 KB memcpy costs far
    /// less than the composite that follows it.
    pub fn infer(&mut self) -> Result<&[f32]> {
        self.request.infer()?;

        let out = if self.mask_output.is_empty() {
            self.request.get_output_tensor()?
        } else {
            self.request.get_tensor(&self.mask_output)?
        };
        self.mask.copy_from_slice(out.get_data::<f32>()?);

        // Carry the model's memory into the next frame. Copied rather than
        // swapped because the request holds these tensors: handing it a
        // different one each frame would mean re-binding every input.
        for state in &mut self.states {
            let produced = self.request.get_tensor(&state.output)?;
            let source: &[f32] = produced.get_data::<f32>()?;
            state.tensor.get_data_mut::<f32>()?.copy_from_slice(source);
            self.request.set_tensor(&state.input, &state.tensor)?;
        }
        Ok(&self.mask)
    }
}
