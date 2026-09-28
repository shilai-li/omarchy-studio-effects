//! Frame maths done directly on NV12 planes.
//!
//! The measurements in AGENTS.md say colour conversion costs six times what the
//! model does, so nothing here ever materialises an RGB frame. The only pixels
//! that become RGB are the 256x256 the model needs -- 65k of them instead of
//! 2M -- and the composite runs on the Y and UV planes as they arrive.

/// Model input side, and the side of the mask that comes back.
pub const NET: usize = 256;

/// BT.601 limited-range YUV -> RGB, the range v4l2 webcams actually emit.
#[inline]
fn yuv_to_rgb(y: u8, u: u8, v: u8) -> (f32, f32, f32) {
    let y = (f32::from(y) - 16.0) * 1.164_383;
    let u = f32::from(u) - 128.0;
    let v = f32::from(v) - 128.0;
    (
        (y + 1.596_027 * v).clamp(0.0, 255.0),
        (y - 0.391_762 * u - 0.812_968 * v).clamp(0.0, 255.0),
        (y + 2.017_232 * u).clamp(0.0, 255.0),
    )
}

/// Fill a planar NCHW float tensor with the frame scaled to 256x256.
///
/// Nearest-neighbour on purpose: the model runs at 256x256 and its output is a
/// soft mask that gets bilinearly upscaled anyway, so a costlier sample here
/// buys nothing a viewer could see.
pub fn write_model_input(
    y_plane: &[u8],
    uv_plane: &[u8],
    width: usize,
    height: usize,
    y_stride: usize,
    uv_stride: usize,
    out: &mut [f32],
) {
    debug_assert_eq!(out.len(), 3 * NET * NET);
    let (r_plane, rest) = out.split_at_mut(NET * NET);
    let (g_plane, b_plane) = rest.split_at_mut(NET * NET);

    for ny in 0..NET {
        let sy = ny * height / NET;
        let y_row = sy * y_stride;
        let uv_row = (sy / 2) * uv_stride;
        for nx in 0..NET {
            let sx = nx * width / NET;
            let uv = uv_row + (sx & !1);
            let (r, g, b) = yuv_to_rgb(y_plane[y_row + sx], uv_plane[uv], uv_plane[uv + 1]);
            let i = ny * NET + nx;
            r_plane[i] = r / 255.0;
            g_plane[i] = g / 255.0;
            b_plane[i] = b / 255.0;
        }
    }
}

/// Box blur one plane in place, separably, via a packed scratch buffer.
///
/// Running-sum, so cost is independent of radius -- the whole reason this is a
/// box blur and not a Gaussian. At the radii a background blur uses there is no
/// detail left to reveal the box shape, so one pass is enough.
///
/// Two things here are not the obvious way round, and both were worth ~4 ms a
/// frame at 1080p:
///
/// The window average is a reciprocal multiply, not a divide. The divisor is the
/// same for every one of four million pixels, so dividing per pixel pays for a
/// division the compiler cannot hoist on its own.
///
/// The vertical pass walks rows, not columns. Blurring a column at a time is the
/// natural way to write it and reads one byte from each of `h` cache lines,
/// missing on nearly every access. Carrying a running sum per column instead
/// turns the whole pass into sequential row scans.
pub fn box_blur(plane: &mut [u8], scratch: &mut [u8], w: usize, h: usize, stride: usize, r: usize) {
    if r == 0 || w == 0 || h == 0 {
        return;
    }
    let r = r.min(w - 1).min(h - 1);
    let avg = window_average(r);

    // Horizontal: plane -> scratch, packed to width so the vertical pass can
    // scan rows without the stride's padding in the way.
    for row in 0..h {
        let src = &plane[row * stride..row * stride + w];
        let dst = &mut scratch[row * w..(row + 1) * w];
        let mut sum: u32 = u32::from(src[0]) * (r + 1) as u32;
        for x in 1..=r {
            sum += u32::from(src[x.min(w - 1)]);
        }
        for x in 0..w {
            dst[x] = avg(sum);
            sum += u32::from(src[(x + r + 1).min(w - 1)]);
            sum -= u32::from(src[x.saturating_sub(r)]);
        }
    }

    blur_columns(plane, scratch, w, h, stride, r, avg);
}

