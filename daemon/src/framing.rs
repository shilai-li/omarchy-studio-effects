//! Deciding where to crop so the subject stays centred.
//!
//! The hard part is not finding the subject -- the mask already did that -- it
//! is moving the crop as little as possible. A frame that tracks every twitch
//! is worse than one that never moves: the viewer sees the room sliding around
//! behind a subject who appears pinned in place, which reads as a broken camera
//! rather than as framing. Everything here exists to keep the crop still.

use crate::nv12::{subject_box, NET};

/// A crop, in source pixels.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    fn centre(&self) -> (f32, f32) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
}

pub struct Framing {
    /// Where the crop is now. `None` until a subject has ever been seen, so the
    /// first frame does not slide in from the whole frame.
    current: Option<Rect>,
    width: f32,
    height: f32,
    /// Largest allowed crop-in. Beyond this the picture is visibly soft, since
    /// the crop is scaled back up to the output size.
    max_zoom: f32,
    /// Fraction of the current crop the subject may drift before the camera
    /// moves at all.
    dead_zone: f32,
    /// How much of the current crop to keep each frame, 0..1. High is slow.
    smoothing: f32,
}

impl Framing {
    pub fn new(width: u32, height: u32, max_zoom: f32, dead_zone: f32, smoothing: f32) -> Self {
        Self {
            current: None,
            width: width as f32,
            height: height as f32,
            max_zoom: max_zoom.clamp(1.0, 3.0),
            dead_zone: dead_zone.clamp(0.0, 0.5),
            smoothing: smoothing.clamp(0.0, 0.99),
        }
    }

    /// The crop this frame should use, or `None` to use the whole frame.
    pub fn update(&mut self, mask: &[f32]) -> Option<Rect> {
        // A subject who has stepped out of shot leaves the camera where it is.
        // Falling back to the full frame would make walking out of view a hard
        // zoom-out, and walking back in a hard zoom-in.
        let Some(target) = self.target_for(mask) else {
            return self.current;
        };

        let Some(current) = self.current else {
            // First sight: land on the target rather than easing from nowhere.
            self.current = Some(target);
            return self.current;
        };

        // Inside the dead zone the camera does not move at all. Easing toward a
        // target that is already close is what produces a slow permanent drift.
        let (cx, cy) = current.centre();
        let (tx, ty) = target.centre();
        let moved = ((tx - cx).abs() / current.w).max((ty - cy).abs() / current.h);
        let resized = (target.w - current.w).abs() / current.w;
        if moved < self.dead_zone && resized < self.dead_zone {
            return self.current;
        }

        let k = self.smoothing;
        let eased = Rect {
            x: current.x * k + target.x * (1.0 - k),
            y: current.y * k + target.y * (1.0 - k),
            w: current.w * k + target.w * (1.0 - k),
            h: current.h * k + target.h * (1.0 - k),
        };
        self.current = Some(eased);
        self.current
    }

