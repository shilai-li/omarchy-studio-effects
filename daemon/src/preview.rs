//! A sharp JPEG of the composited frame, for the bar widget to show.
//!
//! The widget cannot simply open "Studio Camera" and watch it. v4l2loopback
//! allows ten openers, but the second one's REQBUFS invalidates the first's
//! buffer pool: measured here, a second reader joining broke the reader already
//! streaming with "Failed to allocate a buffer". A preview that competes for
//! the device would kill the video call it is previewing, and the call is the
//! thing that matters.
//!
//! So the frames leave by a side channel instead. The daemon already holds
//! every composited frame, and publishing a JPEG somewhere the widget can
//! read lets encoding run off the frame loop and contends with no camera.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;

/// Retain the composited frame's full HD detail. The widget scales for its
/// display, but a 1920-wide camera no longer loses pixels before it gets there.
/// Keep smaller sources at their native size rather than inventing pixels.
const MAX_WIDTH: i32 = 1920;

/// A wall-clock deadline, independent of the camera's frame rate. A single
/// empty pull is normal: encoding runs on another thread.
const STALLED: Duration = Duration::from_secs(2);

pub struct Preview {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
    sink: gst_app::AppSink,
    path: PathBuf,
    temp: PathBuf,
    last_progress: Instant,
    reported_stall: bool,
}

impl Preview {
    /// Where the JPEG lives. In the runtime directory because it is a tmpfs
    /// owned by this user and cleared at logout -- a frame of someone's camera
    /// has no business surviving the session on disk.
    pub fn path_for_runtime() -> PathBuf {
        let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(dir).join("studio-effects-preview.jpg")
    }

    pub fn new(height: i32, width: i32) -> Result<Self> {
        Self::new_at(height, width, Self::path_for_runtime())
    }

    fn new_at(height: i32, width: i32, path: PathBuf) -> Result<Self> {
        // Height follows the source's aspect, rounded to even for the encoder.
        let scaled_width = width.clamp(2, MAX_WIDTH) & !1;
        let scaled_height =
            ((scaled_width as i64 * height as i64 / width.max(1) as i64) as i32).max(2) & !1;

        // Scaled first and never converted: jpegenc takes NV12 as it is. The
        // obvious `videoconvert ! videoscale` converts the whole frame to I420
        // before scaling it. Lanczos keeps detail when shrinking, and the
        // accurate DCT avoids the fast integer method's loss at high quality.
        let desc = format!(
            "appsrc name=src format=time is-live=true block=false \
             max-buffers=1 max-bytes=0 leaky-type=downstream emit-signals=false \
             ! videoscale method=lanczos \
             ! video/x-raw,width={scaled_width},height={scaled_height} \
             ! jpegenc quality=95 idct-method=islow \
             ! appsink name=sink max-buffers=1 drop=true sync=false"
        );
        let pipeline = gst::parse::launch(&desc)
            .context("building the preview pipeline")?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow::anyhow!("preview pipeline was not a pipeline"))?;

        let src = pipeline
            .by_name("src")
            .context("no preview appsrc")?
            .downcast::<gst_app::AppSrc>()
            .map_err(|_| anyhow::anyhow!("preview src was not an appsrc"))?;
        let sink = pipeline
            .by_name("sink")
            .context("no preview appsink")?
            .downcast::<gst_app::AppSink>()
            .map_err(|_| anyhow::anyhow!("preview sink was not an appsink"))?;

        pipeline.set_state(gst::State::Playing)?;

