//! Keeping the subject out of their own background blur.
//!
//! Blurring the whole frame and painting the sharp subject back over it smears
//! the subject into the blur: a skin-coloured glow around a hand, a dark one
//! around hair, as wide as the blur radius. At the radii people actually use
//! (the panel goes to 200) it is the most visible thing that separates this
//! from the effect in a commercial video call, which blurs the background only.
//!
//! Blurring only the background properly means dividing a blur of the weighted
//! frame by a blur of the weights, and in the 8-bit blur that division
//! amplifies rounding wherever the background is scarce -- the gap between two
//! fingers comes out posterised. So instead every subject sample is replaced,
//! before the blur, with an estimate of the background behind it, and the plane
//! is then blurred exactly as before. The estimate is made over small blocks, in
//! floats, where the division costs nothing and its precision does not matter:
//! it only ever stands in for what the subject covers. Compared on a recorded
//! clip of a waving hand, the result matched the true background-only blur.
//!
//! It runs inside `nv12::Blur`, on each plane after it has been shrunk and
//! before it is blurred, because that is the resolution the blur works at:
//! anything finer is averaged away. At 1080p and radius 72 that is 480x270
//! instead of 1920x1080. Done at full size it cost 3.2 ms a frame.

use crate::nv12::{MaskSize, MaskUpscaler};

/// Side of the blocks the estimate is made over, in full-size luma pixels.
/// Coarse on purpose: the estimate only stands in for what the subject hides,
/// whose detail is unknown anyway, and the blur that follows is wider than
/// this whenever the fill runs at all.
const BLOCK: usize = 16;

/// Weight of the plane's average background in every block's estimate, in the
/// same units as the block sums: a quarter of one sample. Enough to give a
/// block with no background in reach a sensible value, too little to move a
/// block that has any.
const PRIOR: f32 = 0.25 * 255.0;

/// What one plane needs kept between frames: the subject's alpha stretched to
/// the plane's width, and each channel's estimate stretched back to it.
struct Slots<const C: usize> {
    alpha: Option<MaskUpscaler>,
    spread: [Option<MaskUpscaler>; C],
}

impl<const C: usize> Default for Slots<C> {
    fn default() -> Self {
        Self { alpha: None, spread: std::array::from_fn(|_| None) }
    }
}

#[derive(Default)]
pub struct Fill {
    luma: Slots<1>,
    chroma: Slots<2>,
    sums: Vec<Vec<f32>>,
    weights: Vec<f32>,
    scratch: Vec<f32>,
    est: Vec<f32>,
    keeps: Vec<u8>,
    isums: Vec<Vec<u32>>,
    iweights: Vec<u32>,
    /// Per row of the plane, the columns `[start, end)` the subject may touch.
    spans: Vec<(usize, usize)>,
}

impl Fill {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fill the subject in a packed luma plane, `pw` x `ph`, each sample
    /// `scale` full-size pixels, ahead of a blur of radius `r` samples. `like`
    /// is the frame's upscaler, prepared with this frame's mask: its aim -- the
    /// part of the mask the output shows -- and its quantised mask are both
    /// taken from it.
    pub fn luma(&mut self, p: &mut [u8], pw: usize, ph: usize, scale: usize, r: usize, like: &MaskUpscaler) {
        let mut slots = std::mem::take(&mut self.luma);
        self.plane::<1>(&mut slots, p, pw, ph, scale, r, like);
        self.luma = slots;
    }

    /// The same for the interleaved chroma plane; `scale` is in full-size luma
    /// pixels, so twice the luma plane's.
    pub fn chroma(&mut self, p: &mut [u8], pw: usize, ph: usize, scale: usize, r: usize, like: &MaskUpscaler) {
        let mut slots = std::mem::take(&mut self.chroma);
        self.plane::<2>(&mut slots, p, pw, ph, scale, r, like);
        self.chroma = slots;
    }