    /// Where the crop would ideally sit for this mask.
    fn target_for(&self, mask: &[f32]) -> Option<Rect> {
        // A minimum run of 4 keeps a stray confident pixel from being a subject.
        let (left, top, right, bottom) = subject_box(mask, 0.5, 4)?;

        let sx = self.width / NET as f32;
        let sy = self.height / NET as f32;
        let (l, r) = (left as f32 * sx, (right + 1) as f32 * sx);
        let (t, b) = (top as f32 * sy, (bottom + 1) as f32 * sy);
        let (bw, bh) = (r - l, b - t);
        if bw <= 0.0 || bh <= 0.0 {
            return None;
        }

        // Headroom above and shoulders below: framing a person tight to their
        // silhouette looks like a hostage video. The subject sits slightly
        // above centre, which is where a person expects to be in shot.
        let want_w = bw * 1.9;
        let want_h = bh * 1.5;

        // Never crop in further than max_zoom, and never wider than the frame.
        let min_w = self.width / self.max_zoom;
        let min_h = self.height / self.max_zoom;
        let mut w = want_w.max(min_w).min(self.width);
        let mut h = want_h.max(min_h).min(self.height);

        // Match the output's aspect, growing rather than cutting: the crop is
        // scaled back to the output size, so a mismatched aspect would stretch
        // the subject.
        let aspect = self.width / self.height;
        if w / h > aspect {
            h = (w / aspect).min(self.height);
            w = h * aspect;
        } else {
            w = (h * aspect).min(self.width);
            h = w / aspect;
        }

        let cx = (l + r) / 2.0;
        let cy = (t + b) / 2.0 - h * 0.06;

        Some(Rect {
            x: (cx - w / 2.0).clamp(0.0, self.width - w),
            y: (cy - h / 2.0).clamp(0.0, self.height - h),
            w,
            h,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A blob in mask coordinates.
    fn mask_with(left: usize, top: usize, right: usize, bottom: usize) -> Vec<f32> {
        let mut m = vec![0.0f32; NET * NET];
        for y in top..=bottom {
            for x in left..=right {
                m[y * NET + x] = 1.0;
            }
        }
        m
    }

    fn framing() -> Framing {
        Framing::new(1280, 720, 1.6, 0.06, 0.92)
    }

    #[test]
    fn the_first_sighting_lands_rather_than_easing_in() {
        let mut f = framing();
        let r = f.update(&mask_with(100, 80, 150, 200)).expect("a crop");
        // Centred on the subject, not on the frame.
        let (cx, _) = r.centre();
        assert!(cx < 1280.0 / 2.0, "should sit left of centre, got {cx}");
    }

    /// The whole point. A subject shifting slightly must not move the camera.
    #[test]
    fn a_small_movement_does_not_move_the_camera() {
        let mut f = framing();
        let first = f.update(&mask_with(100, 80, 150, 200)).unwrap();
        let after = f.update(&mask_with(101, 80, 151, 200)).unwrap();
        assert_eq!(first, after, "a one-pixel drift moved the frame");
    }

    /// A real move must be followed, and gradually.
    #[test]
    fn a_large_movement_is_followed_slowly() {
        let mut f = framing();
        let start = f.update(&mask_with(40, 80, 90, 200)).unwrap();
        let step = f.update(&mask_with(160, 80, 210, 200)).unwrap();
        assert!(step.x > start.x, "should move toward the subject");

        // Slowly: one frame must cover only a small part of the distance.
        let mut settled = step;
        for _ in 0..200 {
            settled = f.update(&mask_with(160, 80, 210, 200)).unwrap();
        }
        let travelled = (step.x - start.x).abs();
        let total = (settled.x - start.x).abs();
        assert!(travelled < total * 0.25,
                "one frame covered {travelled} of {total} -- too fast");
    }

    /// Losing the subject must hold the frame, not snap back to the full view.
    #[test]
    fn losing_the_subject_holds_the_last_frame() {
        let mut f = framing();
        let held = f.update(&mask_with(100, 80, 150, 200)).unwrap();
        let empty = f.update(&vec![0.0f32; NET * NET]).unwrap();
        assert_eq!(held, empty);
    }

    /// The crop is scaled back to the output, so a wrong aspect stretches faces.
    #[test]
    fn the_crop_keeps_the_output_aspect() {
        let mut f = framing();
        for blob in [(100, 80, 150, 200), (10, 10, 240, 240), (120, 120, 130, 140)] {
            let r = f.update(&mask_with(blob.0, blob.1, blob.2, blob.3)).unwrap();
            let got = r.w / r.h;
            assert!((got - 1280.0 / 720.0).abs() < 0.01, "aspect {got} for {blob:?}");
        }
    }

    /// Never crop past the edges, whatever the subject does.
    #[test]
    fn the_crop_stays_inside_the_frame() {
        let mut f = framing();
        for blob in [(0, 0, 20, 20), (235, 235, 255, 255), (0, 100, 255, 200)] {
            let r = f.update(&mask_with(blob.0, blob.1, blob.2, blob.3)).unwrap();
            assert!(r.x >= -0.01 && r.y >= -0.01, "{r:?} starts outside");
            assert!(r.x + r.w <= 1280.01 && r.y + r.h <= 720.01, "{r:?} ends outside");
        }
    }

    /// Zooming in past the limit turns a 720p call into a soft mess.
    #[test]
    fn zoom_is_capped() {
        let mut f = framing();
        // A tiny subject would otherwise ask for an enormous crop-in.
        let r = f.update(&mask_with(126, 126, 130, 130)).unwrap();
        assert!(r.w >= 1280.0 / 1.6 - 1.0, "cropped to {}, past the cap", r.w);
    }
}
