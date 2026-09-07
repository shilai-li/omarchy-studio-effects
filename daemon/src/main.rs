//! studio-effects-daemon -- read a camera, blur the background, publish the
//! result as a second camera.
//!
//! See AGENTS.md for the measurements the design follows. The short version:
//! inference is under a millisecond and colour conversion is not, so the frame
//! never leaves NV12 and only the 256x256 the model needs is ever converted.


use anyhow::{Context, Result};
use std::sync::{Arc, Mutex};
use studio_effects_daemon::control::{self, Effect, Fixed, Settings};
use studio_effects_daemon::{background, device, mask, nv12, preview, segmenter};
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

    /// How hard to push the model's probabilities toward solid foreground or
    /// solid background. 1 uses them as-is, which is what made moving limbs
    /// look transparent.
    #[arg(long, default_value_t = 3.0)]
    mask_gain: f32,

    /// Weight kept from the previous frame's mask, 0 to 0.95. Steadies edges
    /// while you hold still; too much smears the silhouette when you move.
    #[arg(long, default_value_t = 0.5)]
    mask_smoothing: f32,

    #[arg(long, default_value = "models/selfie_segmentation.xml")]
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
}

impl Timings {
    fn report(&mut self, device: &str) {
        let n = self.frames as f64;
        let total = self.prep + self.infer + self.blur + self.blend;
        println!(
            "{device:>3}  {:5.2} ms/frame  (prep {:4.2}  infer {:4.2}  blur {:4.2}  blend {:4.2})  \
             {:5.1}% of a 33 ms budget",
            total / n,
            self.prep / n,
            self.infer / n,
            self.blur / n,
            self.blend / n,
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

    let mut seg = segmenter::Segmenter::new(&args.model, &args.cache, args.device.as_deref())?;
    println!("segmenting on {}", seg.device);

    // decodebin because a USB camera hands over MJPEG while a loopback hands
    // over raw NV12, and the daemon should not care which.
    let src = format!(
        "v4l2src device={} ! decodebin ! videoconvert ! videoscale \
         ! video/x-raw,format=NV12,width={},height={},framerate={}/1 \
         ! appsink name=sink max-buffers=2 drop=true sync=false",
        input, args.width, args.height, args.fps
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

    let (w, h) = (args.width as usize, args.height as usize);
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

    let settings = Arc::new(Mutex::new(Settings {
        effect: args.effect,
        blur: args.blur,
        resume: if args.effect == Effect::None {
            Effect::Blur
        } else {
            args.effect
        },
        has_background: backdrop.is_some(),
        preview: false,
    }));

    match control::serve(
        Arc::clone(&settings),
        Fixed {
            device: seg.device.clone(),
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

    // A frame left by a daemon that was killed rather than shut down is a
    // picture of somebody's camera sitting in the runtime directory. Drop
    // cleans it up on an orderly exit; this covers the rest.
    let _ = std::fs::remove_file(preview::Preview::path_for_runtime());

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
        let info = gst_video::VideoInfo::from_caps(sample.caps().context("sample had no caps")?)?;
        let src_buf = sample.buffer().context("sample had no buffer")?;

        // Strides live on the VideoInfo, not the frame: v4l2 pads rows, so a
        // plane is never simply width bytes wide.
        let y_stride = info.stride()[0] as usize;
        let uv_stride = info.stride()[1] as usize;

        let mut dst = src_buf.copy_deep()?;
        let in_frame = gst_video::VideoFrameRef::from_buffer_ref_readable(src_buf, &info)?;
        let y_in = in_frame.plane_data(0)?;
        let uv_in = in_frame.plane_data(1)?;

        let t = Instant::now();
        nv12::write_model_input(y_in, uv_in, w, h, y_stride, uv_stride, seg.input_buffer()?);
        timings.prep += t.elapsed().as_secs_f64() * 1e3;

        // Read once per frame: the socket thread may change these at any point,
        // and a frame that blurred with one radius and blended with another
        // would tear.
        let (effect, blur_radius, want_preview) = {
            let s = settings.lock().expect("settings mutex poisoned");
            (s.effect, s.blur, s.preview)
        };

        let t = Instant::now();
        mask.copy_from_slice(seg.infer()?);
        timings.infer += t.elapsed().as_secs_f64() * 1e3;
        mask_filter.apply(&mut mask);

        {
            let dst_ref = dst.get_mut().context("output buffer was not writable")?;
            let mut out_frame = gst_video::VideoFrameRef::from_buffer_ref_writable(dst_ref, &info)?;
            let [y_out, uv_out, _, _] = out_frame.planes_data_mut();

            // Build the background over the whole frame, then paint the sharp
            // subject back on top. Doing it this way means neither the blur nor
            // the image copy has to know anything about the mask.
            let t = Instant::now();
            match effect {
                Effect::Replace => {
                    if let Some(bg) = &backdrop {
                        bg.paint(y_out, uv_out, w, h, y_stride, uv_stride);
                    }
                }
                Effect::Blur => {
                    nv12::box_blur(y_out, &mut scratch, w, h, y_stride, blur_radius);
                    nv12::box_blur(uv_out, &mut scratch, w, h / 2, uv_stride, blur_radius / 2);
                }
                Effect::None => {}
            }
            timings.blur += t.elapsed().as_secs_f64() * 1e3;

            let t = Instant::now();
            if effect != Effect::None {
                upscaler.prepare(&mask);
                nv12::blend_luma(y_in, y_out, &upscaler, w, h, y_stride);
                nv12::blend_chroma(uv_in, uv_out, &upscaler, w / 2, h / 2, uv_stride);
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
                if let Err(e) = p.offer(&dst, &info) {
                    eprintln!("preview frame dropped: {e:#}");
                }
            }
        } else if preview.is_some() {
            preview = None;
        }

        if let Some(path) = &args.snapshot {
            frame_no += 1;
            if frame_no >= snapshot_at {
                write_png(&dst, &info, path)?;
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
