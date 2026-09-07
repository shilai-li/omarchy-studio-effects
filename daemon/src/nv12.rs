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
    // 24-bit fixed-point reciprocal of the window, so the average is a multiply
    // and a shift rather than a per-pixel divide.
    //
    // Both roundings here are load-bearing. Truncating the reciprocal, or the
    // product, biases every pixel downward: with a window of 11, a flat plane of
    // 200 blurs to 199, so a still background visibly darkens as soon as effects
    // come on. Rounding the reciprocal to nearest and adding a half before the
    // shift keeps a flat plane exactly flat, which `blur_of_a_flat_plane_is_flat`
    // pins down.
    let window = (2 * r + 1) as u64;
    let recip = ((1u64 << 24) + window / 2) / window;
    let avg = |sum: u32| ((u64::from(sum) * recip + (1 << 23)) >> 24) as u8;

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

    // Vertical: scratch -> plane, one running sum per column, advanced a whole
    // row at a time.
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
    /// The 256 mask rows, each stretched to full frame width.
    band: Vec<u8>,
    width: usize,
}

impl MaskUpscaler {
    pub fn new(width: usize) -> Self {
        let mut x_idx = Vec::with_capacity(width);
        let mut x_frac = Vec::with_capacity(width);
        for col in 0..width {
            // Fixed point with an 8-bit fraction, so the lerp stays in integers.
            let fx = col * (NET - 1) * 256 / width;
            x_idx.push((fx >> 8) as u32);
            x_frac.push((fx & 0xff) as u32);
        }
        Self {
            x_idx,
            x_frac,
            band: vec![0; NET * width],
            width,
        }
    }

    /// Stretch every mask row to frame width. Once per frame, before blending.
    pub fn prepare(&mut self, mask: &[f32]) {
        for row in 0..NET {
            let src = &mask[row * NET..(row + 1) * NET];
            let dst = &mut self.band[row * self.width..(row + 1) * self.width];
            for col in 0..self.width {
                let i = self.x_idx[col] as usize;
                let f = self.x_frac[col];
                let a = (src[i].clamp(0.0, 1.0) * 255.0) as u32;
                let b = (src[(i + 1).min(NET - 1)].clamp(0.0, 1.0) * 255.0) as u32;
                dst[col] = ((a * (256 - f) + b * f) >> 8) as u8;
            }
        }
    }

    /// The two band rows bracketing output row `row` of `height`, and the
    /// 0..=256 weight toward the second.
    #[inline]
    fn rows(&self, row: usize, height: usize) -> (&[u8], &[u8], u32) {
        let fy = row * (NET - 1) * 256 / height;
        let (r0, f) = (fy >> 8, (fy & 0xff) as u32);
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
