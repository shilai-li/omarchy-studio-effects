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

/// The devices to try, best first, saying out loud what was found and why.
///
/// A missing NPU is invisible: the driver's udev rule puts /dev/accel/* in group
/// `render`, and a user outside that group gets an `available_devices` list with
/// no NPU in it rather than a permission error. Falling back silently would
/// leave someone wondering why their battery is worse than promised, so the
/// reason is said out loud.
///
/// A list rather than one choice, because being listed is not being usable: an
/// NPU whose driver is too old for the model is present and refuses to compile
/// it, and the machine that has one is exactly the machine with a GPU behind it.
/// `new` walks the list. A device asked for by name is the whole list -- asking
/// for one and quietly getting another would make a benchmark a lie.
fn candidates(available: &[String], requested: Option<&str>) -> Result<Vec<String>> {
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
        return Ok(vec![want.to_string()]);
    }

    let found: Vec<String> = PREFERENCE
        .iter()
        .filter(|p| available.iter().any(|a| a == *p))
        .map(|p| p.to_string())
        .collect();
    anyhow::ensure!(!found.is_empty(), "no OpenVINO device at all, not even CPU");

    if found[0] != "NPU" {
        eprintln!(
            "no NPU available, falling back to {}. If this machine has one, check \
             that you are in the 'render' group and have logged back in since joining.",
            found[0]
        );
    }
    Ok(found)
}

impl Segmenter {
    pub fn new(model_xml: &str, cache_dir: &str, requested: Option<&str>) -> Result<Self> {
        let core = Core::new().context("initialising OpenVINO")?;
        let available: Vec<String> = core
            .available_devices()
            .context("listing OpenVINO devices")?
            .iter()
            .map(|d| d.as_ref().to_string())
            .collect();
        let tries = candidates(&available, requested)?;
        drop(core);

        let mut failures = Vec::new();
        for (i, device) in tries.iter().enumerate() {
            // A device that took the process down while compiling last time is
            // skipped this once, so a driver that aborts cannot put a service
            // into a crash loop. Never the last candidate, and never a device
            // asked for by name: with nothing left to fall back to, trying it
            // is all there is.
            if i + 1 < tries.len() && crashed_last_time(device) {
                let why = format!("{device} crashed while compiling last time, skipped this once");
                eprintln!("{why}");
                failures.push(why);
                continue;
            }
            // A fresh Core for each: a plugin that failed half-way through a
            // compile is not a state worth building the next attempt on.
            match Self::on_device(model_xml, cache_dir, device) {
                Ok(seg) => {
                    if !failures.is_empty() {
                        eprintln!("segmenting on {device} instead: {}", failures.join("; "));
                    }
                    return Ok(seg);
                }
                Err(e) => {
                    let why = format!("{device} could not run this model ({})", first_line(&e));
                    eprintln!("{why}");
                    failures.push(why);
                }
            }
        }
        anyhow::bail!("no device could run {model_xml}: {}", failures.join("; "))
    }

