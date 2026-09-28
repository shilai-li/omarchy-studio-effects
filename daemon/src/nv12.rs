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
///
/// A second round took another quarter to a third off (P-core and E-core), for
/// the same bytes out -- which `blur_is_what_it_was` pins against the version
/// before it. The window is
/// kept inside the row by clamping both of its ends, and that was done on
/// every pixel although only the first and last `r + 1` ever need it; and the
/// vertical pass walked the running sums twice a row, once to write and once
/// to advance.
pub fn box_blur(plane: &mut [u8], scratch: &mut [u8], w: usize, h: usize, stride: usize, r: usize) {
    if r == 0 || w == 0 || h == 0 {
        return;
    }
    let r = r.min(w - 1).min(h - 1);
    let avg = window_average(r);

    // Horizontal: plane -> scratch, packed to width so the vertical pass can
    // scan rows without the stride's padding in the way.
    for row in 0..h {
        blur_row::<1>(&plane[row * stride..row * stride + w], &mut scratch[row * w..(row + 1) * w], r, avg);
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
        blur_row::<2>(
            &plane[row * stride..row * stride + 2 * n],
            &mut scratch[row * 2 * n..(row + 1) * 2 * n],
            r,
            avg,
        );
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
///
/// In u32, which gives the same answer as the u64 it replaced and is half the
/// lane width to vectorise: a sum is at most 255 * window, so sum * recip + 2^23
/// stays under 2^32 for any window below 65,793 -- a radius of 32,896 rows.
fn window_average(r: usize) -> impl Fn(u32) -> u8 + Copy {
    let window = (2 * r + 1) as u32;
    assert!(window < 65_793, "a blur radius of {r} is past what the average can hold");
    let recip = ((1u32 << 24) + window / 2) / window;
    move |sum: u32| ((sum * recip + (1 << 23)) >> 24) as u8
}

/// One row of the horizontal pass, over `C` interleaved channels: one for
/// luma, two for chroma.
///
/// The window is clamped to the row at both ends, and only the first and last
/// `r + 1` samples ever need it, so the row is walked as those two edges and an
/// interior that indexes directly.
#[inline(always)]
fn blur_row<const C: usize>(src: &[u8], dst: &mut [u8], r: usize, avg: impl Fn(u32) -> u8) {
    let n = src.len() / C;
    let last = n - 1;
    for c in 0..C {
        let at = |x: usize| u32::from(src[x * C + c]);
        let mut sum = at(0) * (r + 1) as u32;
        for x in 1..=r {
            sum += at(x.min(last));
        }
        // Left edge: the sample leaving the window is always the first.
        let p1 = (r + 1).min(n);
        for x in 0..p1 {
            dst[x * C + c] = avg(sum);
            sum += at((x + r + 1).min(last));
            sum -= at(0);
        }
        // Interior: both ends of the window inside the row. A row narrower
        // than the window has none, and the slices below would start past its
        // end.
        let p2 = n.saturating_sub(r + 1).max(p1);
        if C == 1 && p1 < p2 {
            let adds = &src[p1 + r + 1..p2 + r + 1];
            let subs = &src[p1 - r..p2 - r];
            for ((d, &a), &s) in dst[p1..p2].iter_mut().zip(adds).zip(subs) {
                *d = avg(sum);
                sum = sum + u32::from(a) - u32::from(s);
            }
        } else {
            for x in p1..p2 {
                dst[x * C + c] = avg(sum);
                sum = sum + at(x + r + 1) - at(x - r);
            }
        }
        // Right edge: the sample entering is always the last.
        for x in p2..n {
            dst[x * C + c] = avg(sum);
            sum += at(last);
            sum -= at(x - r);
        }
    }
}

/// Vertical pass, scratch -> plane: one running sum per column, advanced a
/// whole row at a time -- written out and advanced in the same walk over the
/// sums, rather than a walk for each.
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
        let add = &scratch[(y + r + 1).min(h - 1) * w..][..w];
        let sub = &scratch[y.saturating_sub(r) * w..][..w];
        for (((o, s), &a), &b) in out.iter_mut().zip(sums.iter_mut()).zip(add).zip(sub) {
            *o = avg(*s);
            *s = *s + u32::from(a) - u32::from(b);
        }
    }
}

