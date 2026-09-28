//! A small JPEG of the composited frame, for the bar widget to show.
//!
//! The widget cannot simply open "Studio Camera" and watch it. v4l2loopback
//! allows ten openers, but the second one's REQBUFS invalidates the first's
//! buffer pool: measured here, a second reader joining broke the reader already
//! streaming with "Failed to allocate a buffer". A preview that competes for
//! the device would kill the video call it is previewing, and the call is the
//! thing that matters.
//!
//! So the frames leave by a side channel instead. The daemon already holds
//! every composited frame, and writing a downscaled JPEG somewhere the widget
//! can read costs a fraction of a millisecond and contends with nothing.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;

/// Preview width. The widget shows this a few centimetres wide, so anything
/// larger is encoded and scaled away again for nothing.
const WIDTH: i32 = 320;

/// Ten a second. Enough to see yourself move and frame a shot, and slow enough
/// that the encode never competes with the frame budget it is sampled from.
const INTERVAL: Duration = Duration::from_millis(100);

/// Offers in a row with nothing coming out before the encoder counts as
/// stuck: two seconds. Nothing coming out of one offer is normal now -- the
/// JPEG is collected on the next -- so a single empty pull says nothing.
const STALLED: u32 = 20;

pub struct Preview {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
    sink: gst_app::AppSink,
    path: PathBuf,
    temp: PathBuf,
    last: Option<Instant>,
    /// Offers since the encoder last produced anything.
    unanswered: u32,
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
        // Height follows the source's aspect, rounded to even for the encoder.
        let scaled_height = ((WIDTH as i64 * height as i64 / width.max(1) as i64) as i32).max(2) & !1;

        // Scaled first and never converted: jpegenc takes NV12 as it is. The
        // obvious `videoconvert ! videoscale` converts the whole frame to I420
        // before throwing most of it away -- more than half the encode's cost
        // at 1080p, 2.7 ms of CPU against 1.2.
        let desc = format!(
            "appsrc name=src format=time is-live=true \
             ! videoscale ! video/x-raw,width={WIDTH},height={scaled_height} \
             ! jpegenc quality=70 ! appsink name=sink max-buffers=1 drop=true sync=false"
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

        let path = Self::path_for_runtime();
        let temp = path.with_extension("jpg.tmp");
        Ok(Self {
            pipeline,
            src,
            sink,
            path,
            temp,
            last: None,
            unanswered: 0,
        })
    }

    /// Offer a composited frame. Encodes at most one per interval.
    ///
    /// Never waits for the encoder. What gets written is the JPEG the encoder
    /// finished since the last offer, collected on the way in, and this frame
    /// is handed over to be ready by the next one -- so the preview runs one
    /// interval behind, and the frame loop does not stop for it. Waiting for
    /// each JPEG, which is what this did first, held the loop 1.1 ms at 720p
    /// and 2.8 ms at 1080p on every third frame the panel was open -- a late
    /// frame on the call, for a picture a tenth of a second fresher. It holds
    /// it 0.2 ms now. `cargo run --release --example preview_cost` measures it.
    ///
    /// Errors are returned rather than propagated into the frame loop by the
    /// caller: a preview that fails is a widget with a stale picture, which is
    /// not a reason to drop somebody's video call.
    pub fn offer(&mut self, buffer: &gst::Buffer, info: &gst_video::VideoInfo) -> Result<()> {
        let now = Instant::now();
        if self.last.is_some_and(|t| now.duration_since(t) < INTERVAL) {
            return Ok(());
        }
        self.last = Some(now);

        let finished = self.sink.try_pull_sample(gst::ClockTime::ZERO);

        if self.src.caps().is_none() {
            self.src.set_caps(Some(&info.to_caps()?));
        }
        self.src
            .push_buffer(buffer.copy())
            .map_err(|e| anyhow::anyhow!("pushing a preview frame: {e:?}"))?;

        let Some(sample) = finished else {
            // Said once, not every interval: a broken encoder would otherwise
            // fill the journal ten lines a second.
            self.unanswered += 1;
            if self.unanswered == STALLED {
                anyhow::bail!("the preview encoder has produced nothing for {:?}", INTERVAL * STALLED);
            }
            return Ok(());
        };
        self.unanswered = 0;
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

impl Drop for Preview {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        // Leaving the last frame behind would let a widget show a picture of a
        // camera that is no longer running.
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(&self.temp);
    }
}
