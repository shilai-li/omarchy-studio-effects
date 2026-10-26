//! Conditioning the raw model output before it is used as an alpha.
//!
//! The model emits a per-pixel probability, and using it directly as alpha is
//! what produced the ghosting: a fast-moving hand is motion-blurred by the
//! camera, the model is genuinely unsure about it, and a mask of 0.5 composites
//! the hand half-way into its own blurred copy. A viewer does not read that as
//! uncertainty, they read it as the hand having gone transparent.
//!
//! Two filters fix it, in this order.

pub struct MaskFilter {
    previous: Vec<f32>,
    scratch: Vec<f32>,
    have_previous: bool,
    gain: f32,
    smoothing: f32,
    /// When set, the smoothing fades out with how much a pixel changed: a
    /// change this large or larger keeps nothing of the previous frame.
    motion: Option<f32>,
}

impl MaskFilter {
    /// `gain` steepens the probability curve; `smoothing` is the weight kept
    /// from the previous frame, in 0..1; `len` is the mask's size in values.
    ///
    /// Give a recurrent model no smoothing. It carries its own memory between
    /// frames, so this blend smooths a second time, and the lag it adds is a
    /// stepped ghost trailing behind a waving hand -- the hand's own position
    /// only half covered, and the room beside it half sharp.
    pub fn new(gain: f32, smoothing: f32, len: usize) -> Self {
        Self {
            previous: vec![0.0; len],
            scratch: vec![0.0; len],
            have_previous: false,
            gain: gain.max(1.0),
            smoothing: smoothing.clamp(0.0, 0.95),
            motion: None,
        }
    }

    /// Smoothing that holds a still edge steady and lets a moving one go: each
    /// pixel keeps `smoothing` of the previous frame scaled down by how much it
    /// changed, to nothing at a change of `threshold`.
    ///
    /// For the matting model. Its edges shimmer frame to frame even with its
    /// own memory -- on a recorded clip the outline of a still head moved 0.028
    /// a frame on average -- and the plain blend that would steady them trails
    /// ghosts behind a waving hand. Jitter is a small change and a moving hand
    /// a large one, so this tells them apart.
    ///
    /// It smooths the model's raw output and steepens afterwards, the other
    /// way round from the plain blend. Smoothing the steepened mask left faint
    /// outlines of a moving hand behind it: a pixel fading from 0.3 to 0 is a
    /// small change, so it was held and decayed over several frames, an echo of
    /// every edge the hand had passed. Steepening after sends those values to
    /// 0. On the clip, at 0.95 and 0.7: still-edge jitter 0.015 a frame (the old
    /// 256x256 build with the plain blend: 0.024), and faint values left of the
    /// head on 0.3% of pixels, against 1.8% the other way round.
    pub fn motion_adaptive(gain: f32, smoothing: f32, threshold: f32, len: usize) -> Self {
        Self { motion: Some(threshold.max(1e-3)), ..Self::new(gain, smoothing, len) }
    }

