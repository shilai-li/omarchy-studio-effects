//! studio-effects-daemon -- read a camera, blur the background, publish the
//! result as a second camera.
//!
//! See AGENTS.md for the measurements the design follows. The short version:
//! inference is under a millisecond and colour conversion is not, so the frame
//! never leaves NV12 and only the 256x256 the model needs is ever converted.

mod nv12;
mod segmenter;

use anyhow::{Context, Result};
use clap::Parser;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use std::time::Instant;

#[derive(Parser)]
#[command(about = "NPU-accelerated camera background effects", version)]
struct Args {
    /// Camera to read.
    #[arg(short, long, default_value = "/dev/video0")]
    input: String,

    /// v4l2loopback device to publish to. Omitted, frames are processed and
    /// dropped -- which is how you measure the pipeline without a loopback.
    #[arg(short, long)]
    output: Option<String>,

    #[arg(long, default_value_t = 1280)]
    width: u32,

    #[arg(long, default_value_t = 720)]
    height: u32,

    #[arg(long, default_value_t = 30)]
    fps: u32,

    /// Force a device instead of taking the best available (NPU, GPU, CPU).
    #[arg(long, value_parser = parse_device)]
    device: Option<String>,

    /// Background blur radius in pixels, at the frame's own scale.
    #[arg(long, default_value_t = 12)]
    blur: usize,

    #[arg(long, default_value = "models/selfie_segmentation.xml")]
    model: String,

    #[arg(long, default_value = "/tmp/studio-effects-cache")]
    cache: String,

    /// Print a timing line every N frames.
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
    gst::init().context("initialising GStreamer")?;

    let mut seg = segmenter::Segmenter::new(&args.model, &args.cache, args.device.as_deref())?;
    println!("segmenting on {}", seg.device);

    // decodebin because a USB camera hands over MJPEG while a loopback hands
    // over raw NV12, and the daemon should not care which.
    let src = format!(
        "v4l2src device={} ! decodebin ! videoconvert ! videoscale \
         ! video/x-raw,format=NV12,width={},height={},framerate={}/1 \
         ! appsink name=sink max-buffers=2 drop=true sync=false",
        args.input, args.width, args.height, args.fps
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
    let out = args
        .output
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
        args.input,
        args.width,
        args.height,
        match &args.output {
            Some(d) => format!(", writing {d}"),
            None => ", discarding output (pass --output to publish)".into(),
        }
    );

    // Give the camera's auto-exposure a moment before snapshotting, or the
    // frame is a dark one that says nothing about mask quality.
    let snapshot_at = 30;

    let (w, h) = (args.width as usize, args.height as usize);
    let mut scratch = vec![0u8; w * h];
    let mut timings = Timings::default();
    let mut frame_no = 0u32;

    loop {
        let sample = match sink.pull_sample() {
            Ok(s) => s,
            Err(_) => break, // EOS or the camera went away
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

        let t = Instant::now();
        let mask = seg.infer()?;
        timings.infer += t.elapsed().as_secs_f64() * 1e3;

        {
            let dst_ref = dst.get_mut().context("output buffer was not writable")?;
            let mut out_frame = gst_video::VideoFrameRef::from_buffer_ref_writable(dst_ref, &info)?;
            let [y_out, uv_out, _, _] = out_frame.planes_data_mut();

            // Blur the whole copy, then paint the sharp subject back over it.
            // Doing it this way means the blur never has to know about the mask.
            let t = Instant::now();
            nv12::box_blur(y_out, &mut scratch, w, h, y_stride, args.blur);
            nv12::box_blur(uv_out, &mut scratch, w, h / 2, uv_stride, args.blur / 2);
            timings.blur += t.elapsed().as_secs_f64() * 1e3;

            let t = Instant::now();
            nv12::blend_luma(y_in, y_out, mask, w, h, y_stride);
            nv12::blend_chroma(uv_in, uv_out, mask, w / 2, h / 2, uv_stride);
            timings.blend += t.elapsed().as_secs_f64() * 1e3;
        }

        drop(in_frame);

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
        if timings.frames >= args.stats_every {
            timings.report(&seg.device);
        }

    }

    pipeline.set_state(gst::State::Null)?;
    if let Some((p, _)) = &out {
        p.set_state(gst::State::Null)?;
    }
    Ok(())
}
