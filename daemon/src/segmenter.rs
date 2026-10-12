//! Model loading, device selection and one inference per frame.

use anyhow::{Context, Result};
use openvino::{Core, DeviceType, ElementType, InferRequest, Model, RwPropertyKey, Shape, Tensor};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

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
    /// How many threads the CPU plugin was held to. `None` on any other device,
    /// where the question does not arise.
    pub threads: Option<usize>,
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
        let spec = Spec { recurrent, inputs, device };

        // The CPU plugin left alone spreads one 256x256 inference across every
        // thread and spins them between frames. Measured on an i5-8250U
        // holding 30 fps, segmentation cost 10.7 ms of CPU a frame that way and
        // 4.2 on one thread, with latency still a fifth of the frame; matting
        // cost 37 ms -- 111% of a core -- and 27 on two threads, with room to
        // spare. So the CPU is held to as few threads as keep up.
        if spec.device == "CPU" {
            let threads = match cached_threads(model_xml) {
                Some(n) => n,
                None => {
                    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
                    let n = pick_threads(&cpu_candidates(cores), CPU_BUDGET_MS, |n| {
                        core.set_property(&device_type, &RwPropertyKey::InferenceNumThreads, &n.to_string())
                            .context("setting the CPU thread count")?;
                        Self::assemble(&mut core, &model, &spec, Some(n))?.probe()
                    })?;
                    remember_threads(model_xml, n);
                    n
                }
            };
            core.set_property(&device_type, &RwPropertyKey::InferenceNumThreads, &threads.to_string())
                .context("setting the CPU thread count")?;
            return Self::assemble(&mut core, &model, &spec, Some(threads));
        }
        Self::assemble(&mut core, &model, &spec, None)
    }

    /// Compile for the device and build everything an inference needs.
    fn assemble(core: &mut Core, model: &Model, spec: &Spec, threads: Option<usize>) -> Result<Self> {
        let Spec { recurrent, inputs, device } = spec;
        let (recurrent, device) = (*recurrent, device.clone());
        let device_type: DeviceType = device.parse().expect("DeviceType parsing is infallible");

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
        let compiled = core.compile_model(model, device_type);
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
            threads,
        })
    }

    /// How long one inference takes, in milliseconds, on an empty frame.
    ///
    /// The median of a handful after two to warm up: the first calls carry
    /// allocation, and one slow outlier on a busy machine should not decide the
    /// thread count. Content does not change the cost of a convolution, so a
    /// black frame is as good as a face.
    fn probe(mut self) -> Result<f64> {
        self.input_buffer()?.fill(0.0);
        for _ in 0..2 {
            self.infer()?;
        }
        let mut ms = Vec::with_capacity(PROBE_FRAMES);
        for _ in 0..PROBE_FRAMES {
            let t = Instant::now();
            self.infer()?;
            ms.push(t.elapsed().as_secs_f64() * 1e3);
        }
        ms.sort_by(|a, b| a.total_cmp(b));
        Ok(ms[ms.len() / 2])
    }

    /// The device, and the thread count where one was chosen: "CPU (2 threads)".
    pub fn describe(&self) -> String {
        match self.threads {
            Some(n) => format!("{} ({n} thread{})", self.device, if n == 1 { "" } else { "s" }),
            None => self.device.clone(),
        }
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

/// What a model needs to be built on a device, gathered once.
struct Spec {
    recurrent: bool,
    inputs: Vec<String>,
    device: String,
}

/// The longest one inference may take on the CPU, in milliseconds: 60% of a
/// 30 fps frame. The rest of the frame is the camera's decode, the blur and the
/// blend, and a model that uses more than this leaves them no room.
const CPU_BUDGET_MS: f64 = 20.0;

/// Inferences timed per thread count.
const PROBE_FRAMES: usize = 6;

/// Thread counts worth trying, fewest first, on a machine with `cores`.
fn cpu_candidates(cores: usize) -> Vec<usize> {
    let mut found: Vec<usize> = [1, 2, 4].into_iter().filter(|&n| n <= cores).collect();
    // A machine with more than four threads gets the rest as a last resort.
    if cores > 4 {
        found.push(cores);
    }
    found
}

/// The fewest threads whose latency fits the budget; failing that, the thread
/// count that was fastest -- a CPU too slow for the budget still has to run.
///
/// Fewest first because that is the cheap one: each thread added buys less
/// latency than the last and costs a whole thread's spin-waiting, so the first
/// count that keeps up is the one that costs least. `latency` is called in
/// order and stops being called at the first that fits.
fn pick_threads(candidates: &[usize], budget_ms: f64, mut latency: impl FnMut(usize) -> Result<f64>) -> Result<usize> {
    let mut best: Option<(usize, f64)> = None;
    for &n in candidates {
        let ms = latency(n)?;
        if ms <= budget_ms {
            return Ok(n);
        }
        if best.is_none_or(|(_, b)| ms < b) {
            best = Some((n, ms));
        }
    }
    best.map(|(n, _)| n).context("no thread count to try")
}

/// What was chosen for each model this run. Switching models is something a
/// person does a few times while comparing them, and measuring again each time
/// would stall the frame loop for a quarter of a second to learn the same thing.
fn chosen() -> &'static Mutex<HashMap<String, usize>> {
    static CHOSEN: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
    CHOSEN.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_threads(model_xml: &str) -> Option<usize> {
    chosen().lock().ok()?.get(model_xml).copied()
}

fn remember_threads(model_xml: &str, threads: usize) {
    if let Ok(mut map) = chosen().lock() {
        map.insert(model_xml.to_string(), threads);
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

    /// The cheap count is the smallest one that keeps up, and nothing larger is
    /// even measured once one does.
    #[test]
    fn the_fewest_threads_that_keep_up_win() {
        let mut asked = Vec::new();
        let n = pick_threads(&[1, 2, 4], 20.0, |n| {
            asked.push(n);
            Ok([4.2, 2.9, 2.2][n.trailing_zeros() as usize])
        })
        .unwrap();
        assert_eq!(n, 1, "segmentation on an i5: one thread is already fast enough");
        assert_eq!(asked, [1], "and the others are not even tried");
    }

    /// Matting on the same i5: one thread misses the budget at 42 ms, two make it.
    #[test]
    fn a_slower_model_gets_more_threads_only_as_needed() {
        let n = pick_threads(&[1, 2, 4], 20.0, |n| Ok(match n { 1 => 42.0, 2 => 17.7, _ => 15.8 })).unwrap();
        assert_eq!(n, 2);
    }

    /// A CPU too slow for the budget still has to run, on whatever was fastest.
    #[test]
    fn when_nothing_fits_the_fastest_is_used() {
        let n = pick_threads(&[1, 2, 4], 20.0, |n| Ok(match n { 1 => 90.0, 2 => 50.0, _ => 41.0 })).unwrap();
        assert_eq!(n, 4);
        // Fewer threads can be faster than more, once the spinning costs more
        // than the parallelism buys.
        let n = pick_threads(&[1, 2, 4], 20.0, |n| Ok(match n { 1 => 30.0, 2 => 25.0, _ => 28.0 })).unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn a_failed_measurement_is_an_error_not_a_guess() {
        assert!(pick_threads(&[1, 2], 20.0, |_| anyhow::bail!("compile failed")).is_err());
        assert!(pick_threads(&[], 20.0, |_| Ok(1.0)).is_err());
    }

    #[test]
    fn candidates_never_exceed_the_machine() {
        assert_eq!(cpu_candidates(1), [1]);
        assert_eq!(cpu_candidates(2), [1, 2]);
        assert_eq!(cpu_candidates(4), [1, 2, 4]);
        assert_eq!(cpu_candidates(8), [1, 2, 4, 8], "a bigger machine gets everything as a last resort");
        assert_eq!(cpu_candidates(16), [1, 2, 4, 16]);
    }

    #[test]
    fn nothing_at_all_is_an_error() {
        assert!(candidates(&[], None).is_err());
    }
}