/// Box blur an interleaved chroma plane, U and V each on their own.
///
/// NV12 stores chroma as U,V,U,V, and `box_blur` sees a row of bytes, so
/// running it here averages every U with the Vs beside it. That is what this
/// replaced, and it is not a subtle error: a red wall (U,V 90,240) came out
/// 159,171, skin tones (110,150) nearly grey at 128,132, and every coloured
/// background drifted toward magenta-grey the moment the blur came on. A
/// single-channel test could never see it, which is why
/// `chroma_blur_keeps_u_and_v_apart` exists.
///
/// The fix is only horizontal. A column of this plane is all U or all V, so the
/// vertical pass is `box_blur`'s own.
///
/// `w` is the row length in bytes, as for `box_blur`; `r` is in chroma samples,
/// so the old call's `r / 2` bytes -- a quarter of the luma radius -- is now
/// the half it was meant to be.
pub fn box_blur_uv(plane: &mut [u8], scratch: &mut [u8], w: usize, h: usize, stride: usize, r: usize) {
    let n = w / 2;
    if r == 0 || n == 0 || h == 0 {
        return;
    }
    let r = r.min(n - 1).min(h - 1);
    let avg = window_average(r);

    for row in 0..h {
        let src = &plane[row * stride..row * stride + 2 * n];
        let dst = &mut scratch[row * 2 * n..(row + 1) * 2 * n];
        for c in 0..2 {
            let at = |x: usize| u32::from(src[2 * x + c]);
            let mut sum: u32 = at(0) * (r + 1) as u32;
            for x in 1..=r {
                sum += at(x.min(n - 1));
            }
            for x in 0..n {
                dst[2 * x + c] = avg(sum);
                sum += at((x + r + 1).min(n - 1));
                sum -= at(x.saturating_sub(r));
            }
        }
    }

    blur_columns(plane, scratch, 2 * n, h, stride, r, avg);
}

/// The window average for a radius, as a 24-bit fixed-point reciprocal: a
/// multiply and a shift rather than a per-pixel divide.
///
/// Both roundings here are load-bearing. Truncating the reciprocal, or the
/// product, biases every pixel downward: with a window of 11, a flat plane of
/// 200 blurs to 199, so a still background visibly darkens as soon as effects
/// come on. Rounding the reciprocal to nearest and adding a half before the
/// shift keeps a flat plane exactly flat, which `blur_of_a_flat_plane_is_flat`
/// pins down.
fn window_average(r: usize) -> impl Fn(u32) -> u8 + Copy {
    let window = (2 * r + 1) as u64;
    let recip = ((1u64 << 24) + window / 2) / window;
    move |sum: u32| ((u64::from(sum) * recip + (1 << 23)) >> 24) as u8
}

/// Vertical pass, scratch -> plane: one running sum per column, advanced a
/// whole row at a time.
fn blur_columns(
    plane: &mut [u8],
    scratch: &[u8],
    w: usize,
    h: usize,
    stride: usize,
    r: usize,
    avg: impl Fn(u32) -> u8,
) {
    let mut sums: Vec<u32> = scratch[..w].iter().map(|&v| u32::from(v) * (r + 1) as u32).collect();
    for y in 1..=r {
        let row = &scratch[y.min(h - 1) * w..][..w];
        for (s, &v) in sums.iter_mut().zip(row) {
            *s += u32::from(v);
        }
    }
    for y in 0..h {
        let out = &mut plane[y * stride..y * stride + w];
        for (o, &s) in out.iter_mut().zip(sums.iter()) {
            *o = avg(s);
        }
        let add = &scratch[(y + r + 1).min(h - 1) * w..][..w];
        let sub = &scratch[y.saturating_sub(r) * w..][..w];
        for ((s, &a), &b) in sums.iter_mut().zip(add).zip(sub) {
            *s = *s + u32::from(a) - u32::from(b);
        }
    }
}

/// Where the subject is, in mask coordinates, as (left, top, right, bottom).
///
/// Taken from the segmentation mask rather than a face detector: the mask is
/// already a per-pixel map of the subject against everything else, so the
/// subject's position costs nothing beyond a scan of 65k values. A second model
/// would cost another inference and could disagree with the one doing the
/// compositing.
///
/// Rows and columns need a minimum run of foreground to count, so a handful of
/// stray confident pixels -- a hand at the frame edge, a patch of noise on a
/// blank wall -- cannot drag the frame across the room.
pub fn subject_box(mask: &[f32], threshold: f32, min_run: usize) -> Option<(usize, usize, usize, usize)> {
    let mut cols = [0u16; NET];
    let mut rows = [0u16; NET];
    for y in 0..NET {
        for x in 0..NET {
            if mask[y * NET + x] >= threshold {
                cols[x] += 1;
                rows[y] += 1;
            }
        }
    }

    let span = |counts: &[u16; NET]| -> Option<(usize, usize)> {
        let first = counts.iter().position(|&c| c as usize >= min_run)?;
        let last = counts.iter().rposition(|&c| c as usize >= min_run)?;
        Some((first, last))
    };

    let (left, right) = span(&cols)?;
    let (top, bottom) = span(&rows)?;
    Some((left, top, right, bottom))
}