    /// Background samples come back exactly as they were.
    ///
    /// Half what the first version cost -- 1.83 ms to 0.94 at 1080p, radius
    /// 20, P-core -- for the same bytes, which `the_fill_is_what_it_was` holds
    /// it to. The mask is quantised once, by the frame's upscaler, rather than
    /// again here; everything per sample is done only between the first and
    /// last subject column of a row, since a background sample keeps itself;
    /// and the block sums are integers. Those are exact where the floats were
    /// exact too: a block's sum stays far under 2^24.
    #[allow(clippy::too_many_arguments)]
    fn plane<const C: usize>(&mut self, slots: &mut Slots<C>, p: &mut [u8], pw: usize, ph: usize, scale: usize,
                             r: usize, like: &MaskUpscaler) {
        let block = (BLOCK / scale).max(1);
        let grid = MaskSize { w: pw.div_ceil(block).max(2), h: ph.div_ceil(block).max(2) };
        let n = grid.len();

        let alpha = ensure(&mut slots.alpha, pw, like.mask_size());
        alpha.aim_like(like);
        alpha.prepare_quantized(like.quantized());
        // Nothing to fill is the common case for a frame with nobody in it.
        if alpha.is_empty() {
            return;
        }

        // How much of each sample is background to keep, once for both
        // passes. Outside a row's span the alpha is zero and the sample keeps
        // all of itself.
        self.keeps.resize(pw * ph, 0);
        self.spans.resize(ph, (0, 0));
        let mut any = false;
        for row in 0..ph {
            let (a0, a1, af, c0, c1) = alpha.rows_span(row, ph);
            let keeps = &mut self.keeps[row * pw..][..pw];
            keeps.fill(255);
            let mut held = 0u8;
            for ((k, &x0), &x1) in keeps[c0..c1].iter_mut().zip(&a0[c0..c1]).zip(&a1[c0..c1]) {
                *k = keep(lerp(x0, x1, af));
                held |= !*k;
            }
            any |= held != 0;
            self.spans[row] = (c0, c1);
        }
        if !any {
            return;
        }

        // Per block: background-weighted sums of each channel, and the
        // weights. A block's average is all that is kept, so blocks are read
        // at every other row and column: a quarter of the samples, within a
        // level. Sums and weights are thinned alike, so their ratio is
        // unchanged.
        self.isums.resize_with(C, Vec::new);
        for s in &mut self.isums {
            s.clear();
            s.resize(n, 0);
        }
        self.iweights.clear();
        self.iweights.resize(n, 0);
        let step = if block >= 2 { 2 } else { 1 };
        for row in (0..ph).step_by(step) {
            let line = &p[row * pw * C..][..pw * C];
            let keeps = &self.keeps[row * pw..][..pw];
            let base = (row / block) * grid.w;
            for (bx, (px, ks)) in line.chunks(block * C).zip(keeps.chunks(block)).enumerate() {
                let mut weight = 0u32;
                let mut sums = [0u32; C];
                for (px, &k) in px.chunks_exact(C).zip(ks).step_by(step) {
                    let k = u32::from(k);
                    weight += k;
                    for c in 0..C {
                        sums[c] += u32::from(px[c]) * k;
                    }
                }
                self.iweights[base + bx] += weight;
                for c in 0..C {
                    self.isums[c][base + bx] += sums[c];
                }
            }
        }
        self.sums.resize_with(C, Vec::new);
        for (s, i) in self.sums.iter_mut().zip(&self.isums) {
            s.clear();
            s.extend(i.iter().map(|&v| v as f32));
        }
        self.weights.clear();
        self.weights.extend(self.iweights.iter().map(|&v| v as f32));

        // Reach well past the blur, so every background sample's window finds
        // an estimate made from real background rather than from the prior.
        // Three passes of radius r reach about 3r.
        let reach = r.div_ceil(block).max(2);
        self.scratch.resize(n, 0.0);
        for plane in self.sums.iter_mut().chain(std::iter::once(&mut self.weights)) {
            for _ in 0..3 {
                box_f32(plane, &mut self.scratch, grid.w, grid.h, reach);
            }
        }

        // Each channel's estimate, stretched back to the plane, then mixed in
        // wherever the sample is not wholly background -- only there, since
        // most of a frame is background and comes back untouched.
        let total: f32 = self.weights.iter().sum();
        self.est.resize(n, 0.0);
        for c in 0..C {
            // The plane's own average background stands in where no background
            // is in reach, and mid grey where there is none at all.
            let sum: f32 = self.sums[c].iter().sum();
            let mean = if total > 0.0 { sum / total } else { 128.0 };
            for ((o, &s), &wt) in self.est.iter_mut().zip(&self.sums[c]).zip(&self.weights) {
                *o = (s + mean * PRIOR) / (wt + PRIOR) / 255.0;
            }
            ensure(&mut slots.spread[c], pw, grid).prepare(&self.est);
        }
        let spreads: [&MaskUpscaler; C] = std::array::from_fn(|c| slots.spread[c].as_ref().expect("prepared above"));
        for row in 0..ph {
            let (c0, c1) = self.spans[row];
            if c1 <= c0 {
                continue;
            }
            let keeps = &self.keeps[row * pw..][..pw];
            let rows: [_; C] = std::array::from_fn(|c| spreads[c].rows(row, ph));
            let line = &mut p[row * pw * C..][..pw * C];
            for x in c0..c1 {
                let k = keeps[x];
                if k != 255 {
                    for (c, (f0, f1, ff)) in rows.iter().enumerate() {
                        line[x * C + c] = mix(line[x * C + c], lerp(f0[x], f1[x], *ff), k);
                    }
                }
            }
        }
    }
}

