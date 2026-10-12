//! How long `Preview::offer` holds up the frame loop, and what it costs.
//!
//!     cargo run --release --example preview_cost
//!     cargo run --release --example preview_cost -- 60
//!     cargo run --release --example preview_cost -- 30 720x405
//!
//! The second argument is the size the panel draws the preview at, in physical
//! pixels; each source is measured without it, as an older widget would ask,
//! and with it. 720x405 is the 360-point panel on a 2x display.
//!
//! "After offer" is how long after `offer` the JPEG's write finished, from the
//! file's mtime: the part of the preview's latency the daemon owns. The panel's
//! decode and the compositor's next frame come after it.
//! The daemon's timing line cannot answer this: it times prep, infer, blur,
//! blend and the resample, and the preview runs after all of them. Needs no
//! camera and no daemon. The JPEGs go to a temporary directory, not to the
//! runtime directory a bar widget might be reading.

use anyhow::Result;
use gstreamer as gst;
use gstreamer_video as gst_video;
use std::time::{Duration, Instant, SystemTime};
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
    let display = std::env::args().nth(2).unwrap_or_else(|| "720x405".into());
    let display = display
        .split_once('x')
        .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
        .ok_or_else(|| anyhow::anyhow!("the display size is WIDTHxHEIGHT"))?;
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

        for target in [None, Some(display)] {
        let mut preview = Preview::new(h as i32, w as i32, target)?;
        let (pw, ph) = preview.size();
        let path = preview.path().to_owned();
        let mut next = Instant::now();
        let mut offered = Vec::new();
        let mut offer = |held: &mut Vec<f64>, offered: &mut Vec<SystemTime>| -> Result<()> {
            // Absolute deadlines keep the input cadence independent of how
            // long an offer takes, just as the camera does.
            next += interval;
            std::thread::sleep(next.saturating_duration_since(Instant::now()));
            let t = Instant::now();
            offered.push(SystemTime::now());
            preview.offer(&frame, &info)?;
            held.push(t.elapsed().as_secs_f64() * 1e3);
            Ok(())
        };
        for _ in 0..5 {
            offer(&mut Vec::new(), &mut Vec::new())?;
        }
        let (mut held, cpu) = (Vec::new(), cpu_seconds());
        let mut modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let mut published = 0;
        let mut delays = Vec::new();
        for _ in 0..offers {
            offer(&mut held, &mut offered)?;
            let current = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            if current != modified {
                published += 1;
                modified = current;
                // Against the newest frame offered before the write: the
                // encoder only ever works on the newest it was given.
                let sent = current.and_then(|at| Some((at, *offered.iter().rev().find(|&&o| o <= at)?)));
                if let Some((at, sent)) = sent {
                    delays.push(at.duration_since(sent).unwrap_or_default().as_secs_f64() * 1e3);
                }
            }
        }
        let cpu = cpu_seconds() - cpu;
        drop(preview);
        held.sort_by(|a, b| a.total_cmp(b));
        delays.sort_by(|a, b| a.total_cmp(b));
        let delay = |q: f64| delays.get(((delays.len() as f64 - 1.0) * q) as usize).copied().unwrap_or(f64::NAN);
        println!(
            "{w}x{h} -> {pw}x{ph} at {fps} fps: frame loop held {:.2} ms median, {:.2} ms worst; {:.2} ms of CPU per offer; {published}/{offers} JPEGs published, written {:.1} ms median after offer ({:.1} worst)",
            held[held.len() / 2],
            held[held.len() - 1],
            1e3 * cpu / offers as f64,
            delay(0.5),
            delay(1.0),
        );
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