/// The background blur for a whole frame, done at a fraction of its size when
/// the radius is wide enough that nothing shows.
///
/// A box blur costs the same at any radius, so the only way to make it cheaper
/// is fewer pixels: shrink the frame by averaging 2x2 blocks, blur that, and
/// stretch it back with a fixed 3:1 lerp. Against the full-size blur, on a
/// photograph and on a synthetic room at 720p, half size from radius 10 and
/// quarter size from radius 48 keep every pixel within 4 levels, at 49 dB PSNR
/// or better -- invisible in a blurred background, and
/// `reduced_blur_stays_close_to_full_size` holds it there. Below those radii
/// the softening the shrink and the stretch add starts to count (half size at
/// radius 8 reached 5 levels in chroma, quarter size at radius 12 reached 9),
/// so those keep the full frame.
///
/// This was once measured as not cheaper, with the general crop-anywhere
/// resampler doing both scalings. It is the scaling that has to be cheap, and
/// fixed factors are: radius 12 costs 2.6x less at half size, and radius 108 6x
/// less at quarter.
pub struct Blur {
    half: Vec<u8>,
    quarter: Vec<u8>,
    scratch: Vec<u8>,
    stretch: Vec<u16>,
}

impl Blur {
    pub fn new(w: usize, h: usize) -> Self {
        Self {
            half: vec![0; w * h / 4],
            quarter: vec![0; w * h / 16],
            scratch: vec![0; w * h],
            stretch: Vec::new(),
        }
    }

    /// How far to shrink a `w` x `h` frame for luma radius `r`: 1, 2 or 4.
    ///
    /// Only by factors both planes divide into exactly -- chroma is half the
    /// size again -- so the stretch lands back on every row and column.
    pub fn factor(w: usize, h: usize, r: usize) -> usize {
        let fits = |f: usize| w >= 2 * f && h >= 2 * f && w % (2 * f) == 0 && h % (2 * f) == 0;
        if r >= 48 && fits(4) {
            4
        } else if r >= 10 && fits(2) {
            2
        } else {
            1
        }
    }

    /// Blur both planes in place, `passes` times, luma at radius `r`.
    ///
    /// Chroma is half the resolution, so half the radius -- and two channels,
    /// which `box_blur` must never be given.
    pub fn apply(
        &mut self,
        y: &mut [u8],
        uv: &mut [u8],
        w: usize,
        h: usize,
        y_stride: usize,
        uv_stride: usize,
        r: usize,
        passes: usize,
    ) {
        let f = Self::factor(w, h, r);
        self.plane::<1>(y, w, h, y_stride, r, passes, f);
        self.plane::<2>(uv, w / 2, h / 2, uv_stride, r / 2, passes, f);
    }

    /// One plane of `C` channels, `w` samples wide, radius `r` in its samples.
    fn plane<const C: usize>(&mut self, p: &mut [u8], w: usize, h: usize, stride: usize, r: usize, passes: usize, f: usize) {
        let blur = |q: &mut [u8], scratch: &mut [u8], w: usize, h: usize, stride: usize, r: usize| {
            for _ in 0..passes {
                if C == 1 {
                    box_blur(q, scratch, w, h, stride, r);
                } else {
                    box_blur_uv(q, scratch, 2 * w, h, stride, r);
                }
            }
        };
        match f {
            4 => {
                let (w1, h1) = halve::<C>(p, w, h, stride, &mut self.half);
                let (w2, h2) = halve::<C>(&self.half, w1, h1, w1 * C, &mut self.quarter);
                blur(&mut self.quarter, &mut self.scratch, w2, h2, w2 * C, r / 4);
                double::<C>(&self.quarter, w2, h2, &mut self.half, w1 * C, &mut self.stretch);
                double::<C>(&self.half, w1, h1, p, stride, &mut self.stretch);
            }
            2 => {
                let (w1, h1) = halve::<C>(p, w, h, stride, &mut self.half);
                blur(&mut self.half, &mut self.scratch, w1, h1, w1 * C, r / 2);
                double::<C>(&self.half, w1, h1, p, stride, &mut self.stretch);
            }
            _ => blur(p, &mut self.scratch, w, h, stride, r),
        }
    }
}