/// The upscaler in `slot`, rebuilt when the width or grid it was made for no
/// longer matches -- the blur shrinks by a different factor as the radius
/// moves.
fn ensure(slot: &mut Option<MaskUpscaler>, width: usize, grid: MaskSize) -> &mut MaskUpscaler {
    if slot.as_ref().is_none_or(|u| u.width() != width || u.mask_size() != grid) {
        *slot = Some(MaskUpscaler::new(width, grid));
    }
    slot.as_mut().expect("just made")
}

/// Two bracketing rows mixed at weight `f` of 256.
#[inline(always)]
fn lerp(a: u8, b: u8, f: u32) -> u8 {
    ((u32::from(a) * (256 - f) + u32::from(b) * f) >> 8) as u8
}

/// How much of a sample is background worth keeping, 0..=255, from its alpha.
///
/// Squared, so the soft band at the silhouette counts as mostly subject: half
/// a hand's motion-blurred edge is still hand, and letting it into the
/// estimate brings back a faint version of the glow this is here to remove.
#[inline(always)]
fn keep(a: u8) -> u8 {
    let b = 255 - u32::from(a);
    ((b * b + 127) / 255) as u8
}

/// `px` where it is background, `fill` where it is subject.
#[inline(always)]
fn mix(px: u8, fill: u8, keep: u8) -> u8 {
    let k = u32::from(keep);
    ((u32::from(px) * k + u32::from(fill) * (255 - k) + 127) / 255) as u8
}

