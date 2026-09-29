//! Does every model load and run on every device this machine has?
//!
//!     cargo run --release --example devices
//!
//! Loads each installed model on NPU, GPU and CPU in turn, runs a few
//! inferences, and reports the device, the time, and how far its mask is from
//! the CPU's on the same input -- FP16 on an NPU is not bit-identical to FP32
//! on a CPU, and a fallback should look the same, not merely run. Needs no
//! camera. A device the machine lacks is reported as absent rather than failing
//! the run, which is the same thing the daemon does when it falls back down the
//! chain.
//!
//! With no argument the input is a smooth gradient with a soft-edged blob, which
//! proves the models load and run but gives them nobody to find -- every mask is
//! zero, and zero against zero shows nothing about whether devices agree. Give
//! it a real frame to compare them properly:
//!
//!     cargo run --release --example devices -- frame.nv12 1280x720
//!
//! where the file is one raw NV12 frame, for instance from
//! `gst-launch-1.0 v4l2src num-buffers=30 ! ... ! filesink` -- take the last.

use std::time::Instant;
use studio_effects_daemon::nv12::{write_model_input, NET};
use studio_effects_daemon::segmenter::Segmenter;

/// One raw NV12 frame and its size, from the command line.
fn frame_from_args() -> Option<(Vec<u8>, usize, usize)> {
    let mut args = std::env::args().skip(1);
    let path = args.next()?;
    let (w, h) = args.next()?.split_once('x').map(|(w, h)| (w.parse().ok(), h.parse().ok()))?;
    let (w, h): (usize, usize) = (w?, h?);
    let data = std::fs::read(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
    assert!(data.len() >= w * h * 3 / 2, "{path} is smaller than one {w}x{h} NV12 frame");
    // The last frame, if the file holds several.
    let frame = w * h * 3 / 2;
    let start = data.len() / frame * frame - frame;
    Some((data[start..start + frame].to_vec(), w, h))
}

fn main() {
    let real = frame_from_args();
    println!("input: {}", if real.is_some() { "a real frame" } else { "a synthetic blob (masks will be empty)" });
    let cache = std::env::temp_dir().join("studio-effects-devices-example");
    let cache = cache.to_string_lossy().into_owned();
    let dirs = ["/usr/share/studio-effects/models", "models"];
    let dir = dirs.iter().find(|d| std::path::Path::new(&format!("{d}/segmentation.xml")).exists());
    let Some(dir) = dir else {
        eprintln!("no models found in {dirs:?}; run from the repository root after tools/convert.py");
        std::process::exit(1);
    };

    for model in ["segmentation", "matting"] {
        let mut reference: Option<Vec<f32>> = None;
        // CPU first, so the others have something to be compared with.
        for device in ["CPU", "NPU", "GPU"] {
            let path = format!("{dir}/{model}.xml");
            let mut seg = match Segmenter::new(&path, &cache, Some(device)) {
                Ok(s) => s,
                Err(e) => {
                    let first = format!("{e:#}").lines().next().unwrap_or("").chars().take(90).collect::<String>();
                    println!("{model:13} {device}: unavailable -- {first}");
                    continue;
                }
            };
            let mut ms = Vec::new();
            let mut last = Vec::new();
            for _ in 0..12 {
                let input = seg.input_buffer().expect("input tensor");
                if let Some((frame, w, h)) = &real {
                    let (y, uv) = frame.split_at(w * h);
                    write_model_input(y, uv, *w, *h, *w, *w, input);
                } else {
                    for c in 0..3 {
                    for y in 0..NET {
                        for x in 0..NET {
                            let (fx, fy) = (x as f32 / NET as f32, y as f32 / NET as f32);
                            let blob = (1.0 - (((fx - 0.5) / 0.3).powi(2) + ((fy - 0.55) / 0.4).powi(2))).clamp(0.0, 1.0);
                            input[c * NET * NET + y * NET + x] =
                                0.15 + 0.3 * fx + 0.1 * c as f32 * fy + 0.45 * blob;
                        }
                    }
                    }
                }
                let t = Instant::now();
                let mask = seg.infer().expect("inference");
                ms.push(t.elapsed().as_secs_f64() * 1e3);
                assert!(mask.iter().all(|v| v.is_finite()), "{model} on {device} produced a non-finite mask");
                last = mask.to_vec();
            }
            ms.sort_by(|a, b| a.total_cmp(b));
            let mean = last.iter().sum::<f32>() / last.len() as f32;
            let vs_cpu = match &reference {
                Some(cpu) => {
                    let worst = last.iter().zip(cpu).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
                    let avg = last.iter().zip(cpu).map(|(a, b)| (a - b).abs()).sum::<f32>() / last.len() as f32;
                    format!(", vs CPU: worst {worst:.3}, mean {avg:.4}")
                }
                None => String::new(),
            };
            println!("{model:13} {device}: ok, {:.2} ms median, mask mean {mean:.3}{vs_cpu}", ms[ms.len() / 2]);
            if reference.is_none() {
                reference = Some(last);
            }
        }
    }
}