/// Crop a rectangle out of a frame and scale it to fill the output, bilinearly.
///
/// The weights for a given rectangle are the same for every row and every
/// frame, so they are handed in precomputed. Recomputing them per frame would
/// cost more than the resample; the caller rebuilds them only when the crop
/// actually moves.
pub struct Resampler {
    /// Per output column: the two source columns to mix, and the 0..=256 weight
    /// toward the second. Both are stored rather than deriving the second as
    /// `first + 1`, which walks off the end of the row at the last column.
    x_idx: Vec<u32>,
    x_next: Vec<u32>,
    x_frac: Vec<u32>,
    width: usize,
}

impl Resampler {
    pub fn new(width: usize) -> Self {
        Self {
            x_idx: vec![0; width],
            x_next: vec![0; width],
            x_frac: vec![0; width],
            width,
        }
    }

    /// Point the tables at a source span. `scale` is how many source columns
    /// each output column advances, in 16.16 fixed point.
    pub fn aim(&mut self, src_left: f32, src_width: f32, limit: usize) {
        for col in 0..self.width {
            let sx = src_left + src_width * col as f32 / self.width as f32;
            let sx = sx.max(0.0).min(limit as f32 - 1.0);
            self.x_idx[col] = sx as u32;
            self.x_next[col] = (sx as u32 + 1).min(limit as u32 - 1);
            self.x_frac[col] = ((sx - sx.floor()) * 256.0) as u32;
        }
    }

    /// The two source rows bracketing an output row, and the weight between.
    #[inline]
    fn rows_for(row: usize, height: usize, top: f32, span: f32, src_rows: usize) -> (usize, usize, u32) {
        let sy = (top + span * row as f32 / height as f32)
            .max(0.0)
            .min(src_rows as f32 - 1.0);
        let y0 = sy as usize;
        (y0, (y0 + 1).min(src_rows - 1), ((sy - sy.floor()) * 256.0) as u32)
    }

    /// Resample a luma plane.
    ///
    /// Split from the chroma version rather than taking a byte count, because a
    /// loop bound the compiler cannot see is a loop it will not unroll or
    /// vectorise -- and this runs on every pixel of every frame.
    pub fn luma(
        &self,
        src: &[u8],
        dst: &mut [u8],
        src_stride: usize,
        dst_stride: usize,
        height: usize,
        top: f32,
        span: f32,
        src_rows: usize,
    ) {
        for row in 0..height {
            let (y0, y1, yf) = Self::rows_for(row, height, top, span, src_rows);
            let (r0, r1) = (y0 * src_stride, y1 * src_stride);
            let out = &mut dst[row * dst_stride..row * dst_stride + self.width];

            for (col, o) in out.iter_mut().enumerate() {
                let (x0, x1) = (self.x_idx[col] as usize, self.x_next[col] as usize);
                let xf = self.x_frac[col];
                let top_row = u32::from(src[r0 + x0]) * (256 - xf) + u32::from(src[r0 + x1]) * xf;
                let bot_row = u32::from(src[r1 + x0]) * (256 - xf) + u32::from(src[r1 + x1]) * xf;
                *o = ((top_row * (256 - yf) + bot_row * yf) >> 16) as u8;
            }
        }
    }

    /// Resample an interleaved chroma plane, where U and V move together and
    /// share one set of weights.
    pub fn chroma(
        &self,
        src: &[u8],
        dst: &mut [u8],
        src_stride: usize,
        dst_stride: usize,
        height: usize,
        top: f32,
        span: f32,
        src_rows: usize,
    ) {
        for row in 0..height {
            let (y0, y1, yf) = Self::rows_for(row, height, top, span, src_rows);
            let (r0, r1) = (y0 * src_stride, y1 * src_stride);
            let out = row * dst_stride;

            for col in 0..self.width {
                let (x0, x1) = (self.x_idx[col] as usize * 2, self.x_next[col] as usize * 2);
                let xf = self.x_frac[col];
                for b in 0..2 {
                    let t = u32::from(src[r0 + x0 + b]) * (256 - xf)
                        + u32::from(src[r0 + x1 + b]) * xf;
                    let d = u32::from(src[r1 + x0 + b]) * (256 - xf)
                        + u32::from(src[r1 + x1 + b]) * xf;
                    dst[out + col * 2 + b] = ((t * (256 - yf) + d * yf) >> 16) as u8;
                }
            }
        }
    }
}

