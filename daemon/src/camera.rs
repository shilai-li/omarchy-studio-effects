//! What the physical camera can actually do, and which of it to use.
//!
//! The daemon used to be told a size and a frame rate and to hope. A camera
//! without that size was opened at its largest mode and scaled -- stretched, if
//! the shapes differed -- and one without that rate refused to start at all, so
//! no default could be right for every machine. Now the camera is asked what it
//! has, and a size and a rate are a ceiling to pick under, not a demand.

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;

/// Modes slower than this are only chosen when the camera has nothing else.
/// Cameras offer uncompressed 1080p at 3-5 fps because a USB 2 link cannot
/// carry more, and a bigger picture at that rate is not a better camera.
const USABLE_FPS: f64 = 15.0;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Media {
    Jpeg,
    Raw,
}

/// One thing the camera can deliver.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Mode {
    pub media: Media,
    pub width: u32,
    pub height: u32,
    /// numerator, denominator -- exactly as the camera states it, because caps
    /// are compared as fractions and 30000/1001 is not 30.
    pub rate: (i32, i32),
}

impl Mode {
    pub fn fps(&self) -> f64 {
        f64::from(self.rate.0) / f64::from(self.rate.1.max(1))
    }

    /// The caps to hold the source to, so it opens exactly this.
    pub fn caps(&self) -> String {
        let kind = match self.media {
            Media::Jpeg => "image/jpeg",
            Media::Raw => "video/x-raw",
        };
        format!(
            "{kind},width={},height={},framerate={}/{}",
            self.width, self.height, self.rate.0, self.rate.1
        )
    }
}

/// Ask a camera for its modes. Opens the device only as far as READY, which is
/// enough to enumerate and starts nothing, so the recording light stays off.
pub fn probe(device: &str) -> Result<Vec<Mode>> {
    let src = gst::ElementFactory::make("v4l2src")
        .property("device", device)
        .build()
        .context("creating v4l2src")?;
    src.set_state(gst::State::Ready)
        .with_context(|| format!("opening {device}"))?;
    let caps = src.static_pad("src").map(|pad| pad.query_caps(None));
    let _ = src.set_state(gst::State::Null);
    Ok(caps.map(|c| modes_from_caps(&c)).unwrap_or_default())
}

/// The modes a caps describes: one per size and rate, for the two kinds this
/// daemon can decode.
///
/// Raw modes the daemon cannot use are left out: `DMA_DRM` is the same YUYV a
/// second time, described for GPU-memory pipelines, and appears beside every
/// real one. Sizes and rates given as ranges, which stepwise cameras do, are
/// taken at their upper bound and at the usual rates inside them.
pub fn modes_from_caps(caps: &gst::CapsRef) -> Vec<Mode> {
    let mut modes = Vec::new();
    for s in caps.iter() {
        let media = match s.name().as_str() {
            "image/jpeg" => Media::Jpeg,
            "video/x-raw" => {
                if s.get::<&str>("format").is_ok_and(|f| f == "DMA_DRM") {
                    continue;
                }
                Media::Raw
            }
            _ => continue,
        };
        // Every combination: GStreamer folds sizes that share a width and a
        // rate into one structure with a list -- `height=(int){ 480, 360 }` --
        // so a camera's 640x480 and 640x360 arrive as a single entry, and
        // reading only single numbers loses both.
        for width in dimension(s, "width") {
            for height in dimension(s, "height") {
                for rate in rates(s) {
                    let mode = Mode { media, width, height, rate };
                    if !modes.contains(&mode) {
                        modes.push(mode);
                    }
                }
            }
        }
    }
    modes
}

fn dimension(s: &gst::StructureRef, field: &str) -> Vec<u32> {
    if let Ok(v) = s.get::<i32>(field) {
        return u32::try_from(v).into_iter().collect();
    }
    if let Ok(list) = s.get::<gst::List>(field) {
        return list.iter().filter_map(|v| v.get::<i32>().ok()).filter_map(|v| u32::try_from(v).ok()).collect();
    }
    s.get::<gst::IntRange<i32>>(field).ok().and_then(|r| u32::try_from(r.max()).ok()).into_iter().collect()
}

