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

/// Box blur one plane into `scratch` and back, separably.
///
/// Running-sum, so cost is independent of radius -- the whole reason this is a
/// box blur and not a Gaussian. At the radii a background blur uses there is no
/// detail left to reveal the box shape, so one pass is enough.
pub fn box_blur(plane: &mut [u8], scratch: &mut [u8], w: usize, h: usize, stride: usize, r: usize) {
    if r == 0 || w == 0 || h == 0 {
        return;
    }
    let r = r.min(w - 1).min(h - 1);
    let window = (2 * r + 1) as u32;

    // Horizontal: plane -> scratch, packed to width so the vertical pass strides by w.
    for row in 0..h {
        let src = &plane[row * stride..row * stride + w];
        let dst = &mut scratch[row * w..(row + 1) * w];
        let mut sum: u32 = u32::from(src[0]) * (r + 1) as u32;
        for x in 1..=r {
            sum += u32::from(src[x.min(w - 1)]);
        }
        for x in 0..w {
            dst[x] = (sum / window) as u8;
            sum += u32::from(src[(x + r + 1).min(w - 1)]);
            sum -= u32::from(src[x.saturating_sub(r)]);
        }
    }

    // Vertical: scratch -> plane, restoring the stride.
    for col in 0..w {
        let mut sum: u32 = u32::from(scratch[col]) * (r + 1) as u32;
        for y in 1..=r {
            sum += u32::from(scratch[y.min(h - 1) * w + col]);
        }
        for y in 0..h {
            plane[y * stride + col] = (sum / window) as u8;
            sum += u32::from(scratch[(y + r + 1).min(h - 1) * w + col]);
            sum -= u32::from(scratch[y.saturating_sub(r) * w + col]);
        }
    }
}

/// Bilinear sample of the mask, as a 0..=255 foreground weight.
#[inline]
fn sample_mask(mask: &[f32], fx: f32, fy: f32) -> u32 {
    let x = fx.clamp(0.0, (NET - 1) as f32);
    let y = fy.clamp(0.0, (NET - 1) as f32);
    let (x0, y0) = (x as usize, y as usize);
    let (x1, y1) = ((x0 + 1).min(NET - 1), (y0 + 1).min(NET - 1));
    let (tx, ty) = (x - x0 as f32, y - y0 as f32);

    let top = mask[y0 * NET + x0] * (1.0 - tx) + mask[y0 * NET + x1] * tx;
    let bot = mask[y1 * NET + x0] * (1.0 - tx) + mask[y1 * NET + x1] * tx;
    ((top * (1.0 - ty) + bot * ty).clamp(0.0, 1.0) * 255.0) as u32
}

/// Blend the sharp luma plane over the blurred one, weighted by the mask.
pub fn blend_luma(fg: &[u8], bg: &mut [u8], mask: &[f32], w: usize, h: usize, stride: usize) {
    let sx = (NET - 1) as f32 / w as f32;
    let sy = (NET - 1) as f32 / h as f32;
    for row in 0..h {
        let my = row as f32 * sy;
        let base = row * stride;
        for col in 0..w {
            let a = sample_mask(mask, col as f32 * sx, my);
            let i = base + col;
            bg[i] = ((u32::from(fg[i]) * a + u32::from(bg[i]) * (255 - a)) / 255) as u8;
        }
    }
}

/// Blend the interleaved chroma plane, which is half resolution in both axes.
///
/// Two adjacent bytes are the U and V of one pixel, so they share a mask sample
/// -- taking it once per pair rather than per byte halves the sampling work.
pub fn blend_chroma(fg: &[u8], bg: &mut [u8], mask: &[f32], w: usize, h: usize, stride: usize) {
    let sx = (NET - 1) as f32 / w as f32;
    let sy = (NET - 1) as f32 / h as f32;
    for row in 0..h {
        let my = row as f32 * sy;
        let base = row * stride;
        for col in 0..w {
            let a = sample_mask(mask, col as f32 * sx, my);
            let i = base + col * 2;
            bg[i] = ((u32::from(fg[i]) * a + u32::from(bg[i]) * (255 - a)) / 255) as u8;
            bg[i + 1] = ((u32::from(fg[i + 1]) * a + u32::from(bg[i + 1]) * (255 - a)) / 255) as u8;
        }
    }
}