/// Darken and drain colour from a background plane, in place.
///
/// Both are done on the planes as they are: luma carries brightness, so dimming
/// is a scale on Y alone, and chroma is stored as a signed offset from 128, so
/// desaturating is pulling UV toward that midpoint. Neither needs the frame in
/// RGB, which is the whole reason they cost almost nothing.
///
/// `dim` and `desat` are 0..=100. They apply to whatever is behind the subject,
/// blurred or replaced, and never to the subject: this runs before the blend,
/// on the background only.
pub fn tint(
    y_plane: &mut [u8],
    uv_plane: &mut [u8],
    w: usize,
    h: usize,
    y_stride: usize,
    uv_stride: usize,
    dim: u32,
    desat: u32,
) {
    if dim > 0 {
        // 8-bit fixed point, rounded like the blur's average so a dim of 0
        // through this path would leave the plane untouched.
        let keep = (100 - dim.min(100)) * 256 / 100;
        for row in 0..h {
            for v in &mut y_plane[row * y_stride..row * y_stride + w] {
                *v = ((u32::from(*v) * keep + 128) >> 8) as u8;
            }
        }
    }

    if desat > 0 {
        let keep = (100 - desat.min(100)) as i32 * 256 / 100;
        for row in 0..h / 2 {
            for v in &mut uv_plane[row * uv_stride..row * uv_stride + w] {
                let centred = i32::from(*v) - 128;
                *v = (128 + ((centred * keep + 128) >> 8)).clamp(0, 255) as u8;
            }
        }
    }
}

/// Upscales the model's 256x256 mask to frame width, separably.
///
/// The naive version sampled the mask bilinearly per pixel: four float loads and
/// half a dozen float ops, two million times a frame, and it dominated the whole
/// composite. Separating the two axes fixes that. The horizontal pass runs once
/// per frame over 256 rows -- 491k operations, not 2M -- and the vertical pass
/// collapses to two byte loads and a lerp per pixel, in integers.
///
/// The horizontal weights are the same for every row and every frame, so they are
/// computed once at construction and never touched again.
pub struct MaskUpscaler {
    /// Per output column: the mask column to its left, and the 0..=256 weight
    /// toward the next one.
    x_idx: Vec<u32>,
    x_frac: Vec<u32>,
    /// The 256 mask rows, each stretched to output width.
    band: Vec<u8>,
    /// The mask quantised to 8-bit once per frame, so the horizontal stretch
    /// is integer-only. Doing the float clamp at output width instead meant
    /// two float conversions per column per mask row -- 655k of them at 720p
    /// -- for a 256x256 source.
    quantized: Vec<u8>,
    width: usize,
    /// The slice of the mask the output covers, in mask units. The whole mask
    /// when the output is the whole frame; a sub-range when framing has cropped
    /// into it, because the output then shows only part of what was segmented.
    x0: f32,
    x_span: f32,
    y0: f32,
    y_span: f32,
}

impl MaskUpscaler {
    pub fn new(width: usize) -> Self {
        let mut me = Self {
            x_idx: vec![0; width],
            x_frac: vec![0; width],
            band: vec![0; NET * width],
            quantized: vec![0; NET * NET],
            width,
            x0: 0.0,
            x_span: (NET - 1) as f32,
            y0: 0.0,
            y_span: (NET - 1) as f32,
        };
        me.recompute_columns();
        me
    }

    /// Point the upscaler at the part of the mask the output actually shows.
    ///
    /// Given in mask units so the caller does the one conversion from its own
    /// crop rectangle, rather than this having to know about capture sizes.
    /// Both spans are the *whole* mask by default, which is the uncropped case.
    pub fn aim(&mut self, x0: f32, x_span: f32, y0: f32, y_span: f32) {
        let limit = (NET - 1) as f32;
        let changed = self.x0 != x0 || self.x_span != x_span;
        self.x0 = x0.clamp(0.0, limit);
        self.x_span = x_span.clamp(1.0, limit);
        self.y0 = y0.clamp(0.0, limit);
        self.y_span = y_span.clamp(1.0, limit);
        // The column table only depends on the horizontal aim, and framing
        // holds still most of the time by design.
        if changed {
            self.recompute_columns();
        }
    }

