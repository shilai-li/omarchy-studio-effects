//! What each CPU stage of the frame loop costs, on a synthetic frame: no
//! camera, no NPU, no daemon.
//!
//!     cargo build --release --example frame_cost
//!     taskset -c 2 target/release/examples/frame_cost 1920x1080 1280x720 108 3
//!
//! Arguments are capture size, output size, blur radius and passes; the
//! defaults are the service's. Pin it to one core: on a hybrid CPU it otherwise
//! migrates between core types mid-run and reports whichever it landed on. The
//! daemon's timing line measures the same stages, but on a live camera, where
//! the frame rate and the clock move under it -- see AGENTS.md.

use std::time::Instant;
use studio_effects_daemon::nv12::{self, MaskUpscaler, Resampler, NET};

/// Median of 200 runs, in milliseconds.
fn time(mut f: impl FnMut()) -> f64 {
    for _ in 0..20 {
        f();
    }
    let mut samples: Vec<f64> = (0..200)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    samples.sort_by(|a, b| a.total_cmp(b));
    samples[samples.len() / 2]
}

fn size(arg: Option<String>, default: (usize, usize)) -> (usize, usize) {
    arg.and_then(|s| {
        let (w, h) = s.split_once('x')?;
        Some((w.parse().ok()?, h.parse().ok()?))
    })
    .unwrap_or(default)
}

/// Something like a room: gradients, hard edges, sensor noise, and colour.
fn frame(w: usize, h: usize) -> (Vec<u8>, Vec<u8>) {
    let mut seed = 0x9e37_79b9u32;
    let mut noise = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        (seed >> 24) as i32 % 9 - 4
    };
    let mut y = vec![0u8; w * h];
    for row in 0..h {
        for col in 0..w {
            let edge = if (col / 97 + row / 61) % 3 == 0 { 40 } else { 0 };
            y[row * w + col] = ((col * 180 / w + row * 60 / h) as i32 + edge + noise()).clamp(16, 235) as u8;
        }
    }
    let mut uv = vec![0u8; w * h / 2];
    for (i, pair) in uv.chunks_exact_mut(2).enumerate() {
        let (u, v) = [(90, 240), (200, 90), (60, 60), (128, 128)][(i % (w / 2)) * 4 / (w / 2)];
        pair[0] = (u + noise()).clamp(16, 240) as u8;
        pair[1] = (v + noise()).clamp(16, 240) as u8;
    }
    (y, uv)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (cw, ch) = size(args.next(), (1280, 720));
    let (w, h) = size(args.next(), (cw, ch));
    let radius: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(12);
    let passes: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(2);

    let (cy, cuv) = frame(cw, ch);
    let mut input = vec![0f32; 3 * NET * NET];
    let (mut sharp_y, mut sharp_uv) = (vec![0u8; w * h], vec![0u8; w * h / 2]);
    let (mut y, mut uv) = (vec![0u8; w * h], vec![0u8; w * h / 2]);
    let mut scratch = vec![0u8; w * h];

    let prep = time(|| nv12::write_model_input(&cy, &cuv, cw, ch, cw, cw, &mut input));

    let (mut rs, mut rs_uv) = (Resampler::new(w), Resampler::new(w / 2));
    rs.aim(0.0, cw as f32, cw);
    rs_uv.aim(0.0, cw as f32 / 2.0, cw / 2);
    let scaling = (cw, ch) != (w, h);
    let frame_stage = time(|| {
        if scaling {
            rs.luma(&cy, &mut sharp_y, cw, w, h, 0.0, ch as f32, ch);
            rs_uv.chroma(&cuv, &mut sharp_uv, cw, w, h / 2, 0.0, ch as f32 / 2.0, ch / 2);
        } else {
            sharp_y.copy_from_slice(&cy);
            sharp_uv.copy_from_slice(&cuv);
        }
    });

    // The blur works on a copy of the sharp frame, as the daemon's does; the
    // copy is timed on its own and taken off.
    let copy = time(|| {
        y.copy_from_slice(&sharp_y);
        uv.copy_from_slice(&sharp_uv);
    });
    let full_size = time(|| {
        y.copy_from_slice(&sharp_y);
        uv.copy_from_slice(&sharp_uv);
        for _ in 0..passes {
            nv12::box_blur(&mut y, &mut scratch, w, h, w, radius);
            nv12::box_blur_uv(&mut uv, &mut scratch, w, h / 2, w, radius / 2);
        }
    }) - copy;
    let mut blurrer = nv12::Blur::new(w, h);
    let blur = time(|| {
        y.copy_from_slice(&sharp_y);
        uv.copy_from_slice(&sharp_uv);
        blurrer.apply(&mut y, &mut uv, w, h, w, w, radius, passes);
    }) - copy;

    let mask: Vec<f32> = (0..NET * NET)
        .map(|i| {
            let (x, yy) = ((i % NET) as f32 / NET as f32 - 0.5, (i / NET) as f32 / NET as f32 - 0.45);
            (1.5 - (x * x / 0.03 + yy * yy / 0.08)).clamp(0.0, 1.0)
        })
        .collect();
    // The blend's cost does not depend on what it blends over.
    let blurred = (y.clone(), uv.clone());
    let mut up = MaskUpscaler::new(w);
    let blend = time(|| {
        y.copy_from_slice(&blurred.0);
        uv.copy_from_slice(&blurred.1);
        up.prepare(&mask);
        nv12::blend_luma(&sharp_y, &mut y, &up, w, h, w);
        nv12::blend_chroma(&sharp_uv, &mut uv, &up, w / 2, h / 2, w);
    }) - copy;

    println!("capture {cw}x{ch}, output {w}x{h}, blur {radius} x{passes}");
    println!("  prep   {prep:6.3} ms");
    println!("  frame  {frame_stage:6.3} ms  ({})", if scaling { "resample" } else { "copy" });
    println!(
        "  blur   {blur:6.3} ms  (at 1/{} size; {full_size:.3} ms at full size)",
        nv12::Blur::factor(w, h, radius)
    );
    println!("  blend  {blend:6.3} ms  (prepare included)");
    println!("  total  {:6.3} ms", prep + frame_stage + blur + blend);
}