fn rates(s: &gst::StructureRef) -> Vec<(i32, i32)> {
    if let Ok(f) = s.get::<gst::Fraction>("framerate") {
        return vec![(f.numer(), f.denom())];
    }
    if let Ok(list) = s.get::<gst::List>("framerate") {
        return list
            .iter()
            .filter_map(|v| v.get::<gst::Fraction>().ok())
            .map(|f| (f.numer(), f.denom()))
            .collect();
    }
    if let Ok(r) = s.get::<gst::FractionRange>("framerate") {
        let (lo, hi) = (r.min(), r.max());
        let (lo_f, hi_f) = (f64::from(lo.numer()) / f64::from(lo.denom()), f64::from(hi.numer()) / f64::from(hi.denom()));
        let mut found: Vec<(i32, i32)> = [120, 60, 50, 30, 25, 24, 20, 15, 10]
            .into_iter()
            .filter(|&n| f64::from(n) >= lo_f && f64::from(n) <= hi_f)
            .map(|n| (n, 1))
            .collect();
        found.push((hi.numer(), hi.denom()));
        return found;
    }
    Vec::new()
}

/// The best mode at or under a size and a rate.
///
/// In order of what matters to a picture people look at:
///
/// 1. A size that fits under `max_w` x `max_h`, and is the biggest that does --
///    unless every mode at that size is slow (under 15 fps, or under the
///    ceiling if that is lower), when the biggest size that is not slow wins.
///    A camera that offers 1080p only at 3 fps is a 720p camera.
/// 2. The highest rate at that size that does not exceed `max_fps`.
/// 3. Uncompressed over MJPEG at a tie, since it needs no decode.
///
/// Nothing fits under the size -- a camera whose smallest mode is bigger than
/// asked for -- takes the smallest mode there is. Odd sizes are never picked:
/// NV12 chroma is half the size, and an odd frame has no half.
pub fn pick(modes: &[Mode], max_w: u32, max_h: u32, max_fps: u32) -> Option<Mode> {
    let usable: Vec<&Mode> = modes
        .iter()
        .filter(|m| m.width > 0 && m.height > 0 && m.width % 2 == 0 && m.height % 2 == 0 && m.rate.0 > 0)
        .collect();
    let area = |m: &Mode| u64::from(m.width) * u64::from(m.height);

    let fitting: Vec<&Mode> = usable.iter().copied().filter(|m| m.width <= max_w && m.height <= max_h).collect();
    let pool = if fitting.is_empty() {
        // Smaller than anything the camera does: the least oversized.
        let smallest = usable.iter().map(|m| area(m)).min()?;
        usable.iter().copied().filter(|m| area(m) == smallest).collect()
    } else {
        fitting
    };

    let fast_enough = USABLE_FPS.min(f64::from(max_fps));
    let viable: Vec<&Mode> = pool.iter().copied().filter(|m| m.fps() >= fast_enough).collect();
    let pool = if viable.is_empty() { pool } else { viable };

    let biggest = pool.iter().map(|m| area(m)).max()?;
    let at_size: Vec<&Mode> = pool.into_iter().filter(|m| area(m) == biggest).collect();

    let ceiling = f64::from(max_fps);
    let under: Vec<Mode> = at_size.iter().map(|m| **m).filter(|m| m.fps() <= ceiling + 1e-6).collect();
    if under.is_empty() {
        // Everything is faster than the ceiling: the slowest, the closest to it.
        at_size.iter().map(|m| **m).min_by(|a, b| a.fps().total_cmp(&b.fps()))
    } else {
        // Fastest under it; raw over MJPEG when they tie.
        under
            .into_iter()
            .max_by(|a, b| a.fps().total_cmp(&b.fps()).then((a.media == Media::Raw).cmp(&(b.media == Media::Raw))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn caps(text: &str) -> gst::Caps {
        gst::init().unwrap();
        gst::Caps::from_str(text).unwrap()
    }

    fn jpeg(w: u32, h: u32, fps: i32) -> Mode {
        Mode { media: Media::Jpeg, width: w, height: h, rate: (fps, 1) }
    }

    fn raw(w: u32, h: u32, fps: i32) -> Mode {
        Mode { media: Media::Raw, width: w, height: h, rate: (fps, 1) }
    }

    /// This machine's USB camera, as it reports itself: MJPEG up to 1080p30,
    /// and uncompressed YUYV that is slower at every size.
    fn usb_camera() -> Vec<Mode> {
        let mut m = vec![jpeg(1920, 1080, 30), jpeg(1280, 720, 30), jpeg(960, 540, 30), jpeg(848, 480, 30),
                         jpeg(640, 480, 30), jpeg(640, 360, 30)];
        m.extend([raw(1920, 1080, 3), raw(1280, 720, 10), raw(960, 540, 15), raw(848, 480, 15),
                  raw(640, 480, 30), raw(640, 360, 30)]);
        m
    }

    #[test]
    fn modes_are_read_out_of_caps() {
        let modes = modes_from_caps(&caps(
            "video/x-raw, format=(string)YUY2, width=(int)1280, height=(int)720, framerate=(fraction)10/1; \
             video/x-raw, format=(string)DMA_DRM, drm-format=(string)YUYV, width=(int)1280, height=(int)720, framerate=(fraction)10/1; \
             image/jpeg, width=(int)1920, height=(int)1080, framerate=(fraction){ 30/1, 15/1 }; \
             video/x-h264, width=(int)1920, height=(int)1080, framerate=(fraction)30/1",
        ));
        assert_eq!(modes, [raw(1280, 720, 10), jpeg(1920, 1080, 30), jpeg(1920, 1080, 15)],
                   "DMA_DRM duplicates and codecs the daemon cannot decode are left out");
    }

    /// This laptop's camera, verbatim. Its 640-wide modes come as one entry with
    /// a list of heights, and the first version of the parser lost both -- so a
    /// request for 640x480 quietly got 848x480, wider than asked for.
    #[test]
    fn sizes_folded_into_a_list_are_all_read() {
        let modes = modes_from_caps(&caps(
            "video/x-raw, format=(string)DMA_DRM, drm-format=(string)YUYV, width=(int)640, height=(int)\
             { 480, 360 }, framerate=(fraction)30/1; \
             image/jpeg, parsed=(boolean)true, width=(int)640, height=(int){ 480, 360 }, framerate=(fraction)30/1; \
             image/jpeg, parsed=(boolean)true, width=(int)848, height=(int)480, framerate=(fraction)30/1; \
             video/x-raw, format=(string)YUY2, width=(int)640, height=(int){ 480, 360 }, framerate=(fraction)30/1",
        ));
        assert_eq!(modes, [jpeg(640, 480, 30), jpeg(640, 360, 30), jpeg(848, 480, 30),
                           raw(640, 480, 30), raw(640, 360, 30)]);
        assert_eq!(pick(&modes, 640, 480, 60), Some(raw(640, 480, 30)), "the size asked for, not a wider one");
    }

    #[test]
    fn ranges_are_read_at_their_top_and_the_usual_rates() {
        let modes = modes_from_caps(&caps(
            "image/jpeg, width=(int)[ 160, 1920 ], height=(int)[ 120, 1080 ], framerate=(fraction)[ 1/1, 60/1 ]",
        ));
        assert!(modes.contains(&jpeg(1920, 1080, 60)) && modes.contains(&jpeg(1920, 1080, 30)), "{modes:?}");
    }

    /// The camera in this laptop, and the reason for all of it.
    #[test]
    fn a_1080p_camera_gets_1080p_at_its_best_rate() {
        assert_eq!(pick(&usb_camera(), 1920, 1080, 60), Some(jpeg(1920, 1080, 30)));
    }

    /// A camera without 1080p is not asked for it, and not scaled up to it.
    #[test]
    fn a_camera_without_1080p_gets_what_it_has() {
        let hd = [jpeg(1280, 720, 30), jpeg(640, 480, 30)];
        assert_eq!(pick(&hd, 1920, 1080, 60), Some(jpeg(1280, 720, 30)));
        let vga = [jpeg(640, 480, 30), jpeg(320, 240, 30)];
        assert_eq!(pick(&vga, 1920, 1080, 60), Some(jpeg(640, 480, 30)), "4:3 stays 4:3");
    }

    /// Uncompressed 1080p at 3 fps is what a USB 2 camera offers beside its
    /// real modes. It must not win on size alone.
    #[test]
    fn a_slow_big_mode_does_not_beat_a_usable_smaller_one() {
        let camera = [raw(1920, 1080, 5), jpeg(1280, 720, 30), raw(1280, 720, 10)];
        assert_eq!(pick(&camera, 1920, 1080, 60), Some(jpeg(1280, 720, 30)));
    }

    /// ...but a camera with nothing fast still gets its biggest, rather than
    /// no camera at all.
    #[test]
    fn a_camera_that_is_slow_everywhere_still_works() {
        let camera = [raw(1280, 720, 10), raw(640, 480, 10)];
        assert_eq!(pick(&camera, 1920, 1080, 60), Some(raw(1280, 720, 10)));
    }

    /// The rate is a ceiling. A camera that has 60 gets it; one that has more
    /// than asked for is held to what was asked.
    #[test]
    fn the_rate_is_a_ceiling_not_a_demand() {
        let camera = [jpeg(1920, 1080, 60), jpeg(1920, 1080, 30), jpeg(3840, 2160, 30)];
        assert_eq!(pick(&camera, 1920, 1080, 60), Some(jpeg(1920, 1080, 60)));
        assert_eq!(pick(&camera, 1920, 1080, 30), Some(jpeg(1920, 1080, 30)));
        assert_eq!(pick(&camera, 1920, 1080, 24), Some(jpeg(1920, 1080, 30)),
                   "nothing at or under 24: the slowest there is, not a refusal");
    }

    /// A 4K camera is asked for 1080p: four times the pixels to decode and
    /// blur for a picture no call transmits.
    #[test]
    fn a_4k_camera_is_held_to_1080p() {
        let camera = [jpeg(3840, 2160, 30), jpeg(2560, 1440, 30), jpeg(1920, 1080, 30)];
        assert_eq!(pick(&camera, 1920, 1080, 60), Some(jpeg(1920, 1080, 30)));
    }

    /// Asking for 720p on a 1080p camera means 720p.
    #[test]
    fn an_explicit_smaller_size_is_honoured() {
        assert_eq!(pick(&usb_camera(), 1280, 720, 60), Some(jpeg(1280, 720, 30)));
    }

    /// Nothing at or under the size: the least oversized mode, not nothing.
    #[test]
    fn a_camera_bigger_than_asked_for_uses_its_smallest() {
        let camera = [jpeg(1280, 720, 30), jpeg(1920, 1080, 30)];
        assert_eq!(pick(&camera, 640, 480, 60), Some(jpeg(1280, 720, 30)));
    }

    /// Raw needs no decode, so at the same size and rate it wins.
    #[test]
    fn raw_beats_mjpeg_at_a_tie() {
        assert_eq!(pick(&[jpeg(1280, 720, 30), raw(1280, 720, 30)], 1920, 1080, 60), Some(raw(1280, 720, 30)));
        assert_eq!(pick(&[raw(1280, 720, 30), jpeg(1280, 720, 30)], 1920, 1080, 60), Some(raw(1280, 720, 30)));
    }

    /// NV12's chroma is half size in both directions.
    #[test]
    fn odd_sizes_are_never_picked() {
        let camera = [jpeg(1279, 719, 30), jpeg(640, 480, 30)];
        assert_eq!(pick(&camera, 1920, 1080, 60), Some(jpeg(640, 480, 30)));
    }

    /// A second real camera, read off another laptop (TM1709, a 2017 i5 with no
    /// NPU): a XiaoMi USB webcam that tops out at 720p, whose uncompressed 720p
    /// is 10 fps and whose small modes are 30. It is asked for 1080p by default
    /// and must come back with 720p MJPEG at 30, not a slow bigger mode or a
    /// tiny fast one.
    #[test]
    fn a_720p_webcam_from_another_laptop_gets_720p30() {
        let mut modes = vec![jpeg(1280, 720, 30), jpeg(640, 480, 30), jpeg(320, 240, 30), jpeg(160, 120, 30)];
        modes.extend([raw(1280, 720, 10), raw(640, 480, 30), raw(320, 240, 30), raw(160, 120, 30)]);
        assert_eq!(pick(&modes, 1920, 1080, 60), Some(jpeg(1280, 720, 30)));
        assert_eq!(pick(&modes, 640, 480, 60), Some(raw(640, 480, 30)), "4:3 when 4:3 is asked for");
    }

    #[test]
    fn a_camera_with_no_usable_modes_yields_nothing() {
        assert_eq!(pick(&[], 1920, 1080, 60), None);
        assert_eq!(pick(&[jpeg(1279, 719, 30)], 1920, 1080, 60), None);
    }

    /// NTSC rates are not 30, and the source has to be asked for the fraction
    /// it advertised or it will not negotiate.
    #[test]
    fn fractional_rates_are_kept_exactly() {
        let m = Mode { media: Media::Jpeg, width: 1280, height: 720, rate: (30000, 1001) };
        assert_eq!(m.caps(), "image/jpeg,width=1280,height=720,framerate=30000/1001");
        assert_eq!(pick(&[m], 1920, 1080, 60), Some(m));
    }
}
