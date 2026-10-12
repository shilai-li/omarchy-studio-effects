//! Decoding a still image once, into the frame's own NV12 layout, and finding
//! the images there are to choose from.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;

/// A replacement background, already scaled and converted to match the frames it
/// will stand behind.
///
/// Decoded once at startup rather than per frame. The composite then costs the
/// same as the blur path -- a plane copy instead of a blur -- so switching
/// effects cannot change whether the daemon holds its frame budget.
pub struct Background {
    pub y: Vec<u8>,
    pub uv: Vec<u8>,
    pub y_stride: usize,
    pub uv_stride: usize,
}

impl Background {
    /// Decode `path`, cropped to the frame's aspect and scaled to fill it.
    ///
    /// Cropped, not letterboxed: a background with black bars reads as a broken
    /// video call, whereas losing the edges of someone's wallpaper does not.
    ///
    /// The path is set as a property rather than written into the pipeline's
    /// description, where a file name with a quote in it -- which a folder of
    /// someone's pictures will eventually have -- would be read as syntax.
    pub fn load(path: &Path, width: u32, height: u32) -> Result<Self> {
        let shown = path.display();
        let desc = format!(
            "filesrc name=file ! decodebin ! videoconvert \
             ! aspectratiocrop aspect-ratio={width}/{height} ! videoscale \
             ! video/x-raw,format=NV12,width={width},height={height} \
             ! appsink name=sink"
        );
        let pipeline = gst::parse::launch(&desc)
            .with_context(|| format!("building a decode pipeline for {shown}"))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow::anyhow!("background pipeline was not a pipeline"))?;
        pipeline
            .by_name("file")
            .context("no filesrc")?
            .set_property("location", path.to_string_lossy().as_ref());
        let sink = pipeline
            .by_name("sink")
            .context("no appsink")?
            .downcast::<gst_app::AppSink>()
            .map_err(|_| anyhow::anyhow!("sink was not an appsink"))?;

        pipeline.set_state(gst::State::Playing)?;
        // Bounded, and watching the bus: a file that is not an image is a
        // decode error, which ends no stream, and an unbounded pull would wait
        // for it forever.
        let bus = pipeline.bus().context("no bus")?;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let sample = loop {
            if let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(50)) {
                break Ok(sample);
            }
            if let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error])
                && let gst::MessageView::Error(e) = msg.view()
            {
                break Err(anyhow::anyhow!("{}", e.error()));
            }
            if sink.is_eos() || std::time::Instant::now() > deadline {
                break Err(anyhow::anyhow!("no picture came out"));
            }
        };
        let sample = match sample {
            Ok(sample) => sample,
            Err(e) => {
                let _ = pipeline.set_state(gst::State::Null);
                return Err(e).with_context(|| format!("decoding {shown}; is it an image GStreamer can read?"));
            }
        };

        let info = gst_video::VideoInfo::from_caps(sample.caps().context("no caps")?)?;
        let buffer = sample.buffer().context("no buffer")?;
        let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)?;

        let loaded = Self {
            y: frame.plane_data(0)?.to_vec(),
            uv: frame.plane_data(1)?.to_vec(),
            y_stride: info.stride()[0] as usize,
            uv_stride: info.stride()[1] as usize,
        };

        drop(frame);
        pipeline.set_state(gst::State::Null)?;
        Ok(loaded)
    }

    /// Copy into a frame's planes row by row.
    ///
    /// Row by row because the decoder and the camera pad their rows
    /// independently; a flat copy silently shears the image when they differ.
    pub fn paint(
        &self,
        y_out: &mut [u8],
        uv_out: &mut [u8],
        w: usize,
        h: usize,
        y_stride: usize,
        uv_stride: usize,
    ) {
        for row in 0..h {
            let (src, dst) = (row * self.y_stride, row * y_stride);
            y_out[dst..dst + w].copy_from_slice(&self.y[src..src + w]);
        }
        for row in 0..h / 2 {
            let (src, dst) = (row * self.uv_stride, row * uv_stride);
            uv_out[dst..dst + w].copy_from_slice(&self.uv[src..src + w]);
        }
    }
}

/// One image the panel can choose, named for its file without the extension.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub name: String,
    pub path: PathBuf,
}

/// Where backgrounds are looked for when the config names no folder.
pub fn default_dir() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())).join(".config"))
        .join("studio-effects/backgrounds")
}

/// What the decoder can read with the plugins the package depends on: jpegdec
/// and pngdec, both in -good. WebP and HEIC need -bad and are left out rather
/// than offered and then refused.
const EXTENSIONS: [&str; 3] = ["jpg", "jpeg", "png"];

