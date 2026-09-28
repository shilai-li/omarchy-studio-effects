//! How long `Preview::offer` holds up the frame loop, and what it costs.
//!
//!     cargo run --release --example preview_cost
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

const OFFERS: usize = 60;

/// utime + stime for the whole process, every GStreamer thread included.
fn cpu_seconds() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").expect("reading /proc/self/stat");
    let fields: Vec<&str> = stat.rsplit_once(')').expect("a stat line").1.split_whitespace().collect();
    let ticks: f64 = fields[11].parse::<f64>().unwrap() + fields[12].parse::<f64>().unwrap();
    ticks / 100.0
}

fn main() -> Result<()> {
    let dir = std::env::temp_dir().join(format!("preview-cost-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    // SAFETY: nothing else is running yet -- this is before gst::init, which
    // is what starts threads.
    unsafe { std::env::set_var("XDG_RUNTIME_DIR", &dir) };
    gst::init()?;

    for (w, h) in [(1280u32, 720u32), (1920, 1080)] {
        let info = gst_video::VideoInfo::builder(gst_video::VideoFormat::Nv12, w, h)
            .fps(gst::Fraction::new(30, 1))
            .build()?;
        let mut frame = gst::Buffer::with_size(info.size())?;
        {
            let mut map = frame.get_mut().expect("a new buffer is writable").map_writable()?;
            for (i, v) in map.as_mut_slice().iter_mut().enumerate() {
                *v = (((i * 2654435761) >> 11) as u8) / 2 + 64;
            }
        }

        let mut preview = Preview::new(h as i32, w as i32)?;
        let mut offer = |held: &mut Vec<f64>| -> Result<()> {
            // Just past the interval, so every offer is one that encodes.
            std::thread::sleep(Duration::from_millis(101));
            let t = Instant::now();
            preview.offer(&frame, &info)?;
            held.push(t.elapsed().as_secs_f64() * 1e3);
            Ok(())
        };
        for _ in 0..5 {
            offer(&mut Vec::new())?;
        }
        let (mut held, cpu) = (Vec::new(), cpu_seconds());
        for _ in 0..OFFERS {
            offer(&mut held)?;
        }
        let cpu = cpu_seconds() - cpu;
        held.sort_by(|a, b| a.total_cmp(b));
        println!(
            "{w}x{h}: frame loop held {:.2} ms median, {:.2} ms worst; {:.2} ms of CPU per encode",
            held[held.len() / 2],
            held[held.len() - 1],
            1e3 * cpu / OFFERS as f64,
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