    fn recompute_columns(&mut self) {
        for col in 0..self.width {
            // Fixed point with an 8-bit fraction, so the lerp stays in integers.
            let x = self.x0 + self.x_span * col as f32 / self.width as f32;
            let x = x.clamp(0.0, (NET - 1) as f32);
            self.x_idx[col] = x as u32;
            self.x_frac[col] = ((x - x.floor()) * 256.0) as u32;
        }
    }

    /// Stretch every mask row to frame width. Once per frame, before blending.
    pub fn prepare(&mut self, mask: &[f32]) {
        for (q, &v) in self.quantized.iter_mut().zip(mask) {
            *q = (v.clamp(0.0, 1.0) * 255.0) as u8;
        }
        for row in 0..NET {
            let src = &self.quantized[row * NET..(row + 1) * NET];
            let dst = &mut self.band[row * self.width..(row + 1) * self.width];
            for col in 0..self.width {
                let i = self.x_idx[col] as usize;
                let f = self.x_frac[col];
                let a = u32::from(src[i]);
                let b = u32::from(src[(i + 1).min(NET - 1)]);
                dst[col] = ((a * (256 - f) + b * f) >> 8) as u8;
            }
        }
    }

    /// The two band rows bracketing output row `row` of `height`, and the
    /// 0..=256 weight toward the second.
    #[inline]
    fn rows(&self, row: usize, height: usize) -> (&[u8], &[u8], u32) {
        let y = self.y0 + self.y_span * row as f32 / height as f32;
        let y = y.clamp(0.0, (NET - 1) as f32);
        let (r0, f) = (y as usize, ((y - y.floor()) * 256.0) as u32);
        let r1 = (r0 + 1).min(NET - 1);
        (
            &self.band[r0 * self.width..(r0 + 1) * self.width],
            &self.band[r1 * self.width..(r1 + 1) * self.width],
            f,
        )
    }
}

/// Blend the sharp luma plane over the blurred one, weighted by the mask.
pub fn blend_luma(
    fg: &[u8],
    bg: &mut [u8],
    up: &MaskUpscaler,
    w: usize,
    h: usize,
    stride: usize,
) {
    for row in 0..h {
        let (m0, m1, vf) = up.rows(row, h);
        let base = row * stride;
        for col in 0..w {
            let a = (u32::from(m0[col]) * (256 - vf) + u32::from(m1[col]) * vf) >> 8;
            let i = base + col;
            bg[i] = ((u32::from(fg[i]) * a + u32::from(bg[i]) * (255 - a)) / 255) as u8;
        }
    }
}

