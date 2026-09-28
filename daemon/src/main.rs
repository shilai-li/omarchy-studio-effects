//! studio-effects-daemon -- read a camera, blur the background, publish the
//! result as a second camera.
//!
//! See AGENTS.md for the measurements the design follows. The short version:
//! inference is under a millisecond and colour conversion is not, so the frame
//! never leaves NV12 and only the 256x256 the model needs is ever converted.


use anyhow::{Context, Result};
use std::sync::{Arc, Mutex};
use studio_effects_daemon::control::{self, Effect, Fixed, Settings};
use studio_effects_daemon::{background, device, framing, mask, nv12, preview, segmenter, state};
use clap::Parser;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use std::time::Instant;

#[derive(Parser)]
#[command(about = "NPU-accelerated camera background effects", version)]
struct Args {
    /// Camera to read: a /dev/video path, or a card label to look up.
    #[arg(short, long, default_value = "/dev/video0")]
    input: String,

    /// v4l2loopback to publish to: a /dev/video path, or a card label to look
    /// up. Omitted, frames are processed and dropped -- which is how the
    /// pipeline gets measured without a loopback.
    #[arg(short, long)]
    output: Option<String>,

    /// List every v4l2 device with its card label, then exit.
    #[arg(long)]
    list_devices: bool,

    #[arg(long, default_value_t = 1280)]
    width: u32,

    #[arg(long, default_value_t = 720)]
    height: u32,

    #[arg(long, default_value_t = 30)]
    fps: u32,

    /// Capture from the camera at this size and publish at --width/--height.
    ///
    /// Only useful with framing. The crop is taken from the captured frame and
    /// scaled to the output, so capturing larger gives the crop real pixels to
    /// use: from 1920x1080 down to a 1280x720 output, zooming to 150% costs no
    /// sharpness at all, where capturing at the output size has to upscale.
    ///
    /// Zero follows --width/--height, which is what you want without framing --
    /// capturing larger then costs a rescale of every frame for nothing.
    #[arg(long, default_value_t = 0)]
    capture_width: u32,

    #[arg(long, default_value_t = 0)]
    capture_height: u32,

    /// Force a device instead of taking the best available (NPU, GPU, CPU).
    #[arg(long, value_parser = parse_device)]
    device: Option<String>,

    /// What to do with the background.
    #[arg(long, value_enum, default_value_t = Effect::Blur)]
    effect: Effect,

    /// Image to stand behind you when --effect replace.
    #[arg(long)]
    background: Option<String>,

    /// Background blur radius in pixels, at the frame's own scale.
    #[arg(long, default_value_t = 12)]
    blur: usize,

    /// Repeats of the box blur, 1 to 3. One is cheapest and looks boxy against
    /// a hard edge; two is close enough to a Gaussian for a background.
    #[arg(long, default_value_t = 2)]
    passes: usize,

    /// Darken the background, 0 to 100. Makes the subject stand out without
    /// touching them.
    #[arg(long, default_value_t = 0)]
    dim: u32,

    /// Drain colour from the background, 0 to 100.
    #[arg(long, default_value_t = 0)]
    desat: u32,