    fn on_device(model_xml: &str, cache_dir: &str, device: &str) -> Result<Self> {
        let device = device.to_string();
        let mut core = Core::new().context("initialising OpenVINO")?;
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

        // A compiler that aborts -- the NPU's did, on a model with a dynamic
        // shape, with "LLVM ERROR: Failed to infer result type(s)" -- takes the
        // process with it, and no error handling in here can catch that. So
        // leave a note first and take it away after: one still there at the
        // next start means this device did not come back.
        let crumb = breadcrumb(&device);
        if let Some(path) = &crumb {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(path, b"");
        }
        let compiled = core.compile_model(&model, device_type);
        // Gone whichever way it went: an `Err` is a refusal the caller handles,
        // not a crash.
        if let Some(path) = &crumb {
            let _ = std::fs::remove_file(path);
        }
        let mut compiled = compiled.with_context(|| format!("compiling the model for {device}"))?;
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

/// Where a device's "I am compiling" note lives: beside the saved settings.
fn breadcrumb(device: &str) -> Option<std::path::PathBuf> {
    Some(crate::state::path().parent()?.join(format!("compiling-{device}")))
}

/// Whether `device` was compiling when the last process died. Reading the note
/// clears it, so the skip lasts one start: the next tries the device again, and
/// a driver that has been fixed since is not written off for good.
fn crashed_last_time(device: &str) -> bool {
    breadcrumb(device).is_some_and(|path| take_note(&path))
}

/// Whether the note was there, removing it. Removing is what makes it once.
fn take_note(path: &std::path::Path) -> bool {
    path.exists() && std::fs::remove_file(path).is_ok()
}

/// The part of an error worth putting in a one-line message.
fn first_line(e: &anyhow::Error) -> String {
    format!("{e:#}").lines().next().unwrap_or("").chars().take(120).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn devices(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// The order is the whole feature: NPU, then GPU, then CPU, whatever order
    /// OpenVINO happens to list them in.
    #[test]
    fn best_device_comes_first_whatever_order_they_are_listed() {
        let got = candidates(&devices(&["CPU", "GPU", "NPU"]), None).unwrap();
        assert_eq!(got, ["NPU", "GPU", "CPU"]);
    }

    /// No NPU is a quieter machine, not a broken one -- and no GPU either is
    /// still a working camera.
    #[test]
    fn a_missing_device_is_skipped_not_fatal() {
        assert_eq!(candidates(&devices(&["CPU", "GPU"]), None).unwrap(), ["GPU", "CPU"]);
        assert_eq!(candidates(&devices(&["CPU"]), None).unwrap(), ["CPU"]);
        assert_eq!(candidates(&devices(&["NPU", "CPU"]), None).unwrap(), ["NPU", "CPU"]);
    }

    /// Devices OpenVINO lists that this daemon has no business using -- a
    /// second GPU is `GPU.1`, and HETERO or AUTO are not devices -- are ignored.
    #[test]
    fn devices_outside_the_chain_are_ignored() {
        assert_eq!(candidates(&devices(&["GPU.0", "GPU.1", "CPU"]), None).unwrap(), ["CPU"]);
    }

    /// Asking for a device by name is asking for it: falling through to another
    /// would make `--device NPU` benchmark a CPU.
    #[test]
    fn a_named_device_is_the_only_candidate() {
        assert_eq!(candidates(&devices(&["NPU", "GPU", "CPU"]), Some("GPU")).unwrap(), ["GPU"]);
    }

    #[test]
    fn a_named_device_that_is_absent_says_what_is_there() {
        let e = candidates(&devices(&["CPU"]), Some("NPU")).unwrap_err().to_string();
        assert!(e.contains("only [CPU]"), "{e}");
        assert!(e.contains("render"), "the NPU hint should explain the usual cause: {e}");
    }

    /// A note left by a crash is honoured once and then gone, so the device is
    /// tried again on the start after -- never written off for good.
    #[test]
    fn a_crash_note_is_read_once() {
        let dir = std::env::temp_dir().join(format!("segmenter-note-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let note = dir.join("compiling-NPU");
        assert!(!take_note(&note), "no note, no crash");
        std::fs::write(&note, b"").unwrap();
        assert!(take_note(&note), "the crash is noticed");
        assert!(!take_note(&note), "and only once");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A model that loads nowhere must fail cleanly, having named every device
    /// it tried -- not exit on the first, and not hang on the last.
    #[test]
    fn a_model_no_device_can_load_is_an_error_naming_each() {
        let e = match Segmenter::new("/nonexistent/model.xml", "/tmp/segmenter-test-cache", None) {
            Ok(_) => panic!("loaded a model that does not exist"),
            Err(e) => format!("{e:#}"),
        };
        assert!(e.contains("no device could run"), "{e}");
        assert!(e.contains("CPU could not run"), "CPU is always there and always tried: {e}");
    }

    #[test]
    fn nothing_at_all_is_an_error() {
        assert!(candidates(&[], None).is_err());
    }
}
