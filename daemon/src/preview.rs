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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;

/// The ceiling when the widget has not said how big it draws the preview -- an
/// older widget, or a size it could not work out. Full HD keeps a 1080p
/// camera's detail; the panel then reduces it with mipmaps, as it always did.
const MAX_WIDTH: i32 = 1920;

/// The largest display size taken from the widget. Beyond any panel, and it
/// only bounds a request that is then capped at the camera's own size anyway.
pub const MAX_TARGET: u32 = 7680;

/// A wall-clock deadline, independent of the camera's frame rate. A single
/// empty pull is normal: encoding runs on another thread.
const STALLED: Duration = Duration::from_secs(2);

/// What the encoder's thread reports back to the frame loop.
struct Published {
    /// When the last JPEG was renamed into place.
    at: Mutex<Instant>,
    /// A write that failed, reported by the next `offer` rather than lost on a
    /// thread nobody reads.
    failure: Mutex<Option<String>>,
    #[cfg(test)]
    tap: Mutex<Option<std::sync::mpsc::Sender<gst::Sample>>>,
}

pub struct Preview {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
    path: PathBuf,
    temp: PathBuf,
    target: Option<(u32, u32)>,
    size: (i32, i32),
    published: Arc<Published>,
    /// The first offer still waiting for a JPEG, which is what a stall is
    /// measured from. Measuring from the last publish instead would call a
    /// camera that paused for two seconds a stuck encoder.
    waiting_since: Option<Instant>,
    reported_stall: bool,
}

/// The encoded size for a `width` x `height` frame drawn in a `target` box of
/// physical pixels.
///
/// The panel crops to fill its box, so the frame is scaled until it covers the
/// box in both directions -- the box is then drawn from it 1:1, after one
/// Lanczos reduction, instead of reduced a second time by the GPU's mipmaps,
/// which is softer. Never larger than the frame: a pixel the camera did not
/// deliver is not detail. Rounded up to even, because the encoder takes NV12,
/// so a box of 405 rows gets 406 and loses one to the crop rather than gaining
/// a stretch.
pub fn encoded_size(height: i32, width: i32, target: Option<(u32, u32)>) -> (i32, i32) {
    let (width, height) = (width.max(2), height.max(2));
    let even_up = |v: f64, limit: i32| ((v.ceil() as i32 + 1) & !1).clamp(2, limit.max(2) & !1);
    match target {
        Some((tw, th)) if tw > 0 && th > 0 => {
            let scale = (f64::from(tw) / f64::from(width))
                .max(f64::from(th) / f64::from(height))
                .min(1.0);
            (
                even_up(f64::from(width) * scale, width),
                even_up(f64::from(height) * scale, height),
            )
        }
        _ => {
            // Height follows the source's aspect, rounded to even for the encoder.
            let w = width.clamp(2, MAX_WIDTH) & !1;
            (w, ((w as i64 * height as i64 / width as i64) as i32).max(2) & !1)
        }
    }
}

impl Preview {
    /// Where the JPEG lives. In the runtime directory because it is a tmpfs
    /// owned by this user and cleared at logout -- a frame of someone's camera
    /// has no business surviving the session on disk.
    pub fn path_for_runtime() -> PathBuf {
        let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(dir).join("studio-effects-preview.jpg")
    }

    /// `target` is the size the widget draws the preview at, in physical
    /// pixels, or `None` when it did not say.
    pub fn new(height: i32, width: i32, target: Option<(u32, u32)>) -> Result<Self> {
        Self::new_at(height, width, target, Self::path_for_runtime())
    }

    fn new_at(height: i32, width: i32, target: Option<(u32, u32)>, path: PathBuf) -> Result<Self> {
        let (scaled_width, scaled_height) = encoded_size(height, width, target);

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

        let temp = path.with_extension("jpg.tmp");
        let published = Arc::new(Published {
            at: Mutex::new(Instant::now()),
            failure: Mutex::new(None),
            #[cfg(test)]
            tap: Mutex::new(None),
        });

        // Published from the encoder's own thread, the moment a JPEG exists.
        // Collecting it on the next `offer` instead, which is what this did
        // first, held every finished JPEG for a whole camera frame -- 33 ms at
        // 30 fps and 100 in dim light -- before the panel could see it.
        {
            let (path, temp, published) = (path.clone(), temp.clone(), published.clone());
            sink.set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |sink| {
                        let sample = sink.pull_sample().map_err(|_| gst::FlowError::Flushing)?;
                        match publish(&sample, &temp, &path) {
                            Ok(()) => *published.at.lock().unwrap() = Instant::now(),
                            Err(e) => *published.failure.lock().unwrap() = Some(format!("{e:#}")),
                        }
                        #[cfg(test)]
                        if let Some(tap) = &*published.tap.lock().unwrap() {
                            let _ = tap.send(sample);
                        }
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .build(),
            );
        }

        pipeline.set_state(gst::State::Playing)?;

        Ok(Self {
            pipeline,
            src,
            path,
            temp,
            target,
            size: (scaled_width, scaled_height),
            published,
            waiting_since: None,
            reported_stall: false,
        })
    }