        let temp = path.with_extension("jpg.tmp");
        Ok(Self {
            pipeline,
            src,
            sink,
            path,
            temp,
            last_progress: Instant::now(),
            reported_stall: false,
        })
    }

    /// Offer every composited frame, at the camera's actual delivered cadence.
    ///
    /// Never waits for the encoder. What gets written is the JPEG the encoder
    /// finished since the last offer, collected on the way in, and this frame
    /// is handed over to be ready by the next one. There is no timer or FPS
    /// ceiling. A single queued input is replaced with the newest frame if
    /// encoding falls behind, keeping latency and memory bounded. Waiting for
    /// each JPEG, which is what this did first, held the loop 1.1 ms at 720p
    /// and 2.8 ms at 1080p on every third frame the panel was open -- a late
    /// frame on the call, for a picture a tenth of a second fresher.
    /// `cargo run --release --example preview_cost` measures both time held
    /// and total CPU, including the higher-resolution encoder's worker.
    ///
    /// Errors are returned rather than propagated into the frame loop by the
    /// caller: a preview that fails is a widget with a stale picture, which is
    /// not a reason to drop somebody's video call.
    pub fn offer(&mut self, buffer: &gst::Buffer, info: &gst_video::VideoInfo) -> Result<()> {
        let now = Instant::now();
        let finished = self.sink.try_pull_sample(gst::ClockTime::ZERO);

        if self.src.caps().is_none() {
            self.src.set_caps(Some(&info.to_caps()?));
        }
        self.src
            .push_buffer(buffer.copy())
            .map_err(|e| anyhow::anyhow!("pushing a preview frame: {e:?}"))?;

        let Some(sample) = finished else {
            // Said once per stall, rather than on every camera frame.
            if !self.reported_stall && now.duration_since(self.last_progress) >= STALLED {
                self.reported_stall = true;
                anyhow::bail!("the preview encoder has produced nothing for {:?}", STALLED);
            }
            return Ok(());
        };
        self.last_progress = now;
        self.reported_stall = false;
        let encoded = sample.buffer().context("preview sample had no buffer")?;
        let map = encoded.map_readable()?;

        // Written to a temporary name and renamed into place, because the
        // widget re-reads this file on a timer of its own. rename(2) is atomic
        // within a filesystem, so a reader sees the previous whole frame or the
        // next whole frame, never half of one being written.
        std::fs::write(&self.temp, map.as_slice())
            .with_context(|| format!("writing {:?}", self.temp))?;
        std::fs::rename(&self.temp, &self.path)
            .with_context(|| format!("renaming into {:?}", self.path))?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gst_video::VideoFrameExt;

    fn detail(x: usize, y: usize) -> u8 {
        64 + ((x * 37 + y * 29) % 128) as u8
    }

    /// Encode through the real preview pipeline and decode its JPEG again.
    /// No camera, and no use of the live widget's runtime file.
    fn decoded_preview(width: u32, height: u32) -> Result<gst::Sample> {
        gst::init()?;
        let info = gst_video::VideoInfo::builder(gst_video::VideoFormat::Nv12, width, height)
            .fps(gst::Fraction::new(30, 1))
            .build()?;
        let mut buffer = gst::Buffer::with_size(info.size())?;
        {
            let mut frame = gst_video::VideoFrameRef::from_buffer_ref_writable(
                buffer.get_mut().unwrap(),
                &info,
            )?;
            let y_stride = frame.plane_stride()[0] as usize;
            let [luma, chroma, _, _] = frame.planes_data_mut();
            chroma.fill(128);
            for y in 0..height as usize {
                for x in 0..width as usize {
                    luma[y * y_stride + x] = detail(x, y);
                }
            }
        }
        let path = std::env::temp_dir().join(format!(
            "studio-preview-test-{}-{width}x{height}.jpg",
            std::process::id(),
        ));
        let mut preview = Preview::new_at(height as i32, width as i32, path)?;
        preview.offer(&buffer, &info)?;
        let encoded = preview
            .sink
            .try_pull_sample(gst::ClockTime::from_seconds(5))
            .context("preview encoder did not produce a JPEG")?;

        let decoder = gst::parse::launch(
            "appsrc name=src format=time caps=image/jpeg \
             ! jpegdec ! video/x-raw,format=I420 ! appsink name=sink sync=false",
        )?
        .downcast::<gst::Pipeline>()
        .unwrap();
        let src = decoder
            .by_name("src")
            .unwrap()
            .downcast::<gst_app::AppSrc>()
            .unwrap();
        let sink = decoder
            .by_name("sink")
            .unwrap()
            .downcast::<gst_app::AppSink>()
            .unwrap();
        decoder.set_state(gst::State::Playing)?;
        src.push_buffer(encoded.buffer().unwrap().copy())?;
        src.end_of_stream()?;
        let decoded = sink.try_pull_sample(gst::ClockTime::from_seconds(5));
        decoder.set_state(gst::State::Null)?;
        decoded.context("preview JPEG did not decode")
    }

    #[test]
    fn preview_retains_full_hd_and_small_cameras_native_resolution() -> Result<()> {
        for (width, height, expected) in [
            (2560, 1440, (1920, 1080)),
            (1920, 1080, (1920, 1080)),
            (1600, 1200, (1600, 1200)),
            (1280, 720, (1280, 720)),
            (640, 480, (640, 480)),
            (320, 240, (320, 240)),
        ] {
            let sample = decoded_preview(width, height)?;
            let info = gst_video::VideoInfo::from_caps(sample.caps().unwrap())?;
            assert_eq!((info.width(), info.height()), expected);
        }
        Ok(())
    }

    #[test]
    fn preview_preserves_fine_luma_detail() -> Result<()> {
        for (width, height) in [(1280, 720), (1920, 1080)] {
            let sample = decoded_preview(width, height)?;
            let info = gst_video::VideoInfo::from_caps(sample.caps().unwrap())?;
            assert_eq!((info.width(), info.height()), (width, height));
            let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(
                sample.buffer().unwrap(),
                &info,
            )?;
            let stride = frame.plane_stride()[0] as usize;
            let luma = frame.plane_data(0)?;
            let mut error = 0u64;
            for y in 0..height as usize {
                for x in 0..width as usize {
                    error += luma[y * stride + x].abs_diff(detail(x, y)) as u64;
                }
            }
            let mean_error = error as f64 / (f64::from(width) * f64::from(height));
            assert!(
                mean_error < 2.0,
                "{width}x{height} preview lost fine detail: mean luma error {mean_error:.3}"
            );
        }
        Ok(())
    }

    #[test]
    fn preview_offers_every_source_frame_without_timer_throttling() -> Result<()> {
        gst::init()?;
        for (numer, denom) in [(24, 1), (30000, 1001), (60, 1)] {
            let info = gst_video::VideoInfo::builder(gst_video::VideoFormat::Nv12, 640, 480)
                .fps(gst::Fraction::new(numer, denom))
                .build()?;
            let mut buffer = gst::Buffer::with_size(info.size())?;
            buffer
                .get_mut()
                .unwrap()
                .map_writable()?
                .as_mut_slice()
                .fill(128);
            let path = std::env::temp_dir().join(format!(
                "studio-preview-cadence-{}-{numer}-{denom}.jpg",
                std::process::id()
            ));
            let mut preview = Preview::new_at(480, 640, path)?;
            let mut pts = gst::ClockTime::ZERO;
            // Irregular arrival timestamps also cover a camera slowing down
            // without renegotiating its nominal FPS. Explicit pulls wait for
            // each encode; there is no sleep to accidentally hide a throttle.
            for index in 0..16 {
                buffer.get_mut().unwrap().set_pts(pts);
                preview.offer(&buffer, &info)?;
                let encoded = preview
                    .sink
                    .try_pull_sample(gst::ClockTime::from_seconds(5))
                    .context("a source frame was skipped by the preview")?;
                assert_eq!(encoded.buffer().unwrap().pts(), Some(pts));
                pts += gst::ClockTime::from_mseconds([17, 33, 100, 42][index % 4]);
            }
        }
        Ok(())
    }

    #[test]
    fn slow_preview_encoding_keeps_only_the_newest_queued_frame() -> Result<()> {
        gst::init()?;
        let info = gst_video::VideoInfo::builder(gst_video::VideoFormat::Nv12, 640, 480)
            .fps(gst::Fraction::new(60, 1))
            .build()?;
        let mut buffer = gst::Buffer::with_size(info.size())?;
        buffer
            .get_mut()
            .unwrap()
            .map_writable()?
            .as_mut_slice()
            .fill(128);
        let path =
            std::env::temp_dir().join(format!("studio-preview-backlog-{}.jpg", std::process::id()));
        let mut preview = Preview::new_at(480, 640, path)?;
        // Hold the first buffer on the streaming thread while offers continue.
        // The camera loop must neither wait for it nor accumulate old frames.
        let pad = preview.src.static_pad("src").unwrap();
        let (entered, wait) = std::sync::mpsc::channel();
        let probe = pad
            .add_probe(
                gst::PadProbeType::BLOCK | gst::PadProbeType::BUFFER,
                move |_, _| {
                    let _ = entered.send(());
                    gst::PadProbeReturn::Ok
                },
            )
            .unwrap();
        buffer.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
        preview.offer(&buffer, &info)?;
        wait.recv_timeout(Duration::from_secs(5))?;
        for index in 1..128 {
            buffer
                .get_mut()
                .unwrap()
                .set_pts(gst::ClockTime::from_mseconds(index * 17));
            preview.offer(&buffer, &info)?;
            assert!(preview.src.current_level_bytes() <= info.size() as u64);
        }
        pad.remove_probe(probe);
        let last = gst::ClockTime::from_mseconds(127 * 17);
        // Only the held frame and the latest queued frame may emerge.
        for _ in 0..2 {
            let encoded = preview
                .sink
                .try_pull_sample(gst::ClockTime::from_seconds(5))
                .context("the preview did not resume after its worker stalled")?;
            if encoded.buffer().unwrap().pts() == Some(last) {
                return Ok(());
            }
        }
        anyhow::bail!("the preview replayed old frames instead of the latest one")
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        // Leaving the last frame behind would let a widget show a picture of a
        // camera that is no longer running.
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(&self.temp);
    }
}
