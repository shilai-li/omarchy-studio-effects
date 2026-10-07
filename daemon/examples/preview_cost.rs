//! How long `Preview::offer` holds up the frame loop, and what it costs.
//!
//!     cargo run --release --example preview_cost
//!     cargo run --release --example preview_cost -- 60
//!
//! The daemon's timing line cannot answer this: it times prep, infer, blur,
//! blend and the resample, and the preview runs after all of them. Needs no
//! camera and no daemon. The JPEGs go to a temporary directory, not to the
//! runtime directory a bar widget might be reading.

use anyhow::Result;
use gstreamer as gst;
use gstreamer_video as gst_video;
use std::time::{Duration, Instant};
use studio_effects_daemon::preview::Preview;

/// utime + stime for the whole process, every GStreamer thread included.
fn cpu_seconds() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").expect("reading /proc/self/stat");
    let fields: Vec<&str> = stat
        .rsplit_once(')')
        .expect("a stat line")
        .1
        .split_whitespace()
        .collect();
    let ticks: f64 = fields[11].parse::<f64>().unwrap() + fields[12].parse::<f64>().unwrap();
    ticks / 100.0
}

fn main() -> Result<()> {
    let fps = std::env::args()
        .nth(1)
        .map(|arg| arg.parse::<u32>())
        .transpose()?
        .unwrap_or(30);
    anyhow::ensure!((1..=120).contains(&fps), "FPS must be between 1 and 120");
    let offers = fps as usize * 2;
    let interval = Duration::from_secs_f64(1.0 / f64::from(fps));
    let dir = std::env::temp_dir().join(format!("preview-cost-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    // SAFETY: nothing else is running yet -- this is before gst::init, which
    // is what starts threads.
    unsafe { std::env::set_var("XDG_RUNTIME_DIR", &dir) };
    gst::init()?;

    for (w, h) in [(1280u32, 720u32), (1920, 1080)] {
        let info = gst_video::VideoInfo::builder(gst_video::VideoFormat::Nv12, w, h)
            .fps(gst::Fraction::new(fps as i32, 1))
            .build()?;
        let mut frame = gst::Buffer::with_size(info.size())?;
        {
            let mut map = frame
                .get_mut()
                .expect("a new buffer is writable")
                .map_writable()?;
            for (i, v) in map.as_mut_slice().iter_mut().enumerate() {
                *v = (((i * 2654435761) >> 11) as u8) / 2 + 64;
            }
        }

        let mut preview = Preview::new(h as i32, w as i32)?;
        let path = preview.path().to_owned();
        let mut next = Instant::now();
        let mut offer = |held: &mut Vec<f64>| -> Result<()> {
            // Absolute deadlines keep the input cadence independent of how
            // long an offer takes, just as the camera does.
            next += interval;
            std::thread::sleep(next.saturating_duration_since(Instant::now()));
            let t = Instant::now();
            preview.offer(&frame, &info)?;
            held.push(t.elapsed().as_secs_f64() * 1e3);
            Ok(())
        };
        for _ in 0..5 {
            offer(&mut Vec::new())?;
        }
        let (mut held, cpu) = (Vec::new(), cpu_seconds());
        let mut modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let mut published = 0;
        for _ in 0..offers {
            offer(&mut held)?;
            let current = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            if current != modified {
                published += 1;
                modified = current;
            }
        }
        let cpu = cpu_seconds() - cpu;
        held.sort_by(|a, b| a.total_cmp(b));
        println!(
            "{w}x{h} at {fps} fps: frame loop held {:.2} ms median, {:.2} ms worst; {:.2} ms of CPU per offer; {published}/{offers} JPEGs published",
            held[held.len() / 2],
            held[held.len() - 1],
            1e3 * cpu / offers as f64,
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