    /// Track the subject and keep them centred: on or off.
    ///
    /// Takes a value rather than being a bare flag, because the systemd unit
    /// has no way to omit an argument and must pass `--framing=${FRAMING}`
    /// whatever the setting is. An empty value counts as off, for the same
    /// reason `--background` does.
    ///
    /// `action = Set` is required: clap infers a flag for any `bool` field and
    /// a `value_parser` alone does not change that, so without it the argument
    /// still refuses to take a value -- and the failure is at runtime, in the
    /// service, not at compile time.
    #[arg(long, default_value = "off", value_parser = parse_switch,
          action = clap::ArgAction::Set)]
    framing: bool,

    /// Furthest the framing may crop in, as a percentage: 200 is 2x. The crop
    /// is scaled back to the output size, so past this the picture goes soft --
    /// but someone sitting far from the camera needs the room, which is why the
    /// default is not tighter.
    #[arg(long, default_value_t = 200)]
    framing_zoom: u32,

    /// How far the subject may drift before the camera moves at all, as a
    /// fraction of the crop. Zero makes the frame chase every twitch.
    #[arg(long, default_value_t = 0.06)]
    framing_dead_zone: f32,

    /// How much of the current crop to keep each frame, 0 to 0.99. Higher is
    /// slower and steadier.
    #[arg(long, default_value_t = 0.92)]
    framing_smoothing: f32,

    /// How hard to push the model's probabilities toward solid foreground or
    /// solid background. 1 uses them as-is, which is what made moving limbs
    /// look transparent.
    #[arg(long, default_value_t = 3.0)]
    mask_gain: f32,

    /// Weight kept from the previous frame's mask, 0 to 0.95. Steadies edges
    /// while you hold still; too much smears the silhouette when you move.
    #[arg(long, default_value_t = 0.5)]
    mask_smoothing: f32,

    /// Which model to segment with: `segmentation`, `matting`, or a path.
    ///
    /// segmentation  MediaPipe selfie segmentation. 0.8 ms, a hard-edged mask.
    /// matting       RobustVideoMatting. 3.4 ms, a true alpha matte with
    ///               recurrent state, so edges hold still between frames.
    #[arg(long, default_value = "segmentation")]
    model: String,

    #[arg(long, default_value = "/tmp/studio-effects-cache")]
    cache: String,

    /// Print a timing line every N frames. 0 turns timing off, which is what a
    /// service wants -- otherwise it writes a line a second to the journal
    /// forever.
    #[arg(long, default_value_t = 60)]
    stats_every: u64,

    /// Write one processed frame here as a PNG, then exit. The only way to see
    /// what the composite actually looks like without a loopback device.
    #[arg(long)]
    snapshot: Option<String>,
}

/// Accepts the words a config file and a socket command already use, so the
/// same setting is not spelled three different ways depending on where it is
/// written.
/// A bare name means one of the models we ship; anything with a separator is a
/// path. Installed models win over the checkout's, so a running service is not
/// quietly using whatever happens to be in a working tree.
fn resolve_model(name: &str) -> String {
    if name.contains('/') || name.ends_with(".xml") {
        return name.to_string();
    }
    for dir in ["/usr/share/studio-effects/models", "models"] {
        let candidate = format!("{dir}/{name}.xml");
        if std::path::Path::new(&candidate).exists() {
            return candidate;
        }
    }
    format!("models/{name}.xml")
}

/// Every model that could be loaded, by bare name, nearest first.
///
/// Discovered rather than listed, so installing a third model makes it appear
/// in the panel without the widget or the daemon knowing its name.
fn installed_models() -> Vec<String> {
    // The first directory holding any model wins outright, rather than the two
    // being merged. resolve_model already prefers an installed model over a
    // checkout's, and a union would additionally offer whatever a working tree
    // happens to contain -- half-converted files, or the same model under both
    // its upstream name and ours.
    for dir in ["/usr/share/studio-effects/models", "models"] {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut found: Vec<String> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "xml"))
            .filter_map(|p| p.file_stem()?.to_str().map(str::to_string))
            .collect();
        if !found.is_empty() {
            found.sort();
            return found;
        }
    }
    Vec::new()
}

fn parse_switch(s: &str) -> Result<bool, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Ok(true),
        // Empty is off: the unit passes the argument unconditionally, so an
        // unset environment variable arrives as an empty string.
        "off" | "false" | "no" | "0" | "" => Ok(false),
        other => Err(format!("expected on or off, got {other:?}")),
    }
}

fn parse_device(s: &str) -> Result<String, String> {
    match s.to_ascii_uppercase().as_str() {
        d @ ("NPU" | "GPU" | "CPU") => Ok(d.to_string()),
        other => Err(format!("unknown device {other}, expected NPU, GPU or CPU")),
    }
}

/// Per-stage timing, because a frame budget is the only thing that matters here.
#[derive(Default)]
struct Timings {
    frames: u64,
    prep: f64,
    infer: f64,
    blur: f64,
    blend: f64,
    frame: f64,
}