/// Box blur of a small float plane in place, edges clamped, radius `r`.
/// Both passes walk rows, the vertical one carrying a running sum per column.
fn box_f32(p: &mut [f32], scratch: &mut [f32], w: usize, h: usize, r: usize) {
    let n = (2 * r + 1) as f32;
    for y in 0..h {
        let row = &p[y * w..][..w];
        let out = &mut scratch[y * w..][..w];
        let mut s: f32 = (0..=r).map(|i| row[i.min(w - 1)]).sum::<f32>() + r as f32 * row[0];
        for x in 0..w {
            out[x] = s / n;
            s += row[(x + r + 1).min(w - 1)] - row[x.saturating_sub(r)];
        }
    }
    let mut sums: Vec<f32> = scratch[..w].iter().map(|&v| v * (r + 1) as f32).collect();
    for y in 1..=r {
        let row = &scratch[y.min(h - 1) * w..][..w];
        for (s, &v) in sums.iter_mut().zip(row) {
            *s += v;
        }
    }
    for y in 0..h {
        let add = (y + r + 1).min(h - 1);
        let sub = y.saturating_sub(r);
        for x in 0..w {
            p[y * w + x] = sums[x] / n;
            sums[x] += scratch[add * w + x] - scratch[sub * w + x];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nv12::Blur;

    /// A mask with a block of subject in it, and a frame of one grey with that
    /// block in another.
    fn scene(w: usize, h: usize, bg: u8, fg: u8) -> (Vec<u8>, Vec<u8>, Vec<f32>, MaskUpscaler) {
        let size = MaskSize { w: 64, h: 36 };
        let mut mask = vec![0.0f32; size.len()];
        for my in 10..30 {
            for mx in 24..40 {
                mask[my * size.w + mx] = 1.0;
            }
        }
        let mut up = MaskUpscaler::new(w, size);
        up.prepare(&mask);
        let mut y = vec![bg; w * h];
        for row in 0..h {
            let (m0, m1, f) = up.rows(row, h);
            for col in 0..w {
                if lerp(m0[col], m1[col], f) > 128 {
                    y[row * w + col] = fg;
                }
            }
        }
        (y, vec![128; w * h / 2], mask, up)
    }

    /// No subject, no change: every sample is background and comes back as it
    /// was, bit for bit.
    #[test]
    fn a_plane_with_no_subject_is_untouched() {
        let (w, h) = (320, 180);
        let size = MaskSize { w: 64, h: 36 };
        let mut up = MaskUpscaler::new(w, size);
        up.prepare(&vec![0.0; size.len()]);
        let p0: Vec<u8> = (0..w * h).map(|i| (i * 7 % 251) as u8).collect();
        let mut p = p0.clone();
        Fill::new().luma(&mut p, w, h, 1, 40, &up);
        assert_eq!(p, p0);
    }

    /// Under the subject the fill is the background, not the subject.
    #[test]
    fn the_subject_is_replaced_by_the_background_behind_it() {
        let (w, h) = (320, 180);
        let (mut y, _, _, up) = scene(w, h, 60, 220);
        Fill::new().luma(&mut y, w, h, 1, 40, &up);
        let centre = y[(h / 2) * w + w / 2];
        assert!((i32::from(centre) - 60).abs() <= 2, "the subject's centre became {centre}, not the background's 60");
    }

    /// The point of it: a background pixel beside the subject, once blurred,
    /// carries none of the subject's colour. Without the fill it is a glow.
    /// Through `Blur` as the daemon runs it, at a radius that shrinks the frame.
    #[test]
    fn blurring_with_the_fill_leaves_no_glow() {
        let (w, h, r) = (320, 180, 24);
        let (y0, uv0, _, up) = scene(w, h, 60, 220);
        // Just left of the subject, which starts at mask column 24 of 64.
        let probe = (h / 2) * w + 24 * w / 64 - 6;

        let (mut plain, mut uv) = (y0.clone(), uv0.clone());
        Blur::new(w, h).apply(&mut plain, &mut uv, w, h, w, w, r, 3);
        let (mut filled, mut uv) = (y0.clone(), uv0.clone());
        let mut fill = Fill::new();
        Blur::new(w, h).apply_filled(&mut filled, &mut uv, w, h, w, w, r, 3, &mut fill, &up);

        assert!(plain[probe] > 100, "the test needs a glow to remove: plain blur gave {}", plain[probe]);
        assert!((i32::from(filled[probe]) - 60).abs() <= 3, "with the fill: {}", filled[probe]);
    }

    /// Chroma is two channels, and the fill must keep them apart: a coloured
    /// background stays that colour under the subject, not a mix of U and V.
    #[test]
    fn chroma_is_filled_per_channel() {
        let (w, h) = (160, 90);
        let (_, _, mask, up) = scene(2 * w, 2 * h, 0, 0);
        let mut uv: Vec<u8> = (0..w * h).flat_map(|_| [90u8, 240u8]).collect();
        // Paint the subject a different colour.
        let size = MaskSize { w: 64, h: 36 };
        let mut chroma_up = MaskUpscaler::new(w, size);
        chroma_up.prepare(&mask);
        for row in 0..h {
            let (m0, m1, f) = chroma_up.rows(row, h);
            for cx in 0..w {
                if lerp(m0[cx], m1[cx], f) > 128 {
                    uv[(row * w + cx) * 2] = 200;
                    uv[(row * w + cx) * 2 + 1] = 50;
                }
            }
        }
        Fill::new().chroma(&mut uv, w, h, 2, 20, &up);
        let centre = ((h / 2) * w + w / 2) * 2;
        assert!((i32::from(uv[centre]) - 90).abs() <= 2 && (i32::from(uv[centre + 1]) - 240).abs() <= 2,
                "the subject's chroma became {},{}", uv[centre], uv[centre + 1]);
    }

    /// The fill as it was first written, per sample over whole rows, in
    /// floats, quantising the mask itself. The one in use must give its bytes.
    #[allow(clippy::too_many_arguments)]
    fn plane_as_it_was<const C: usize>(f: &mut Fill, slots: &mut Slots<C>, p: &mut [u8], pw: usize, ph: usize, scale: usize,
                             r: usize, mask: &[f32], like: &MaskUpscaler) {
        let block = (BLOCK / scale).max(1);
        let grid = MaskSize { w: pw.div_ceil(block).max(2), h: ph.div_ceil(block).max(2) };
        let n = grid.len();

        let alpha = ensure(&mut slots.alpha, pw, like.mask_size());
        alpha.aim_like(like);
        alpha.prepare(mask);

        // How much of each sample is background to keep, once for both passes.
        f.keeps.resize(pw * ph, 0);
        for row in 0..ph {
            let (a0, a1, af) = alpha.rows(row, ph);
            for ((k, &x0), &x1) in f.keeps[row * pw..][..pw].iter_mut().zip(a0).zip(a1) {
                *k = keep(lerp(x0, x1, af));
            }
        }
        // Nothing to fill is the common case for a frame with nobody in it.
        if f.keeps.iter().all(|&k| k == 255) {
            return;
        }

        // Per block: background-weighted sums of each channel, and the weights.
        f.sums.resize_with(C, Vec::new);
        for s in &mut f.sums {
            s.clear();
            s.resize(n, 0.0);
        }
        f.weights.clear();
        f.weights.resize(n, 0.0);
        // A block's average is all that is kept, so blocks are read at every
        // other row and column: a quarter of the samples, within a level. Sums
        // and weights are thinned alike, so their ratio is unchanged.
        let step = if block >= 2 { 2 } else { 1 };
        for row in (0..ph).step_by(step) {
            let line = &p[row * pw * C..][..pw * C];
            let keeps = &f.keeps[row * pw..][..pw];
            let base = (row / block) * grid.w;
            for x in (0..pw).step_by(step) {
                let (b, k) = (base + x / block, f32::from(keeps[x]));
                for c in 0..C {
                    f.sums[c][b] += f32::from(line[x * C + c]) * k;
                }
                f.weights[b] += k;
            }
        }

        // Reach well past the blur, so every background sample's window finds
        // an estimate made from real background rather than from the prior.
        // Three passes of radius r reach about 3r.
        let reach = r.div_ceil(block).max(2);
        f.scratch.resize(n, 0.0);
        for plane in f.sums.iter_mut().chain(std::iter::once(&mut f.weights)) {
            for _ in 0..3 {
                box_f32(plane, &mut f.scratch, grid.w, grid.h, reach);
            }
        }

        // Each channel's estimate, stretched back to the plane, then mixed in
        // wherever the sample is not wholly background -- only there, since
        // most of a frame is background and comes back untouched.
        let total: f32 = f.weights.iter().sum();
        f.est.resize(n, 0.0);
        for c in 0..C {
            // The plane's own average background stands in where no background
            // is in reach, and mid grey where there is none at all.
            let sum: f32 = f.sums[c].iter().sum();
            let mean = if total > 0.0 { sum / total } else { 128.0 };
            for ((o, &s), &wt) in f.est.iter_mut().zip(&f.sums[c]).zip(&f.weights) {
                *o = (s + mean * PRIOR) / (wt + PRIOR) / 255.0;
            }
            ensure(&mut slots.spread[c], pw, grid).prepare(&f.est);
        }
        let spreads: [&MaskUpscaler; C] = std::array::from_fn(|c| slots.spread[c].as_ref().expect("prepared above"));
        for row in 0..ph {
            let keeps = &f.keeps[row * pw..][..pw];
            if keeps.iter().all(|&k| k == 255) {
                continue;
            }
            let rows: [_; C] = std::array::from_fn(|c| spreads[c].rows(row, ph));
            let line = &mut p[row * pw * C..][..pw * C];
            for (x, &k) in keeps.iter().enumerate() {
                if k != 255 {
                    for (c, (f0, f1, ff)) in rows.iter().enumerate() {
                        line[x * C + c] = mix(line[x * C + c], lerp(f0[x], f1[x], *ff), k);
                    }
                }
            }
        }
    }

    #[test]
    fn the_fill_is_what_it_was() {
        let size = MaskSize { w: 64, h: 36 };
        for (w, h) in [(320usize, 180usize), (480, 270)] {
            let frame: Vec<u8> = (0..w * h * 2).map(|i| ((i * 37 + i / 7) % 251) as u8).collect();
            for (cx, cy, aim) in [(20.0f32, 18.0f32, false), (50.0, 10.0, true), (-90.0, -90.0, false), (32.0, 30.0, true)] {
                let mask: Vec<f32> = (0..size.len())
                    .map(|i| {
                        let (x, y) = ((i % size.w) as f32 - cx, (i / size.w) as f32 - cy);
                        (1.3 - (x * x + y * y) / 120.0).clamp(0.0, 1.0)
                    })
                    .collect();
                let mut up = MaskUpscaler::new(2 * w, size);
                if aim {
                    up.aim(8.0, 40.0, 4.0, 25.0);
                }
                up.prepare(&mask);
                for scale in [1, 2, 4] {
                    let (mut want, mut got) = (frame[..w * h].to_vec(), frame[..w * h].to_vec());
                    let (mut old, mut new) = (Fill::new(), Fill::new());
                    let mut slots = Slots::<1>::default();
                    plane_as_it_was::<1>(&mut old, &mut slots, &mut want, w, h, scale, 20, &mask, &up);
                    new.luma(&mut got, w, h, scale, 20, &up);
                    assert_eq!(got, want, "luma {w}x{h} at 1/{scale}, blob at {cx},{cy}");
                    let (mut want, mut got) = (frame[..w * h].to_vec(), frame[..w * h].to_vec());
                    let mut slots = Slots::<2>::default();
                    plane_as_it_was::<2>(&mut old, &mut slots, &mut want, w / 2, h, 2 * scale, 10, &mask, &up);
                    new.chroma(&mut got, w / 2, h, 2 * scale, 10, &up);
                    assert_eq!(got, want, "chroma {w}x{h} at 1/{scale}, blob at {cx},{cy}");
                }
            }
        }
    }

    #[test]
    fn the_float_box_keeps_a_flat_plane_flat_and_matches_a_naive_one() {
        let (w, h, r) = (17, 9, 4);
        let mut p = vec![3.5f32; w * h];
        let mut s = vec![0.0; w * h];
        box_f32(&mut p, &mut s, w, h, r);
        assert!(p.iter().all(|&v| (v - 3.5).abs() < 1e-4));

        let src: Vec<f32> = (0..w * h).map(|i| ((i * 37) % 11) as f32).collect();
        let mut fast = src.clone();
        box_f32(&mut fast, &mut s, w, h, r);
        let at = |x: isize, y: isize| src[(y.clamp(0, h as isize - 1) as usize) * w + x.clamp(0, w as isize - 1) as usize];
        for y in 0..h as isize {
            for x in 0..w as isize {
                let mut sum = 0.0;
                for dy in -(r as isize)..=r as isize {
                    for dx in -(r as isize)..=r as isize {
                        sum += at(x + dx, y + dy);
                    }
                }
                let want = sum / ((2 * r + 1) * (2 * r + 1)) as f32;
                assert!((fast[y as usize * w + x as usize] - want).abs() < 1e-3);
            }
        }
    }
}
