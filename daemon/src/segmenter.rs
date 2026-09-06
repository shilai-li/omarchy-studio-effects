//! Model loading, device selection and one inference per frame.

use anyhow::{Context, Result};
use openvino::{Core, DeviceType, ElementType, InferRequest, RwPropertyKey, Shape, Tensor};

use crate::nv12::NET;

/// Devices in the order we want them, best first.
const PREFERENCE: [&str; 3] = ["NPU", "GPU", "CPU"];

pub struct Segmenter {
    request: InferRequest,
    input: Tensor,
    mask: Vec<f32>,
    pub device: String,
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

        let mut compiled = core
            .compile_model(&model, device.parse().unwrap())
            .with_context(|| format!("compiling the model for {device}"))?;
        let request = compiled.create_infer_request()?;

        // Allocated once and refilled per frame; the model's shape is static
        // precisely so this never has to be rebuilt.
        let input = Tensor::new(ElementType::F32, &Shape::new(&[1, 3, NET as i64, NET as i64])?)?;

        Ok(Self {
            request,
            input,
            mask: vec![0.0; NET * NET],
            device,
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
        self.request.set_input_tensor(&self.input)?;
        self.request.infer()?;
        let out = self.request.get_output_tensor()?;
        self.mask.copy_from_slice(out.get_data::<f32>()?);
        Ok(&self.mask)
    }
}
