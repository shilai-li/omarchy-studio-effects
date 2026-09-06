//! Decoding a still image once, into the frame's own NV12 layout.

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;

/// A replacement background, already scaled and converted to match the frames it
/// will stand behind.
///
/// Decoded once at startup rather than per frame. The composite then costs the
/// same as the blur path -- a plane copy instead of a blur -- so switching
/// effects cannot change whether the daemon holds its frame budget.
pub struct Background {
    pub y: Vec<u8>,
    pub uv: Vec<u8>,
    pub y_stride: usize,
    pub uv_stride: usize,
}

impl Background {
    /// Decode `path`, cropped to the frame's aspect and scaled to fill it.
    ///
    /// Cropped, not letterboxed: a background with black bars reads as a broken
    /// video call, whereas losing the edges of someone's wallpaper does not.
    pub fn load(path: &str, width: u32, height: u32) -> Result<Self> {
        let desc = format!(
            "filesrc location=\"{path}\" ! decodebin ! videoconvert \
             ! aspectratiocrop aspect-ratio={width}/{height} ! videoscale \
             ! video/x-raw,format=NV12,width={width},height={height} \
             ! appsink name=sink"
        );
        let pipeline = gst::parse::launch(&desc)
            .with_context(|| format!("building a decode pipeline for {path}"))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow::anyhow!("background pipeline was not a pipeline"))?;
        let sink = pipeline
            .by_name("sink")
            .context("no appsink")?
            .downcast::<gst_app::AppSink>()
            .map_err(|_| anyhow::anyhow!("sink was not an appsink"))?;

        pipeline.set_state(gst::State::Playing)?;
        let sample = sink
            .pull_sample()
            .with_context(|| format!("decoding {path}; is it an image GStreamer can read?"))?;

        let info = gst_video::VideoInfo::from_caps(sample.caps().context("no caps")?)?;
        let buffer = sample.buffer().context("no buffer")?;
        let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)?;

        let loaded = Self {
            y: frame.plane_data(0)?.to_vec(),
            uv: frame.plane_data(1)?.to_vec(),
            y_stride: info.stride()[0] as usize,
            uv_stride: info.stride()[1] as usize,
        };

        drop(frame);
        pipeline.set_state(gst::State::Null)?;
        Ok(loaded)
    }

    /// Copy into a frame's planes row by row.
    ///
    /// Row by row because the decoder and the camera pad their rows
    /// independently; a flat copy silently shears the image when they differ.
    pub fn paint(
        &self,
        y_out: &mut [u8],
        uv_out: &mut [u8],
        w: usize,
        h: usize,
        y_stride: usize,
        uv_stride: usize,
    ) {
        for row in 0..h {
            let (src, dst) = (row * self.y_stride, row * y_stride);
            y_out[dst..dst + w].copy_from_slice(&self.y[src..src + w]);
        }
        for row in 0..h / 2 {
            let (src, dst) = (row * self.uv_stride, row * uv_stride);
            uv_out[dst..dst + w].copy_from_slice(&self.uv[src..src + w]);
        }
    }
}