/// Halve a plane of `C` interleaved channels, `w` samples wide, into `dst`
/// packed: each sample the rounded mean of a 2x2 block. A flat plane stays
/// exactly flat, (4v + 2) >> 2 being v.
fn halve<const C: usize>(src: &[u8], w: usize, h: usize, stride: usize, dst: &mut [u8]) -> (usize, usize) {
    let (ow, oh) = (w / 2, h / 2);
    for y in 0..oh {
        let top = &src[2 * y * stride..][..2 * ow * C];
        let bottom = &src[(2 * y + 1) * stride..][..2 * ow * C];
        let out = &mut dst[y * ow * C..][..ow * C];
        for ((o, a), b) in out.chunks_exact_mut(C).zip(top.chunks_exact(2 * C)).zip(bottom.chunks_exact(2 * C)) {
            for c in 0..C {
                let sum = u16::from(a[c]) + u16::from(a[C + c]) + u16::from(b[c]) + u16::from(b[C + c]);
                o[c] = ((sum + 2) >> 2) as u8;
            }
        }
    }
    (ow, oh)
}

/// Double a packed plane back up into `dst`, bilinearly, sample centres
/// aligned: output sample 2x sits a quarter of a sample left of x, and 2x + 1 a
/// quarter right, so every output is a 3:1 mix of two inputs on each axis. The
/// edges repeat. A flat plane stays exactly flat.
fn double<const C: usize>(lo: &[u8], ow: usize, oh: usize, dst: &mut [u8], stride: usize, mix: &mut Vec<u16>) {
    let lw = ow * C;
    mix.resize(lw, 0);
    for y in 0..oh * 2 {
        // Vertical 3:1, kept at 4x scale for the horizontal one to finish.
        let near = y / 2;
        let far = if y % 2 == 0 { near.saturating_sub(1) } else { (near + 1).min(oh - 1) };
        for ((m, &a), &b) in mix.iter_mut().zip(&lo[near * lw..][..lw]).zip(&lo[far * lw..][..lw]) {
            *m = 3 * u16::from(a) + u16::from(b);
        }
        let out = &mut dst[y * stride..][..2 * lw];
        for x in [0, ow - 1] {
            let (left, right) = (x.saturating_sub(1), (x + 1).min(ow - 1));
            for c in 0..C {
                let centre = 3 * mix[x * C + c];
                out[2 * x * C + c] = ((centre + mix[left * C + c] + 8) >> 4) as u8;
                out[(2 * x + 1) * C + c] = ((centre + mix[right * C + c] + 8) >> 4) as u8;
            }
        }
        if ow > 2 {
            let inner = &mut out[2 * C..2 * (ow - 1) * C];
            let (left, centre, right) = (&mix[..(ow - 2) * C], &mix[C..(ow - 1) * C], &mix[2 * C..]);
            for (((o, l), m), r) in inner
                .chunks_exact_mut(2 * C)
                .zip(left.chunks_exact(C))
                .zip(centre.chunks_exact(C))
                .zip(right.chunks_exact(C))
            {
                for c in 0..C {
                    let centre = 3 * m[c];
                    o[c] = ((centre + l[c] + 8) >> 4) as u8;
                    o[C + c] = ((centre + r[c] + 8) >> 4) as u8;
                }
            }
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
    /// One source row's worth of the vertical lerp, before the horizontal one.
    column_mix: Vec<u16>,
}

impl Resampler {
    pub fn new(width: usize) -> Self {
        Self {
            x_idx: vec![0; width],
            x_next: vec![0; width],
            x_frac: vec![0; width],
            width,
            column_mix: Vec::new(),
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
    ///
    /// Vertical first, over the whole source span as one contiguous run, then
    /// the horizontal gather from that single row: two loads a pixel where
    /// mixing four source pixels per output took four, and the vertical half
    /// vectorises. Bilinear is the same polynomial whichever axis goes first
    /// and neither order rounds in between, so the bytes are the ones the
    /// four-tap version made -- `resampling_is_what_it_was` holds it to that --
    /// at 1.6-1.8x the speed.
    pub fn luma(
        &mut self,
        src: &[u8],
        dst: &mut [u8],
        src_stride: usize,
        dst_stride: usize,
        height: usize,
        top: f32,
        span: f32,
        src_rows: usize,
    ) {
        // The tables only ever step forward, so this is every column read.
        let lo = self.x_idx[0] as usize;
        let hi = self.x_next[self.width - 1] as usize + 1;
        self.column_mix.resize(hi - lo, 0);
        for row in 0..height {
            let (y0, y1, yf) = Self::rows_for(row, height, top, span, src_rows);
            self.mix_rows(&src[y0 * src_stride + lo..y0 * src_stride + hi], &src[y1 * src_stride + lo..y1 * src_stride + hi], yf);

            let out = &mut dst[row * dst_stride..row * dst_stride + self.width];
            for (col, o) in out.iter_mut().enumerate() {
                let (x0, x1) = (self.x_idx[col] as usize - lo, self.x_next[col] as usize - lo);
                *o = Self::lerp_mixed(self.column_mix[x0], self.column_mix[x1], self.x_frac[col]);
            }
        }
    }

    /// Resample an interleaved chroma plane, where U and V move together and
    /// share one set of weights. Vertical first, as `luma`.
    pub fn chroma(
        &mut self,
        src: &[u8],
        dst: &mut [u8],
        src_stride: usize,
        dst_stride: usize,
        height: usize,
        top: f32,
        span: f32,
        src_rows: usize,
    ) {
        let lo = self.x_idx[0] as usize * 2;
        let hi = self.x_next[self.width - 1] as usize * 2 + 2;
        self.column_mix.resize(hi - lo, 0);
        for row in 0..height {
            let (y0, y1, yf) = Self::rows_for(row, height, top, span, src_rows);
            self.mix_rows(&src[y0 * src_stride + lo..y0 * src_stride + hi], &src[y1 * src_stride + lo..y1 * src_stride + hi], yf);

            let out = &mut dst[row * dst_stride..row * dst_stride + 2 * self.width];
            for (col, pair) in out.chunks_exact_mut(2).enumerate() {
                let (x0, x1) = (self.x_idx[col] as usize * 2 - lo, self.x_next[col] as usize * 2 - lo);
                let xf = self.x_frac[col];
                for b in 0..2 {
                    pair[b] = Self::lerp_mixed(self.column_mix[x0 + b], self.column_mix[x1 + b], xf);
                }
            }
        }
    }

    /// The vertical lerp of two source rows, kept at 16 bits: at most 255 * 256,
    /// since the weights sum to 256, and not rounded, since the horizontal
    /// lerp still has to be applied on top.
    #[inline(always)]
    fn mix_rows(&mut self, top: &[u8], bottom: &[u8], yf: u32) {
        let (yf, keep) = (yf as u16, 256 - yf as u16);
        for ((m, &a), &b) in self.column_mix.iter_mut().zip(top).zip(bottom) {
            *m = u16::from(a) * keep + u16::from(b) * yf;
        }
    }

    /// The horizontal lerp between two vertically mixed columns, and the one
    /// rounding of the whole resample.
    #[inline(always)]
    fn lerp_mixed(a: u16, b: u16, xf: u32) -> u8 {
        ((u32::from(a) * (256 - xf) + u32::from(b) * xf) >> 16) as u8
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
///
/// In u16, because everything fits: the mask lerp is at most 255 * 256, since
/// its weights sum to 256, and the blend at most 255 * 255, since a and
/// 255 - a do. Half the lane width is twice the pixels per instruction, and the
/// same bytes -- 3.4x on this machine's baseline build, where the u32 version
/// left the compiler emulating a 32-bit multiply it has no instruction for.
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
        let (vf, keep) = (vf as u16, 256 - vf as u16);
        let base = row * stride;
        let fg = &fg[base..base + w];
        let bg = &mut bg[base..base + w];
        for (((b, &f), &a0), &a1) in bg.iter_mut().zip(fg).zip(&m0[..w]).zip(&m1[..w]) {
            let a = (u16::from(a0) * keep + u16::from(a1) * vf) >> 8;
            *b = ((u16::from(f) * a + u16::from(*b) * (255 - a)) / 255) as u8;
        }
    }
}

/// Blend the interleaved chroma plane, which is half resolution in both axes.
///
/// The band is built at luma width, so chroma column `c` reads band column
/// `2 * c` -- no second upscale, and the U and V bytes of a pixel share one
/// mask value rather than sampling it twice. Taken as pairs of the band, the
/// same stride as the pairs of chroma, so the compiler sees one regular walk.
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
        let (vf, keep) = (vf as u16, 256 - vf as u16);
        let base = row * stride;
        let fg = &fg[base..base + 2 * w];
        let bg = &mut bg[base..base + 2 * w];
        for (((b, f), a0), a1) in bg
            .chunks_exact_mut(2)
            .zip(fg.chunks_exact(2))
            .zip(m0[..2 * w].chunks_exact(2))
            .zip(m1[..2 * w].chunks_exact(2))
        {
            let a = (u16::from(a0[0]) * keep + u16::from(a1[0]) * vf) >> 8;
            b[0] = ((u16::from(f[0]) * a + u16::from(b[0]) * (255 - a)) / 255) as u8;
            b[1] = ((u16::from(f[1]) * a + u16::from(b[1]) * (255 - a)) / 255) as u8;
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

    /// Deterministic noise, so a failure reproduces.
    fn noise(seed: u32, len: usize) -> Vec<u8> {
        let mut s = seed | 1;
        (0..len)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                (s >> 24) as u8
            })
            .collect()
    }

    /// The box blur before its second round of speed-ups: a clamp on every
    /// pixel, two walks over the sums, the average in u64. Kept to hold the
    /// current one to the same bytes.
    fn reference_box_blur(plane: &mut [u8], scratch: &mut [u8], w: usize, h: usize, stride: usize, r: usize) {
        if r == 0 || w == 0 || h == 0 {
            return;
        }
        let r = r.min(w - 1).min(h - 1);
        let window = (2 * r + 1) as u64;
        let recip = ((1u64 << 24) + window / 2) / window;
        let avg = |sum: u32| ((u64::from(sum) * recip + (1 << 23)) >> 24) as u8;
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

    /// The same bytes as before, on every shape the edges can take: rows
    /// narrower than the window, a single column, padded strides, a radius past
    /// the plane.
    #[test]
    fn blur_is_what_it_was() {
        for (i, &(w, h, stride)) in [(64, 48, 70), (37, 29, 41), (7, 5, 9), (2, 2, 2), (1, 9, 1), (300, 3, 304)]
            .iter()
            .enumerate()
        {
            let plane = noise(i as u32 + 7, stride * h);
            for r in [1, 2, 3, 6, 12, 54, 108, 5000] {
                let (mut want, mut got) = (plane.clone(), plane.clone());
                let mut scratch = vec![0u8; w * h];
                reference_box_blur(&mut want, &mut scratch, w, h, stride, r);
                box_blur(&mut got, &mut scratch, w, h, stride, r);
                assert_eq!(got, want, "{w}x{h} stride {stride} radius {r}");
            }
        }
    }

    /// The four-tap bilinear the vertical-first resample replaced.
    fn reference_resample(r: &Resampler, src: &[u8], dst: &mut [u8], stride: usize, out_stride: usize,
                          height: usize, top: f32, span: f32, src_rows: usize, channels: usize) {
        for row in 0..height {
            let (y0, y1, yf) = Resampler::rows_for(row, height, top, span, src_rows);
            let (r0, r1) = (y0 * stride, y1 * stride);
            for col in 0..r.width {
                let (x0, x1) = (r.x_idx[col] as usize * channels, r.x_next[col] as usize * channels);
                let xf = r.x_frac[col];
                for b in 0..channels {
                    let t = u32::from(src[r0 + x0 + b]) * (256 - xf) + u32::from(src[r0 + x1 + b]) * xf;
                    let d = u32::from(src[r1 + x0 + b]) * (256 - xf) + u32::from(src[r1 + x1 + b]) * xf;
                    dst[row * out_stride + col * channels + b] = ((t * (256 - yf) + d * yf) >> 16) as u8;
                }
            }
        }
    }

    #[test]
    fn resampling_is_what_it_was() {
        let (sw, sh) = (192usize, 108usize);
        let y = noise(3, sw * sh);
        let uv = noise(5, sw * sh / 2);
        for (ow, oh) in [(128usize, 72usize), (192, 108), (64, 36)] {
            for (x, top, cw, ch) in [(0.0f32, 0.0f32, 192.0f32, 108.0f32), (10.0, 5.0, 128.0, 72.0),
                                     (33.3, 9.1, 96.5, 54.2), (150.0, 80.0, 42.0, 28.0)] {
                let mut r = Resampler::new(ow);
                r.aim(x, cw, sw);
                let (mut want, mut got) = (vec![0u8; ow * oh], vec![0u8; ow * oh]);
                reference_resample(&r, &y, &mut want, sw, ow, oh, top, ch, sh, 1);
                r.luma(&y, &mut got, sw, ow, oh, top, ch, sh);
                assert_eq!(got, want, "luma {ow}x{oh} from {x},{top} {cw}x{ch}");

                let mut r = Resampler::new(ow / 2);
                r.aim(x / 2.0, cw / 2.0, sw / 2);
                let (mut want, mut got) = (vec![0u8; ow * oh / 2], vec![0u8; ow * oh / 2]);
                reference_resample(&r, &uv, &mut want, sw, ow, oh / 2, top / 2.0, ch / 2.0, sh / 2, 2);
                r.chroma(&uv, &mut got, sw, ow, oh / 2, top / 2.0, ch / 2.0, sh / 2);
                assert_eq!(got, want, "chroma {ow}x{oh} from {x},{top} {cw}x{ch}");
            }
        }
    }

    /// The blend in u32, as it was before the lanes were narrowed.
    fn reference_blend(fg: &[u8], bg: &mut [u8], up: &MaskUpscaler, w: usize, h: usize, stride: usize,
                       chroma: bool) {
        for row in 0..h {
            let (m0, m1, vf) = if chroma { up.rows(row * 2, h * 2) } else { up.rows(row, h) };
            for col in 0..w {
                let c = if chroma { col * 2 } else { col };
                let a = (u32::from(m0[c]) * (256 - vf) + u32::from(m1[c]) * vf) >> 8;
                for b in 0..if chroma { 2 } else { 1 } {
                    let i = row * stride + c + b;
                    bg[i] = ((u32::from(fg[i]) * a + u32::from(bg[i]) * (255 - a)) / 255) as u8;
                }
            }
        }
    }

    #[test]
    fn blending_is_what_it_was() {
        let (w, h) = (320usize, 180usize);
        let mask: Vec<f32> = noise(9, NET * NET).iter().map(|&v| f32::from(v) / 255.0).collect();
        for aim in [None, Some((40.0f32, 150.0f32, 30.0f32, 120.0f32))] {
            let mut up = MaskUpscaler::new(w);
            if let Some((a, b, c, d)) = aim {
                up.aim(a, b, c, d);
            }
            up.prepare(&mask);
            let (fg, bg) = (noise(11, w * h), noise(13, w * h));
            let (mut want, mut got) = (bg.clone(), bg.clone());
            reference_blend(&fg, &mut want, &up, w, h, w, false);
            blend_luma(&fg, &mut got, &up, w, h, w);
            assert_eq!(got, want, "luma, aimed {aim:?}");

            let (mut want, mut got) = (bg[..w * h / 2].to_vec(), bg[..w * h / 2].to_vec());
            reference_blend(&fg, &mut want, &up, w / 2, h / 2, w, true);
            blend_chroma(&fg, &mut got, &up, w / 2, h / 2, w);
            assert_eq!(got, want, "chroma, aimed {aim:?}");
        }
    }

    /// A frame with something in it for a blur to get wrong: gradients, hard
    /// edges, noise, and chroma that is actually coloured.
    fn room(w: usize, h: usize) -> (Vec<u8>, Vec<u8>) {
        let n = noise(17, w * h);
        let mut y = vec![0u8; w * h];
        for row in 0..h {
            for col in 0..w {
                let edge = if (col / 47 + row / 31) % 3 == 0 { 40 } else { 0 };
                let v = (col * 180 / w + row * 60 / h) as i32 + edge + i32::from(n[row * w + col] % 9) - 4;
                y[row * w + col] = v.clamp(16, 235) as u8;
            }
        }
        let mut uv = vec![0u8; w * h / 2];
        for (i, pair) in uv.chunks_exact_mut(2).enumerate() {
            let (u, v) = [(90, 240), (200, 90), (60, 60), (128, 128)][(i % (w / 2)) * 4 / (w / 2)];
            let jitter = i32::from(n[i] % 5) - 2;
            pair[0] = (u + jitter) as u8;
            pair[1] = (v + jitter) as u8;
        }
        (y, uv)
    }

    /// Worst difference in levels, and PSNR in dB.
    fn difference(a: &[u8], b: &[u8]) -> (u8, f64) {
        let worst = a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
        let mse = a.iter().zip(b).map(|(x, y)| f64::from(x.abs_diff(*y)).powi(2)).sum::<f64>() / a.len() as f64;
        (worst, if mse == 0.0 { f64::INFINITY } else { 10.0 * (255.0 * 255.0 / mse).log10() })
    }

    /// Shrinking and stretching must not move a flat colour by a level. A bias
    /// here would be the 199-for-200 darkening again, on every background.
    #[test]
    fn reduced_blur_of_a_flat_colour_is_that_colour() {
        let (w, h) = (640, 360);
        for r in [12, 108] {
            let mut y = vec![200u8; w * h];
            let mut uv: Vec<u8> = [90u8, 240].repeat(w * h / 4);
            Blur::new(w, h).apply(&mut y, &mut uv, w, h, w, w, r, 2);
            assert!(y.iter().all(|&v| v == 200), "luma moved at radius {r}");
            assert!(uv.chunks_exact(2).all(|p| p == [90, 240]), "chroma moved at radius {r}");
        }
    }

    /// Where it shrinks, the result must stay indistinguishable from the
    /// full-size blur: a few levels at worst, in a picture that is blurred.
    #[test]
    fn reduced_blur_stays_close_to_full_size() {
        let (w, h) = (1280, 720);
        let (y0, uv0) = room(w, h);
        for (r, factor) in [(10, 2), (12, 2), (47, 2), (48, 4), (108, 4)] {
            assert_eq!(Blur::factor(w, h, r), factor, "factor for radius {r}");
            let (mut full_y, mut full_uv) = (y0.clone(), uv0.clone());
            let mut scratch = vec![0u8; w * h];
            for _ in 0..2 {
                box_blur(&mut full_y, &mut scratch, w, h, w, r);
                box_blur_uv(&mut full_uv, &mut scratch, w, h / 2, w, r / 2);
            }
            let (mut y, mut uv) = (y0.clone(), uv0.clone());
            Blur::new(w, h).apply(&mut y, &mut uv, w, h, w, w, r, 2);
            let (worst_y, psnr_y) = difference(&y, &full_y);
            let (worst_uv, psnr_uv) = difference(&uv, &full_uv);
            assert!(worst_y <= 4 && psnr_y >= 49.0, "radius {r}: luma {worst_y} levels, {psnr_y:.1} dB");
            assert!(worst_uv <= 4 && psnr_uv >= 49.0, "radius {r}: chroma {worst_uv} levels, {psnr_uv:.1} dB");
        }
    }

    /// Below the thresholds, or on a frame the factor does not divide, the blur
    /// is the full-size one exactly.
    #[test]
    fn reduced_blur_falls_back_to_full_size() {
        assert_eq!(Blur::factor(1280, 720, 9), 1);
        assert_eq!(Blur::factor(1280, 720, 108), 4);
        assert_eq!(Blur::factor(1282, 720, 108), 1, "1282 does not halve into even chroma");
        assert_eq!(Blur::factor(1284, 720, 108), 2, "1284 halves, but not twice");
        let (w, h) = (322, 180);
        let (y0, uv0) = room(w, h);
        let (mut want_y, mut want_uv) = (y0.clone(), uv0.clone());
        let mut scratch = vec![0u8; w * h];
        for _ in 0..2 {
            box_blur(&mut want_y, &mut scratch, w, h, w, 20);
            box_blur_uv(&mut want_uv, &mut scratch, w, h / 2, w, 10);
        }
        let (mut y, mut uv) = (y0.clone(), uv0.clone());
        Blur::new(w, h).apply(&mut y, &mut uv, w, h, w, w, 20, 2);
        assert_eq!((y, uv), (want_y, want_uv));
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