/// Every background there is to choose: the configured image first, then the
/// folder's images by name.
///
/// Read again on every socket command rather than watched, so an image dropped
/// into the folder is there the next time the panel opens, and nothing runs
/// while nobody is asking. A folder of a few dozen files costs microseconds.
pub fn scan(configured: Option<&Path>, dir: Option<&Path>) -> Vec<Choice> {
    let mut found: Vec<Choice> = configured
        .filter(|p| p.is_file())
        .and_then(|p| choice(p, false))
        .into_iter()
        .collect();
    let mut listed: Vec<Choice> = dir
        .and_then(|d| std::fs::read_dir(d).ok())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter_map(|p| choice(&p, true))
        .collect();
    // By name, then by file, so which of beach.jpg and beach.png is offered
    // does not depend on the order the filesystem lists them in.
    listed.sort_by(|a, b| {
        (a.name.to_lowercase(), &a.name, &a.path).cmp(&(b.name.to_lowercase(), &b.name, &b.path))
    });
    for c in listed {
        // One entry per name: the panel shows names, and two rows reading
        // "beach" would be one too many. The configured image wins its name,
        // and is not listed twice when it also lives in the folder.
        if found.iter().any(|f| f.name == c.name || same_file(&f.path, &c.path)) {
            continue;
        }
        found.push(c);
    }
    found
}

fn choice(path: &Path, check_extension: bool) -> Option<Choice> {
    if check_extension {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        if !EXTENSIONS.contains(&ext.as_str()) {
            return None;
        }
    }
    let name = path.file_stem()?.to_str()?;
    // Hidden files are editors' and thumbnailers' leftovers, not choices. A name
    // the line protocol cannot carry -- a control character, or a space at
    // either end, which the socket trims -- is left out rather than offered and
    // then refused.
    if name.is_empty() || name.starts_with('.') || name.trim() != name || name.chars().any(char::is_control) {
        return None;
    }
    Some(Choice { name: name.to_string(), path: path.to_path_buf() })
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// A background being decoded off the frame loop. Decoding a 4K photograph
/// takes long enough to drop frames, and a switch made mid-call must not be
/// one; the old image stays on screen until the new one is ready.
pub struct Loading {
    pub path: PathBuf,
    done: std::sync::mpsc::Receiver<Result<Background>>,
}

impl Loading {
    pub fn start(path: PathBuf, width: u32, height: u32) -> Self {
        let (tx, done) = std::sync::mpsc::channel();
        let from = path.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Background::load(&from, width, height));
        });
        Self { path, done }
    }

    /// The result, once there is one.
    pub fn poll(&self) -> Option<Result<Background>> {
        match self.done.try_recv() {
            Ok(result) => Some(result),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Err(anyhow::anyhow!("the decoder thread died"))),
        }
    }

    /// Wait for it. Only at startup, where the first frame is not out yet.
    pub fn wait(self, timeout: Duration) -> Result<Background> {
        self.done.recv_timeout(timeout).context("decoding the background took too long")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(files: &[&str]) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "studio-backgrounds-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in files {
            std::fs::write(dir.join(f), b"").unwrap();
        }
        dir
    }

    fn names(found: &[Choice]) -> Vec<&str> {
        found.iter().map(|c| c.name.as_str()).collect()
    }

    #[test]
    fn a_folder_offers_its_images_by_name() {
        let dir = folder(&[
            "Office.jpg", "beach.PNG", "city.jpeg", "notes.txt", ".hidden.jpg", "anim.webp",
            "two words.jpg", " padded.jpg",
        ]);
        std::fs::create_dir_all(dir.join("sub.jpg")).unwrap();
        let found = scan(None, Some(&dir));
        assert_eq!(names(&found), ["beach", "city", "Office", "two words"]);
        assert_eq!(found[0].path, dir.join("beach.PNG"));
    }

    #[test]
    fn the_configured_image_comes_first_and_once() {
        let dir = folder(&["beach.jpg", "beach.png", "zoo.jpg", "aardvark.jpg"]);
        let found = scan(Some(&dir.join("zoo.jpg")), Some(&dir));
        assert_eq!(names(&found), ["zoo", "aardvark", "beach"]);
        assert_eq!(found[2].path, dir.join("beach.jpg"));
    }

    #[test]
    fn nothing_to_offer_is_an_empty_list() {
        assert!(scan(None, None).is_empty());
        assert!(scan(Some(Path::new("/nonexistent/x.jpg")), Some(Path::new("/nonexistent"))).is_empty());
    }

    /// A file named with a quote used to be spliced into the pipeline text.
    #[test]
    fn an_awkward_file_name_still_decodes() -> Result<()> {
        gst::init()?;
        let dir = folder(&[]);
        let path = dir.join(r#"it's "quoted".png"#);
        let make = format!(
            "videotestsrc num-buffers=1 ! video/x-raw,width=64,height=36 ! pngenc ! filesink location={}",
            dir.join("made.png").display()
        );
        let p = gst::parse::launch(&make)?;
        p.set_state(gst::State::Playing)?;
        p.bus().unwrap().timed_pop_filtered(gst::ClockTime::from_seconds(5), &[gst::MessageType::Eos]);
        p.set_state(gst::State::Null)?;
        std::fs::rename(dir.join("made.png"), &path)?;
        let bg = Loading::start(path, 32, 18).wait(Duration::from_secs(10))?;
        assert!(bg.y.len() >= 32 * 18);
        Ok(())
    }

    #[test]
    fn a_file_that_is_not_an_image_is_an_error_not_a_hang() {
        let dir = folder(&["broken.jpg"]);
        std::fs::write(dir.join("broken.jpg"), b"not a jpeg").unwrap();
        gst::init().unwrap();
        assert!(Loading::start(dir.join("broken.jpg"), 32, 18).wait(Duration::from_secs(15)).is_err());
    }
}