    /// Steepen the model's probability around the halfway mark.
    ///
    /// Pushes confident pixels to a solid 0 or 1 while leaving a narrow band of
    /// genuine blending at the silhouette, which is what keeps hair looking like
    /// hair. A hard threshold would do the first part and ruin the second.
    #[inline]
    fn contrast(&self, p: f32) -> f32 {
        let t = ((p - 0.5) * self.gain + 0.5).clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t) // smoothstep
    }

    /// Condition one frame's mask, in place.
    pub fn apply(&mut self, mask: &mut [f32]) {
        if let Some(threshold) = self.motion {
            self.apply_motion_adaptive(mask, threshold);
            return;
        }
        for (i, p) in mask.iter().enumerate() {
            self.scratch[i] = self.contrast(*p);
        }

        // Blend toward the previous frame to stop edges shimmering while the
        // subject holds still. Kept modest by default: this is a lag, and too
        // much of it smears the silhouette behind anyone who moves quickly --
        // trading the flicker for a worse version of the problem above.
        if self.have_previous && self.smoothing > 0.0 {
            let keep = self.smoothing;
            for (s, prev) in self.scratch.iter_mut().zip(&self.previous) {
                *s = prev * keep + *s * (1.0 - keep);
            }
        }

        self.previous.copy_from_slice(&self.scratch);
        self.have_previous = true;
        mask.copy_from_slice(&self.scratch);
    }

    /// Smooth the raw probability by how little it moved, then steepen it.
    /// `previous` holds the smoothed raw values, not the steepened ones.
    fn apply_motion_adaptive(&mut self, mask: &mut [f32], threshold: f32) {
        if self.have_previous && self.smoothing > 0.0 {
            let keep = self.smoothing;
            for (p, prev) in mask.iter_mut().zip(&self.previous) {
                let k = keep * (1.0 - (*p - prev).abs() / threshold).max(0.0);
                *p = prev * k + *p * (1.0 - k);
            }
        }
        self.previous.copy_from_slice(mask);
        self.have_previous = true;
        for p in mask.iter_mut() {
            *p = self.contrast(*p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nv12::NET;

    fn filtered(gain: f32, smoothing: f32, values: &[f32]) -> Vec<f32> {
        let mut f = MaskFilter::new(gain, smoothing, NET * NET);
        let mut buf = vec![0.0; NET * NET];
        buf[..values.len()].copy_from_slice(values);
        f.apply(&mut buf);
        buf[..values.len()].to_vec()
    }

    /// Confident pixels must come out fully solid. This is the ghosting fix: a
    /// 0.8 probability has to composite as opaque, not as 80% opaque.
    #[test]
    fn confident_pixels_saturate() {
        let out = filtered(3.0, 0.0, &[0.0, 0.15, 0.85, 1.0]);
        assert_eq!(out[0], 0.0);
        assert!(out[1] < 0.05, "0.15 should read as background, got {}", out[1]);
        assert!(out[2] > 0.95, "0.85 should read as foreground, got {}", out[2]);
        assert_eq!(out[3], 1.0);
    }

    /// The silhouette must keep a soft band, or hair turns into a cut-out.
    #[test]
    fn the_uncertain_middle_stays_soft() {
        let out = filtered(3.0, 0.0, &[0.45, 0.5, 0.55]);
        assert!(out[0] > 0.0 && out[0] < 0.5, "got {}", out[0]);
        assert_eq!(out[1], 0.5);
        assert!(out[2] > 0.5 && out[2] < 1.0, "got {}", out[2]);
    }

    /// Monotonic: a pixel the model is more sure about can never come out less
    /// opaque than one it is less sure about.
    #[test]
    fn conditioning_is_monotonic() {
        let ramp: Vec<f32> = (0..=100).map(|i| i as f32 / 100.0).collect();
        let out = filtered(4.0, 0.0, &ramp);
        for pair in out.windows(2) {
            assert!(pair[1] >= pair[0], "{} then {}", pair[0], pair[1]);
        }
    }

    /// The first frame has nothing to blend with, so it must not be dragged
    /// toward zero -- otherwise effects visibly fade in on every start.
    #[test]
    fn the_first_frame_is_not_blended_with_nothing() {
        let mut f = MaskFilter::new(3.0, 0.9, NET * NET);
        let mut buf = vec![1.0; NET * NET];
        f.apply(&mut buf);
        assert_eq!(buf[0], 1.0);
    }

    /// A large change -- a hand arriving -- passes straight through, where the
    /// plain blend would show it at half strength for a frame and trail after.
    #[test]
    fn motion_adaptive_lets_a_large_change_through() {
        let mut f = MaskFilter::motion_adaptive(3.0, 0.95, 0.7, NET * NET);
        let mut buf = vec![0.0; NET * NET];
        f.apply(&mut buf);
        buf.fill(1.0);
        f.apply(&mut buf);
        assert_eq!(buf[0], 1.0);
    }

    /// A small wobble on a held edge is mostly held back.
    #[test]
    fn motion_adaptive_steadies_a_small_wobble() {
        let mut f = MaskFilter::motion_adaptive(1.0, 0.95, 0.7, NET * NET);
        let mut buf = vec![0.5; NET * NET];
        f.apply(&mut buf);
        buf.fill(0.55);
        f.apply(&mut buf);
        // Unsteadied, 0.55 comes out of the smoothstep at 0.5748, a 0.075
        // step; the filter lets through under a fifth of it.
        assert!(buf[0] > 0.5 && buf[0] < 0.5 + 0.075 / 5.0, "moved to {}", buf[0]);
    }

    /// The echo this order exists to prevent: an edge that has moved on leaves
    /// a pixel fading from part-subject to none. It must not linger as a faint
    /// outline, which smoothing after the curve did for several frames.
    #[test]
    fn motion_adaptive_leaves_no_echo_of_a_passing_edge() {
        let mut f = MaskFilter::motion_adaptive(3.0, 0.95, 0.7, NET * NET);
        let mut buf = vec![0.45; NET * NET];
        f.apply(&mut buf);
        buf.fill(0.0);
        f.apply(&mut buf);
        assert!(buf[0] < 0.02, "an edge that left still shows at {}", buf[0]);
    }

    /// Smoothing must converge, not stall short of the target.
    #[test]
    fn smoothing_converges_on_a_held_value() {
        let mut f = MaskFilter::new(3.0, 0.6, NET * NET);
        let mut buf = vec![0.0; NET * NET];
        f.apply(&mut buf);
        for _ in 0..40 {
            buf.fill(1.0);
            f.apply(&mut buf);
        }
        assert!(buf[0] > 0.99, "converged only to {}", buf[0]);
    }
}