    /// Offer every composited frame, at the camera's actual delivered cadence.
    ///
    /// Never waits for the encoder: the frame is handed over and the JPEG is
    /// written by the encoder's thread when it is done. There is no timer or
    /// FPS ceiling. A single queued input is replaced with the newest frame if
    /// encoding falls behind, keeping latency and memory bounded. Waiting for
    /// each JPEG, which is what this did first, held the loop 1.1 ms at 720p
    /// and 2.8 ms at 1080p on every third frame the panel was open -- a late
    /// frame on the call, for a picture a tenth of a second fresher.
    /// `cargo run --release --example preview_cost` measures both time held
    /// and total CPU, including the encoder's worker.
    ///
    /// Errors are returned rather than propagated into the frame loop by the
    /// caller: a preview that fails is a widget with a stale picture, which is
    /// not a reason to drop somebody's video call.
    pub fn offer(&mut self, buffer: &gst::Buffer, info: &gst_video::VideoInfo) -> Result<()> {
        let now = Instant::now();

        if self.src.caps().is_none() {
            self.src.set_caps(Some(&info.to_caps()?));
        }
        self.src
            .push_buffer(buffer.copy())
            .map_err(|e| anyhow::anyhow!("pushing a preview frame: {e:?}"))?;

        if let Some(failure) = self.published.failure.lock().unwrap().take() {
            anyhow::bail!("{failure}");
        }

        let published = *self.published.at.lock().unwrap();
        match self.waiting_since {
            Some(since) if published >= since => {
                self.waiting_since = Some(now);
                self.reported_stall = false;
            }
            // Said once per stall, rather than on every camera frame.
            Some(since) if !self.reported_stall && now.duration_since(since) >= STALLED => {
                self.reported_stall = true;
                anyhow::bail!("the preview encoder has produced nothing for {:?}", STALLED);
            }
            Some(_) => {}
            None => self.waiting_since = Some(now),
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The display size this encoder was built for, to tell when the widget
    /// has asked for another.
    pub fn target(&self) -> Option<(u32, u32)> {
        self.target
    }

    /// What it actually encodes.
    pub fn size(&self) -> (i32, i32) {
        self.size
    }

    /// Every JPEG the encoder publishes, as well as the file. Tests read the
    /// samples, because the file has no timestamp to check.
    #[cfg(test)]
    fn tap(&self) -> std::sync::mpsc::Receiver<gst::Sample> {
        let (tx, rx) = std::sync::mpsc::channel();
        *self.published.tap.lock().unwrap() = Some(tx);
        rx
    }
}

/// Written to a temporary name and renamed into place, because the widget
/// re-reads this file whenever it changes. rename(2) is atomic within a
/// filesystem, so a reader sees the previous whole frame or the next whole
/// frame, never half of one being written.
fn publish(sample: &gst::Sample, temp: &Path, path: &Path) -> Result<()> {
    let encoded = sample.buffer().context("preview sample had no buffer")?;
    let map = encoded.map_readable()?;
    std::fs::write(temp, map.as_slice()).with_context(|| format!("writing {temp:?}"))?;
    std::fs::rename(temp, path).with_context(|| format!("renaming into {path:?}"))?;
    Ok(())
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
    fn decoded_preview(width: u32, height: u32, target: Option<(u32, u32)>) -> Result<gst::Sample> {
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
        let mut preview = Preview::new_at(height as i32, width as i32, target, path)?;
        let published = preview.tap();
        preview.offer(&buffer, &info)?;
        let encoded = published
            .recv_timeout(Duration::from_secs(5))
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
            let sample = decoded_preview(width, height, None)?;
            let info = gst_video::VideoInfo::from_caps(sample.caps().unwrap())?;
            assert_eq!((info.width(), info.height()), expected);
        }
        Ok(())
    }

    /// The widget's box, in physical pixels, is covered exactly: the panel
    /// crops what overhangs and draws the rest 1:1.
    #[test]
    fn a_display_size_is_covered_without_upscaling() {
        for (source, target, expected) in [
            // 2x display, 360-point panel: one Lanczos reduction, no mipmaps.
            ((1920, 1080), (720, 405), (720, 406)),
            // A fractional scale rounds up to the next even pixel, never down.
            ((1920, 1080), (721, 406), (722, 406)),
            // 304 rows of 16:9 need 540.4 columns, and a partial one is rounded up.
            ((1280, 720), (540, 304), (542, 304)),
            // A 4:3 camera in a 16:9 box is wide enough and taller than needed.
            ((1280, 960), (720, 405), (720, 540)),
            // A box bigger than the camera gets the camera, not a stretch.
            ((640, 480), (1440, 810), (640, 480)),
            ((1920, 1080), (3840, 2160), (1920, 1080)),
        ] {
            assert_eq!(
                encoded_size(source.1, source.0, Some(target)),
                expected,
                "{source:?} into {target:?}"
            );
        }
        // No size from the widget is the old behaviour, so an older widget
        // sees exactly what it did.
        assert_eq!(encoded_size(1080, 1920, None), (1920, 1080));
        assert_eq!(encoded_size(1440, 2560, None), (1920, 1080));
        assert_eq!(encoded_size(480, 640, None), (640, 480));
        assert_eq!(encoded_size(1080, 1920, Some((0, 0))), (1920, 1080));
    }

    #[test]
    fn the_pipeline_encodes_at_the_display_size() -> Result<()> {
        let sample = decoded_preview(1920, 1080, Some((720, 405)))?;
        let info = gst_video::VideoInfo::from_caps(sample.caps().unwrap())?;
        assert_eq!((info.width(), info.height()), (720, 406));
        Ok(())
    }

    /// The JPEG is on disk as soon as it is encoded. Before, it waited in the
    /// appsink for the next camera frame to collect it, so the panel was always
    /// a frame period behind -- and a camera that stopped left the last frame
    /// it delivered unpublished.
    #[test]
    fn a_frame_is_published_without_waiting_for_the_next_one() -> Result<()> {
        gst::init()?;
        let info = gst_video::VideoInfo::builder(gst_video::VideoFormat::Nv12, 640, 480)
            .fps(gst::Fraction::new(30, 1))
            .build()?;
        let buffer = gst::Buffer::from_mut_slice(vec![128u8; info.size()]);
        let path = std::env::temp_dir()
            .join(format!("studio-preview-immediate-{}.jpg", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut preview = Preview::new_at(480, 640, Some((320, 180)), path.clone())?;
        let published = preview.tap();
        preview.offer(&buffer, &info)?;
        published
            .recv_timeout(Duration::from_secs(5))
            .context("the only frame offered was never published")?;
        let jpeg = std::fs::read(&path).context("the JPEG was not on disk")?;
        assert_eq!(&jpeg[..2], &[0xff, 0xd8], "not a JPEG");
        drop(preview);
        assert!(!path.exists(), "dropping the preview must remove its frame");
        Ok(())
    }

    /// A write that fails on the encoder's thread still reaches the frame loop,
    /// which is the only place anything is reported.
    #[test]
    fn a_failed_write_is_reported_by_the_next_offer() -> Result<()> {
        gst::init()?;
        let info = gst_video::VideoInfo::builder(gst_video::VideoFormat::Nv12, 64, 48)
            .fps(gst::Fraction::new(30, 1))
            .build()?;
        let buffer = gst::Buffer::from_mut_slice(vec![128u8; info.size()]);
        let path = std::env::temp_dir()
            .join(format!("studio-preview-missing-{}", std::process::id()))
            .join("preview.jpg");
        let mut preview = Preview::new_at(48, 64, None, path)?;
        let published = preview.tap();
        preview.offer(&buffer, &info)?;
        published.recv_timeout(Duration::from_secs(5))?;
        let error = preview.offer(&buffer, &info).expect_err("the failed write was lost");
        assert!(format!("{error:#}").contains("writing"), "{error:#}");
        Ok(())
    }

    #[test]
    fn preview_preserves_fine_luma_detail() -> Result<()> {
        for (width, height) in [(1280, 720), (1920, 1080)] {
            let sample = decoded_preview(width, height, None)?;
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
            let mut preview = Preview::new_at(480, 640, None, path)?;
            let published = preview.tap();
            let mut pts = gst::ClockTime::ZERO;
            // Irregular arrival timestamps also cover a camera slowing down
            // without renegotiating its nominal FPS. Explicit waits for
            // each encode; there is no sleep to accidentally hide a throttle.
            for index in 0..16 {
                buffer.get_mut().unwrap().set_pts(pts);
                preview.offer(&buffer, &info)?;
                let encoded = published
                    .recv_timeout(Duration::from_secs(5))
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
        let mut preview = Preview::new_at(480, 640, None, path)?;
        let published = preview.tap();
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
            let encoded = published
                .recv_timeout(Duration::from_secs(5))
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