impl Timings {
    fn report(&mut self, device: &str) {
        let n = self.frames as f64;
        let total = self.prep + self.infer + self.blur + self.blend + self.frame;
        println!(
            "{device:>3}  {:5.2} ms/frame  (prep {:4.2}  infer {:4.2}  blur {:4.2}  blend {:4.2}  frame {:4.2})  \
             {:5.1}% of a 33 ms budget",
            total / n,
            self.prep / n,
            self.infer / n,
            self.blur / n,
            self.blend / n,
            self.frame / n,
            100.0 * (total / n) / 33.3,
        );
        *self = Self::default();
    }
}

/// Encode one NV12 buffer to a PNG through a throwaway pipeline.
fn write_png(buffer: &gst::Buffer, info: &gst_video::VideoInfo, path: &str) -> Result<()> {
    let desc = format!("appsrc name=src ! videoconvert ! pngenc ! filesink location={path}");
    let pipeline = gst::parse::launch(&desc)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("snapshot pipeline was not a pipeline"))?;
    let src = pipeline
        .by_name("src")
        .context("no appsrc")?
        .downcast::<gst_app::AppSrc>()
        .map_err(|_| anyhow::anyhow!("src was not an appsrc"))?;
    src.set_caps(Some(&info.to_caps()?));

    pipeline.set_state(gst::State::Playing)?;
    src.push_buffer(buffer.copy())
        .map_err(|e| anyhow::anyhow!("pushing the snapshot frame: {e:?}"))?;
    src.end_of_stream()
        .map_err(|e| anyhow::anyhow!("ending the snapshot stream: {e:?}"))?;

    // Wait for the encoder to actually finish writing before returning.
    if let Some(bus) = pipeline.bus() {
        for msg in bus.iter_timed(gst::ClockTime::from_seconds(5)) {
            match msg.view() {
                gst::MessageView::Eos(_) => break,
                gst::MessageView::Error(e) => anyhow::bail!("snapshot failed: {}", e.error()),
                _ => {}
            }
        }
    }
    pipeline.set_state(gst::State::Null)?;
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();

    if args.list_devices {
        for (dev, label) in device::list()? {
            println!("{dev:<16} {label:?}");
        }
        return Ok(());
    }

    gst::init().context("initialising GStreamer")?;

    let input = device::resolve(&args.input)?;
    let output = args.output.as_deref().map(device::resolve).transpose()?;

    let models = installed_models();
    let mut loaded = args.model.trim_end_matches(".xml").rsplit('/').next().unwrap_or("segmentation").to_string();
    let mut seg = segmenter::Segmenter::new(&resolve_model(&loaded), &args.cache, args.device.as_deref())?;
    println!("segmenting on {} with the {} model", seg.device, seg.model);

    // decodebin because a USB camera hands over MJPEG while a loopback hands
    // over raw NV12, and the daemon should not care which.
    // Zero means "same as the output", which is the ordinary case.
    let cap_w = if args.capture_width == 0 { args.width } else { args.capture_width };
    let cap_h = if args.capture_height == 0 { args.height } else { args.capture_height };

    // What the camera is asked for, ahead of decodebin. decodebin accepts
    // anything, so without this v4l2src never learns what size is wanted
    // downstream and opens the camera's largest mode: the 720p default was
    // decoding 1080p MJPEG and scaling it down on every frame, 6.35 ms of CPU
    // where asking for 720p costs 3.52 -- on the capture thread, where the
    // timing line never sees it.
    //
    // Raw first, because raw needs no decode; then MJPEG at the same size;
    // then anything, so a camera without this size still opens and videoscale
    // makes up the difference, as it always did.
    let wanted = format!(
        "video/x-raw,width={cap_w},height={cap_h},framerate={fps}/1;\
         image/jpeg,width={cap_w},height={cap_h},framerate={fps}/1;image/jpeg;video/x-raw",
        fps = args.fps
    );
    let src = format!(
        "v4l2src name=camera device={} ! {wanted} ! decodebin ! videoconvert ! videoscale \
         ! video/x-raw,format=NV12,width={},height={},framerate={}/1 \
         ! appsink name=sink max-buffers=2 drop=true sync=false",
        input, cap_w, cap_h, args.fps
    );
    let pipeline = gst::parse::launch(&src)
        .context("building the capture pipeline")?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("capture pipeline was not a pipeline"))?;
    let sink = pipeline
        .by_name("sink")
        .context("no appsink")?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow::anyhow!("sink was not an appsink"))?;

    // The output half is optional so the pipeline can be measured on a machine
    // with no spare loopback device -- creating one needs root.
    let out = output
        .as_ref()
        .map(|dev| -> Result<(gst::Pipeline, gst_app::AppSrc)> {
            let desc = format!(
                "appsrc name=src is-live=true format=time \
                 caps=video/x-raw,format=NV12,width={},height={},framerate={}/1 \
                 ! videoconvert ! v4l2sink device={} sync=false",
                args.width, args.height, args.fps, dev
            );
            let p = gst::parse::launch(&desc)
                .context("building the output pipeline")?
                .downcast::<gst::Pipeline>()
                .map_err(|_| anyhow::anyhow!("output pipeline was not a pipeline"))?;
            let s = p
                .by_name("src")
                .context("no appsrc")?
                .downcast::<gst_app::AppSrc>()
                .map_err(|_| anyhow::anyhow!("src was not an appsrc"))?;
            p.set_state(gst::State::Playing)?;
            Ok((p, s))
        })
        .transpose()?;

    pipeline.set_state(gst::State::Playing)?;
    println!(
        "reading {} at {}x{}{}",
        input,
        args.width,
        args.height,
        match &output {
            Some(d) => format!(", writing {d}"),
            None => ", discarding output (pass --output to publish)".into(),
        }
    );

    // Give the camera's auto-exposure a moment before snapshotting, or the
    // frame is a dark one that says nothing about mask quality.
    let snapshot_at = 30;

    // `w`/`h` are the output, which is what everything downstream of the
    // resample works in. The capture size is only the segmentation's and the
    // crop's business.
    let (w, h) = (args.width as usize, args.height as usize);
    let (cw, ch) = (cap_w as usize, cap_h as usize);
    let scaling = (cw, ch) != (w, h);
    if scaling {
        println!("capturing {cap_w}x{cap_h}, publishing {}x{}", args.width, args.height);
    }

    // The published frame, built fresh each time rather than copied from the
    // input: the input is the capture size and this is the output size.
    let out_info = gst_video::VideoInfo::builder(gst_video::VideoFormat::Nv12, args.width, args.height)
        .fps(gst::Fraction::new(args.fps as i32, 1))
        .build()?;
    let out_y_stride = out_info.stride()[0] as usize;
    let out_uv_stride = out_info.stride()[1] as usize;

    // The sharp frame at output size: the capture, cropped and scaled. The
    // composite needs it alongside the blurred copy, so it cannot be built in
    // place.
    let mut sharp_y = vec![0u8; out_y_stride * h];
    let mut sharp_uv = vec![0u8; out_uv_stride * h.div_ceil(2)];
    let mut scratch = vec![0u8; w * h];
    let mut upscaler = nv12::MaskUpscaler::new(w);
    let mut mask_filter = mask::MaskFilter::new(args.mask_gain, args.mask_smoothing);
    let mut mask = vec![0.0f32; nv12::NET * nv12::NET];

    // Empty counts as unset: the systemd unit passes --background
    // unconditionally, because it has no way to omit an argument.
    let background_path = args.background.as_deref().filter(|p| !p.is_empty());

    // Loaded whenever one is configured, not only when the effect starts as
    // `replace`, so switching to it over the socket is instant rather than a
    // decode stall mid-call.
    let backdrop = match background_path {
        Some(path) => Some(background::Background::load(path, args.width, args.height)?),
        None if args.effect == Effect::Replace => {
            anyhow::bail!("--effect replace needs --background <image>")
        }
        None => None,
    };

    let mut initial = Settings {
        effect: args.effect,
        blur: args.blur,
        passes: args.passes.clamp(1, 3),
        dim: args.dim.min(100),
        desat: args.desat.min(100),
        framing: args.framing,
        zoom: args.framing_zoom.clamp(100, 300),
        resume: if args.effect == Effect::None {
            Effect::Blur
        } else {
            args.effect
        },
        has_background: backdrop.is_some(),
        preview: false,
        model: loaded.clone(),
    };

    // What the panel last set wins over what the config starts with. Turning
    // the camera off stops this process, so without this every live change is
    // undone by the switch that is meant only to pause the camera.
    state::restore(&mut initial);

    // A remembered model that is no longer installed -- uninstalled, renamed,
    // or saved on another machine -- falls back to what the config asked for
    // rather than failing to start.
    if !models.iter().any(|m| *m == initial.model) {
        initial.model = loaded.clone();
    }
    if initial.model != loaded {
        match segmenter::Segmenter::new(&resolve_model(&initial.model), &args.cache, args.device.as_deref()) {
            Ok(other) => {
                println!("restoring the {} model chosen last time", initial.model);
                seg = other;
                loaded = initial.model.clone();
            }
            Err(e) => {
                eprintln!("cannot load the remembered {} model: {e:#}", initial.model);
                initial.model = loaded.clone();
            }
        }
    }
    let settings = Arc::new(Mutex::new(initial));

    match control::serve(
        Arc::clone(&settings),
        Fixed {
            device: seg.device.clone(),
            models: models.clone(),
            input: input.clone(),
            output: output.clone().unwrap_or_else(|| "(none)".into()),
            width: args.width,
            height: args.height,
            preview_path: preview::Preview::path_for_runtime()
                .to_string_lossy()
                .into_owned(),
        },
    ) {
        Ok(path) => println!("control socket at {}", path.display()),
        // A daemon that cannot be controlled is still a daemon that works, so
        // this is worth saying and not worth dying over.
        Err(e) => eprintln!("no control socket: {e:#}"),
    }
    let mut timings = Timings::default();
    let mut frame_no = 0u32;
    let mut preview: Option<preview::Preview> = None;

    let mut framer = framing::Framing::new(
        cap_w,
        cap_h,
        args.framing_zoom as f32 / 100.0,
        args.framing_dead_zone,
        args.framing_smoothing,
    );
    let mut resampler = nv12::Resampler::new(w);
    let mut resampler_uv = nv12::Resampler::new(w / 2);
    let mut aimed: Option<framing::Rect> = None;

    // A frame left by a daemon that was killed rather than shut down is a
    // picture of somebody's camera sitting in the runtime directory. Drop
    // cleans it up on an orderly exit; this covers the rest.
    let _ = std::fs::remove_file(preview::Preview::path_for_runtime());

    let camera = pipeline.by_name("camera");
    let mut announced = false;

    loop {
        let sample = match sink.pull_sample() {
            Ok(s) => s,
            // End of stream is the source finishing on purpose; anything else
            // is the camera going away under us. They have to be told apart:
            // exiting cleanly on a glitch looks like a successful shutdown, so
            // Restart=on-failure leaves the service down and the user's camera
            // silently stops working until they notice.
            Err(_) if sink.is_eos() => break,
            Err(e) => anyhow::bail!("the camera stopped delivering frames: {e}"),
        };

        // Said once, on the first frame, because what the camera agreed to is
        // the one fact no setting shows. The 720p default spent its whole life
        // decoding 1080p, and this line would have said so.
        if !announced {
            announced = true;
            let caps = camera.as_ref().and_then(|c| c.static_pad("src")).and_then(|p| p.current_caps());
            if let Some(s) = caps.as_ref().and_then(|c| c.structure(0)) {
                let rate = s.get::<gst::Fraction>("framerate").ok();
                println!(
                    "camera delivers {} {}x{} at {} fps",
                    s.name(),
                    s.get::<i32>("width").unwrap_or(0),
                    s.get::<i32>("height").unwrap_or(0),
                    rate.map_or("?".into(), |r| format!("{}/{}", r.numer(), r.denom())),
                );
            }
        }
        let info = gst_video::VideoInfo::from_caps(sample.caps().context("sample had no caps")?)?;
        let src_buf = sample.buffer().context("sample had no buffer")?;

        // Strides live on the VideoInfo, not the frame: v4l2 pads rows, so a
        // plane is never simply width bytes wide.
        let y_stride = info.stride()[0] as usize;
        let uv_stride = info.stride()[1] as usize;

        let in_frame = gst_video::VideoFrameRef::from_buffer_ref_readable(src_buf, &info)?;
        let y_in = in_frame.plane_data(0)?;
        let uv_in = in_frame.plane_data(1)?;

        // Read once per frame: the socket thread may change these at any point,
        // and a frame that blurred with one radius and blended with another
        // would tear.
        let (effect, blur_radius, passes, dim, desat, want_framing, zoom, want_preview, wanted_model) = {
            let s = settings.lock().expect("settings mutex poisoned");
            (s.effect, s.blur, s.passes, s.dim, s.desat, s.framing, s.zoom, s.preview, s.model.clone())
        };

        // Swapping the model costs one frame: the compile is cached, so it is
        // milliseconds, and a recurrent model starts from zeroed state as it
        // would on any other first frame. Done here rather than on the socket
        // thread because the segmenter belongs to this loop and nothing else
        // may touch it mid-inference.
        //
        // It must also happen before the frame is written, not after. Writing
        // first put the camera into the outgoing segmenter and then inferred on
        // the incoming one, whose input tensor no one had written: `Tensor::new`
        // hands back uninitialised memory, so the swap frame segmented whatever
        // the allocator was holding. With `matting` that frame is not merely
        // wrong, it is permanent -- the garbage becomes the recurrent state and
        // is fed back for the life of the daemon, so switching models appeared
        // to do nothing until the camera was turned off and on again.
        if wanted_model != loaded {
            match segmenter::Segmenter::new(&resolve_model(&wanted_model), &args.cache, args.device.as_deref()) {
                Ok(other) => {
                    println!("switched to the {} model on {}", other.model, other.device);
                    seg = other;
                    loaded = wanted_model;
                }
                Err(e) => {
                    // Keep the working model and put the setting back, so the
                    // panel shows what is running rather than what was asked
                    // for and refused.
                    eprintln!("cannot load the {wanted_model} model: {e:#}");
                    settings.lock().expect("settings mutex poisoned").model = loaded.clone();
                }
            }
        }

        let t = Instant::now();
        nv12::write_model_input(y_in, uv_in, cw, ch, y_stride, uv_stride, seg.input_buffer()?);
        timings.prep += t.elapsed().as_secs_f64() * 1e3;

        let t = Instant::now();
        mask.copy_from_slice(seg.infer()?);
        timings.infer += t.elapsed().as_secs_f64() * 1e3;
        mask_filter.apply(&mut mask);

        // Which part of the captured frame the output shows. The whole of it
        // unless framing has cropped in.
        let t = Instant::now();
        let crop = if want_framing {
            framer.set_max_zoom(zoom as f32 / 100.0);
            framer.update(&mask)
        } else {
            None
        }
        .unwrap_or(framing::Rect { x: 0.0, y: 0.0, w: cw as f32, h: ch as f32 });

        // Crop and scale into the sharp output-sized frame. Done before the
        // composite, not after: blurring at the capture size would throw away
        // the whole point of capturing larger, since the expensive stages would
        // run on the bigger frame and then be scaled down.
        if aimed != Some(crop) {
            resampler.aim(crop.x, crop.w, cw);
            resampler_uv.aim(crop.x / 2.0, crop.w / 2.0, cw / 2);
            upscaler.aim(
                crop.x * (nv12::NET - 1) as f32 / cw as f32,
                crop.w * (nv12::NET - 1) as f32 / cw as f32,
                crop.y * (nv12::NET - 1) as f32 / ch as f32,
                crop.h * (nv12::NET - 1) as f32 / ch as f32,
            );
            aimed = Some(crop);
        }
        // Nothing to scale and nothing cropped away: a straight row copy,
        // which is what this used to do. Resampling a frame onto itself is
        // three milliseconds of bilinear arithmetic for an identity, and most
        // frames are this case -- framing off, capture the same size as output.
        let untouched = !scaling
            && crop.x == 0.0
            && crop.y == 0.0
            && crop.w == cw as f32
            && crop.h == ch as f32;
        if untouched {
            for row in 0..h {
                sharp_y[row * out_y_stride..row * out_y_stride + w]
                    .copy_from_slice(&y_in[row * y_stride..row * y_stride + w]);
            }
            for row in 0..h / 2 {
                sharp_uv[row * out_uv_stride..row * out_uv_stride + w]
                    .copy_from_slice(&uv_in[row * uv_stride..row * uv_stride + w]);
            }
        } else {
            resampler.luma(y_in, &mut sharp_y, y_stride, out_y_stride, h, crop.y, crop.h, ch);
            resampler_uv.chroma(
                uv_in, &mut sharp_uv, uv_stride, out_uv_stride, h / 2,
                crop.y / 2.0, crop.h / 2.0, ch / 2,
            );
        }
        timings.frame += t.elapsed().as_secs_f64() * 1e3;

        let mut dst = gst::Buffer::with_size(out_info.size())?;
        {
            let dst_ref = dst.get_mut().expect("a freshly made buffer is writable");
            let mut out_frame = gst_video::VideoFrameRef::from_buffer_ref_writable(dst_ref, &out_info)?;
            let [y_out, uv_out, _, _] = out_frame.planes_data_mut();

            // The background starts as a copy of the sharp frame; the subject
            // is painted back over it afterwards. Neither the blur nor the
            // image copy has to know anything about the mask this way.
            for row in 0..h {
                y_out[row * out_y_stride..row * out_y_stride + w]
                    .copy_from_slice(&sharp_y[row * out_y_stride..row * out_y_stride + w]);
            }
            for row in 0..h / 2 {
                uv_out[row * out_uv_stride..row * out_uv_stride + w]
                    .copy_from_slice(&sharp_uv[row * out_uv_stride..row * out_uv_stride + w]);
            }

            let t = Instant::now();
            match effect {
                Effect::Replace => {
                    if let Some(bg) = &backdrop {
                        bg.paint(y_out, uv_out, w, h, out_y_stride, out_uv_stride);
                    }
                }
                Effect::Blur => {
                    // Repeated box blur converges on a Gaussian. Two passes is
                    // the point where the boxiness stops being visible against
                    // a hard edge, which is why it is the default.
                    //
                    // Chroma is half the resolution, so half the radius --
                    // and two channels, which box_blur must never be given.
                    for _ in 0..passes {
                        nv12::box_blur(y_out, &mut scratch, w, h, out_y_stride, blur_radius);
                        nv12::box_blur_uv(uv_out, &mut scratch, w, h / 2, out_uv_stride, blur_radius / 2);
                    }
                }
                Effect::None => {}
            }

            // On the background only, and before the blend, so the subject is
            // never dimmed or drained along with what is behind them.
            if effect != Effect::None {
                nv12::tint(y_out, uv_out, w, h, out_y_stride, out_uv_stride, dim, desat);
            }
            timings.blur += t.elapsed().as_secs_f64() * 1e3;

            let t = Instant::now();
            if effect != Effect::None {
                upscaler.prepare(&mask);
                nv12::blend_luma(&sharp_y, y_out, &upscaler, w, h, out_y_stride);
                nv12::blend_chroma(&sharp_uv, uv_out, &upscaler, w / 2, h / 2, out_uv_stride);
            }
            timings.blend += t.elapsed().as_secs_f64() * 1e3;
        }

        drop(in_frame);

        // Built on first use and dropped when nothing is watching, so a daemon
        // nobody has a panel open on runs no encoder at all -- and the stale
        // last frame is removed with it, rather than left for a widget to show
        // as if the camera were still on.
        if want_preview {
            if preview.is_none() {
                match preview::Preview::new(args.height as i32, args.width as i32) {
                    Ok(p) => preview = Some(p),
                    Err(e) => eprintln!("preview unavailable: {e:#}"),
                }
            }
            if let Some(p) = &mut preview {
                // A failed preview is a stale picture in a widget. That is not
                // a reason to drop somebody's video call, so it is reported
                // once and the frame loop carries on.
                if let Err(e) = p.offer(&dst, &out_info) {
                    eprintln!("preview frame dropped: {e:#}");
                }
            }
        } else if preview.is_some() {
            preview = None;
        }

        if let Some(path) = &args.snapshot {
            frame_no += 1;
            if frame_no >= snapshot_at {
                write_png(&dst, &out_info, path)?;
                println!("wrote {path}");
                break;
            }
        }

        if let Some((_, src)) = &out {
            src.push_buffer(dst).ok();
        }

        timings.frames += 1;
        if args.stats_every > 0 && timings.frames >= args.stats_every {
            timings.report(&seg.device);
        }

    }

    pipeline.set_state(gst::State::Null)?;
    if let Some((p, _)) = &out {
        p.set_state(gst::State::Null)?;
    }
    Ok(())
}