/// Blend the interleaved chroma plane, which is half resolution in both axes.
///
/// The band is built at luma width, so chroma column `c` reads band column
/// `2 * c` -- no second upscale, and the U and V bytes of a pixel share one
/// mask value rather than sampling it twice.
pub fn blend_chroma(
    fg: &[u8],
    bg: &mut [u8],
    up: &MaskUpscaler,
    w: usize,
    h: usize,
    stride: usize,
) {
    for row in 0..h {
        let (m0, m1, vf) = up.rows(row * 2, h * 2);
        let base = row * stride;
        for col in 0..w {
            let c = (col * 2).min(up.width - 1);
            let a = (u32::from(m0[c]) * (256 - vf) + u32::from(m1[c]) * vf) >> 8;
            let i = base + col * 2;
            bg[i] = ((u32::from(fg[i]) * a + u32::from(bg[i]) * (255 - a)) / 255) as u8;
            bg[i + 1] = ((u32::from(fg[i + 1]) * a + u32::from(bg[i + 1]) * (255 - a)) / 255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The float bilinear sample the separable upscaler replaced. Kept as the
    /// reference the fast path is checked against, since "it looked right on a
    /// webcam" is not a test.
    fn reference_alpha(mask: &[f32], fx: f32, fy: f32) -> f32 {
        let x = fx.clamp(0.0, (NET - 1) as f32);
        let y = fy.clamp(0.0, (NET - 1) as f32);
        let (x0, y0) = (x as usize, y as usize);
        let (x1, y1) = ((x0 + 1).min(NET - 1), (y0 + 1).min(NET - 1));
        let (tx, ty) = (x - x0 as f32, y - y0 as f32);
        let top = mask[y0 * NET + x0] * (1.0 - tx) + mask[y0 * NET + x1] * tx;
        let bot = mask[y1 * NET + x0] * (1.0 - tx) + mask[y1 * NET + x1] * tx;
        (top * (1.0 - ty) + bot * ty) * 255.0
    }

    fn alpha_at(up: &MaskUpscaler, col: usize, row: usize, height: usize) -> u32 {
        let (m0, m1, vf) = up.rows(row, height);
        (u32::from(m0[col]) * (256 - vf) + u32::from(m1[col]) * vf) >> 8
    }

    /// A smooth mask must upscale to within rounding of the float reference.
    #[test]
    fn upscaler_matches_float_bilinear_on_a_gradient() {
        let mut mask = vec![0.0f32; NET * NET];
        for y in 0..NET {
            for x in 0..NET {
                mask[y * NET + x] = (x as f32 / NET as f32) * (y as f32 / NET as f32);
            }
        }
        let (w, h) = (1920, 1080);
        let mut up = MaskUpscaler::new(w);
        up.prepare(&mask);

        let mut worst = 0.0f32;
        for row in (0..h).step_by(7) {
            for col in (0..w).step_by(11) {
                let got = alpha_at(&up, col, row, h) as f32;
                let want = reference_alpha(
                    &mask,
                    col as f32 * (NET - 1) as f32 / w as f32,
                    row as f32 * (NET - 1) as f32 / h as f32,
                );
                worst = worst.max((got - want).abs());
            }
        }
        // The band quantises the mask to 8 bits and each axis rounds again, so
        // three levels out of 255 is the floor for this representation, not slack.
        assert!(worst <= 3.0, "worst deviation {worst} of 255");
    }

    /// Aiming at half the mask must show that half stretched across the whole
    /// output. This is what makes the composite line up with a framing crop:
    /// the output shows part of what was segmented, so the mask has to be read
    /// over that part only.
    #[test]
    fn aiming_at_part_of_the_mask_stretches_that_part() {
        // Left half black, right half white.
        let mut mask = vec![0.0f32; NET * NET];
        for y in 0..NET {
            for x in NET / 2..NET {
                mask[y * NET + x] = 1.0;
            }
        }
        let (w, h) = (640, 360);

        // Whole mask: the split lands in the middle of the output.
        let mut whole = MaskUpscaler::new(w);
        whole.prepare(&mask);
        assert_eq!(alpha_at(&whole, 10, 0, h), 0, "far left is background");
        assert_eq!(alpha_at(&whole, w - 10, 0, h), 255, "far right is foreground");

        // Right half only: the output should be foreground throughout.
        let mut right = MaskUpscaler::new(w);
        right.aim((NET / 2) as f32, (NET / 2 - 1) as f32, 0.0, (NET - 1) as f32);
        right.prepare(&mask);
        assert_eq!(alpha_at(&right, 10, 0, h), 255, "aimed at the white half");
        assert_eq!(alpha_at(&right, w - 10, 0, h), 255);
    }

    /// The vertical aim has to move independently, or a crop that is offset
    /// only in y would sample the wrong rows.
    #[test]
    fn the_vertical_aim_moves_on_its_own() {
        // Top half black, bottom half white.
        let mut mask = vec![0.0f32; NET * NET];
        for y in NET / 2..NET {
            for x in 0..NET {
                mask[y * NET + x] = 1.0;
            }
        }
        let (w, h) = (640, 360);
        let mut up = MaskUpscaler::new(w);
        up.aim(0.0, (NET - 1) as f32, (NET / 2) as f32, (NET / 2 - 1) as f32);
        up.prepare(&mask);
        assert_eq!(alpha_at(&up, 100, 5, h), 255, "aimed at the white half");
        assert_eq!(alpha_at(&up, 100, h - 5, h), 255);
    }

    /// Quantising the 256x256 mask first, then stretching in integers, must
    /// match converting each sample at output width -- the path this replaced.
    /// A speed change that shifted every alpha by a level would look like a
    /// soft subject, and a webcam would not catch it.
    #[test]
    fn prepare_matches_converting_at_output_width() {
        let mut mask = vec![0.0f32; NET * NET];
        for y in 0..NET {
            for x in 0..NET {
                mask[y * NET + x] = (x as f32 / NET as f32) * (y as f32 / NET as f32);
            }
        }
        let (w, h) = (1280, 720);
        let mut up = MaskUpscaler::new(w);
        up.prepare(&mask);

        let band_at = |row: usize, col: usize| -> u8 {
            let x = ((NET - 1) as f32 * col as f32 / w as f32).clamp(0.0, (NET - 1) as f32);
            let i = x as usize;
            let f = ((x - x.floor()) * 256.0) as u32;
            let src = &mask[row * NET..(row + 1) * NET];
            let a = (src[i].clamp(0.0, 1.0) * 255.0) as u32;
            let b = (src[(i + 1).min(NET - 1)].clamp(0.0, 1.0) * 255.0) as u32;
            ((a * (256 - f) + b * f) >> 8) as u8
        };

        for row in (0..h).step_by(13) {
            for col in (0..w).step_by(17) {
                let y = ((NET - 1) as f32 * row as f32 / h as f32).clamp(0.0, (NET - 1) as f32);
                let (r0, f) = (y as usize, ((y - y.floor()) * 256.0) as u32);
                let r1 = (r0 + 1).min(NET - 1);
                let want = (u32::from(band_at(r0, col)) * (256 - f)
                    + u32::from(band_at(r1, col)) * f)
                    >> 8;
                assert_eq!(alpha_at(&up, col, row, h), want, "at {col},{row}");
            }
        }
    }

    /// A saturated mask must stay saturated: fully-foreground pixels have to
    /// reach 255, or the subject is blended with its own blurred copy and looks
    /// washed out rather than sharp.
    #[test]
    fn upscaler_preserves_saturation() {
        let mask = vec![1.0f32; NET * NET];
        let (w, h) = (1280, 720);
        let mut up = MaskUpscaler::new(w);
        up.prepare(&mask);
        for row in (0..h).step_by(13) {
            for col in (0..w).step_by(17) {
                assert_eq!(alpha_at(&up, col, row, h), 255, "at {col},{row}");
            }
        }
    }

    #[test]
    fn subject_box_finds_a_blob_and_ignores_specks() {
        let mut mask = vec![0.0f32; NET * NET];
        // A block from (100,80) to (150,200).
        for y in 80..=200 {
            for x in 100..=150 {
                mask[y * NET + x] = 1.0;
            }
        }
        // A speck that must not widen the box.
        mask[10 * NET + 5] = 1.0;

        let (l, t, r, b) = subject_box(&mask, 0.5, 4).expect("a blob is present");
        assert_eq!((l, t, r, b), (100, 80, 150, 200));
    }

    /// An empty mask must report nothing rather than a degenerate box: framing
    /// on it would slam the crop to a corner the moment the subject steps out.
    #[test]
    fn subject_box_of_an_empty_mask_is_none() {
        assert!(subject_box(&vec![0.0f32; NET * NET], 0.5, 4).is_none());
    }

    /// Resampling the whole frame at 1:1 must return the frame.
    #[test]
    fn resampling_the_full_frame_is_a_copy() {
        let (w, h) = (64usize, 32usize);
        let src: Vec<u8> = (0..w * h).map(|i| ((i * 5) % 251) as u8).collect();
        let mut dst = vec![0u8; w * h];
        let mut r = Resampler::new(w);
        r.aim(0.0, w as f32, w);
        r.luma(&src, &mut dst, w, w, h, 0.0, h as f32, h);
        // Bilinear at exact sample points, so only the last row and column can
        // differ by a rounding step where the clamp bites.
        for y in 0..h - 1 {
            for x in 0..w - 1 {
                let (a, b) = (i32::from(dst[y * w + x]), i32::from(src[y * w + x]));
                assert!((a - b).abs() <= 1, "at {x},{y}: {a} vs {b}");
            }
        }
    }

    /// Neither knob may touch anything at zero. A background that shifts a
    /// level the moment a slider exists, without being moved, is the kind of
    /// thing nobody tracks down later.
    #[test]
    fn tint_at_zero_is_a_no_op() {
        let (w, h, ys, uvs) = (32, 16, 40, 40);
        let mut y: Vec<u8> = (0..ys * h).map(|i| (i % 256) as u8).collect();
        let mut uv: Vec<u8> = (0..uvs * h / 2).map(|i| ((i * 7) % 256) as u8).collect();
        let (y0, uv0) = (y.clone(), uv.clone());
        tint(&mut y, &mut uv, w, h, ys, uvs, 0, 0);
        assert_eq!(y, y0, "dim 0 changed luma");
        assert_eq!(uv, uv0, "desat 0 changed chroma");
    }

    #[test]
    fn dim_darkens_luma_and_leaves_colour_alone() {
        let (w, h, ys, uvs) = (16, 8, 16, 16);
        let mut y = vec![200u8; ys * h];
        let mut uv = vec![200u8; uvs * h / 2];
        tint(&mut y, &mut uv, w, h, ys, uvs, 50, 0);
        assert_eq!(y[0], 100, "half brightness");
        assert_eq!(uv[0], 200, "chroma must not move when only dimming");
    }

    /// Full desaturation is grey, which is chroma at the midpoint -- not zero.
    /// Writing 0 here would tint the whole background green.
    #[test]
    fn full_desaturation_lands_on_neutral_grey() {
        let (w, h, ys, uvs) = (16, 8, 16, 16);
        let mut y = vec![120u8; ys * h];
        let mut uv = vec![30u8; uvs * h / 2];
        tint(&mut y, &mut uv, w, h, ys, uvs, 0, 100);
        assert_eq!(uv[0], 128, "grey is 128, not 0");
        assert_eq!(y[0], 120, "luma must not move when only desaturating");
    }

    /// Blurring a flat plane must not change it -- the running sums, the edge
    /// clamping and the reciprocal all have to agree for this to hold.
    #[test]
    fn blur_of_a_flat_plane_is_flat() {
        let (w, h, stride) = (64, 48, 70);
        let mut plane = vec![0u8; stride * h];
        for row in 0..h {
            plane[row * stride..row * stride + w].fill(200);
        }
        let mut scratch = vec![0u8; w * h];
        box_blur(&mut plane, &mut scratch, w, h, stride, 5);
        for row in 0..h {
            for col in 0..w {
                assert_eq!(plane[row * stride + col], 200, "at {col},{row}");
            }
        }
    }

    /// A flat colour must come out the colour it went in. Through the
    /// single-channel blur this red came out 159,171, because every U was
    /// averaged with the Vs beside it.
    #[test]
    fn chroma_blur_keeps_u_and_v_apart() {
        let (w, h) = (64, 24);
        for (u, v) in [(90u8, 240u8), (200, 90), (110, 150)] {
            let mut plane = vec![0u8; w * h];
            for pair in plane.chunks_exact_mut(2) {
                pair.copy_from_slice(&[u, v]);
            }
            let mut scratch = vec![0u8; w * h];
            box_blur_uv(&mut plane, &mut scratch, w, h, w, 6);
            for (i, pair) in plane.chunks_exact(2).enumerate() {
                assert_eq!((pair[0], pair[1]), (u, v), "pair {i} of {u},{v}");
            }
        }
    }

    /// Interleaved, the chroma blur must be exactly the single-channel blur run
    /// on U and V as planes of their own -- and leave the stride's padding be.
    #[test]
    fn chroma_blur_matches_blurring_u_and_v_separately() {
        let (w, h, stride) = (38, 15, 42);
        let n = w / 2;
        let mut plane = vec![0u8; stride * h];
        for row in 0..h {
            for col in 0..stride {
                plane[row * stride + col] = ((row * 29 + col * 53) % 256) as u8;
            }
        }
        for r in [1, 3, 7, 100] {
            let mut want = plane.clone();
            for c in 0..2 {
                let mut channel: Vec<u8> =
                    (0..n * h).map(|i| plane[(i / n) * stride + (i % n) * 2 + c]).collect();
                let mut scratch = vec![0u8; n * h];
                box_blur(&mut channel, &mut scratch, n, h, n, r);
                for i in 0..n * h {
                    want[(i / n) * stride + (i % n) * 2 + c] = channel[i];
                }
            }
            let mut got = plane.clone();
            let mut scratch = vec![0u8; w * h];
            box_blur_uv(&mut got, &mut scratch, w, h, stride, r);
            assert_eq!(got, want, "radius {r}");
        }
    }

    /// The fast blur must match a naive box blur with the same edge clamping.
    #[test]
    fn blur_matches_a_naive_box() {
        let (w, h, stride, r) = (37, 29, 41, 4);
        let mut plane = vec![0u8; stride * h];
        for row in 0..h {
            for col in 0..w {
                plane[row * stride + col] = ((row * 7 + col * 13) % 256) as u8;
            }
        }
        let original = plane.clone();

        let mut naive = vec![0u8; w * h];
        for row in 0..h {
            for col in 0..w {
                let mut sum = 0u32;
                for dy in -(r as isize)..=(r as isize) {
                    for dx in -(r as isize)..=(r as isize) {
                        let y = (row as isize + dy).clamp(0, h as isize - 1) as usize;
                        let x = (col as isize + dx).clamp(0, w as isize - 1) as usize;
                        sum += u32::from(original[y * stride + x]);
                    }
                }
                naive[row * w + col] = (sum / ((2 * r + 1) * (2 * r + 1)) as u32) as u8;
            }
        }

        let mut scratch = vec![0u8; w * h];
        box_blur(&mut plane, &mut scratch, w, h, stride, r);
        for row in 0..h {
            for col in 0..w {
                let got = i32::from(plane[row * stride + col]);
                let want = i32::from(naive[row * w + col]);
                // Separable rounds twice where the naive version rounds once.
                assert!((got - want).abs() <= 2, "at {col},{row}: {got} vs {want}");
            }
        }
    }
}
