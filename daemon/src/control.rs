//! A tiny line protocol on a unix socket, so effects can change without a restart.
//!
//! Restarting the service to change a setting drops the camera for a second.
//! On a live call that is a black frame everyone sees, which rules it out as
//! the way a toggle in the bar works.
//!
//! The protocol is one line in, one JSON object out, because the client is a
//! short-lived process spawned by a QML widget: that is the shape Omarchy's
//! plugins already use, and it keeps the widget from having to speak anything
//! richer than "run a command, parse stdout".

use anyhow::{Context, Result};
use clap::ValueEnum;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum, Debug)]
pub enum Effect {
    /// Pass the camera through untouched. Still worth running, because the
    /// output device stays alive and apps keep their selection.
    None,
    /// Blur the background.
    Blur,
    /// Replace the background with an image.
    Replace,
}

impl Effect {
    /// Parse the words the socket and the saved state both use.
    pub fn parse(text: &str) -> Option<Effect> {
        match text.trim().to_ascii_lowercase().as_str() {
            "none" | "off" => Some(Effect::None),
            "blur" => Some(Effect::Blur),
            "replace" => Some(Effect::Replace),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Effect::None => "none",
            Effect::Blur => "blur",
            Effect::Replace => "replace",
        }
    }
}

/// What the control socket may change while the daemon runs.
#[derive(Clone)]
pub struct Settings {
    pub effect: Effect,
    pub blur: usize,
    /// Repeats of the box blur. One is boxy on hard edges; two or three
    /// approximate a Gaussian, at a cost that scales with them.
    pub passes: usize,
    /// How much to darken the background, 0..=100.
    pub dim: u32,
    /// How much colour to drain from the background, 0..=100.
    pub desat: u32,
    /// Track the subject and keep them centred.
    pub framing: bool,
    /// How far the framing may crop in, as a percentage: 160 is 1.6x. A
    /// percentage rather than a float because the socket takes integers, and a
    /// setting the panel steps needs to be one.
    pub zoom: u32,
    /// What `toggle` should return to. Without this, turning effects off and on
    /// again would silently demote a replaced background to a blur.
    pub resume: Effect,
    /// Whether a background image was loaded at startup. `replace` is refused
    /// rather than accepted-and-ignored when there is none.
    pub has_background: bool,
    /// Whether to publish preview JPEGs. Off unless something is watching:
    /// the widget turns it on when its panel opens and off when it closes, so
    /// nothing is encoded for a picture nobody is looking at.
    pub preview: bool,
    /// Which model finds the subject. Live, because comparing two models means
    /// seeing them on the same face in the same light -- a restart apart, the
    /// pose and the lighting have both moved and the comparison is worthless.
    pub model: String,
}

/// Facts the socket reports but cannot change.
pub struct Fixed {
    pub device: String,
    /// Every model installed. Discovered rather than hardcoded, so the panel
    /// offers exactly what can actually be loaded.
    pub models: Vec<String>,
    pub input: String,
    pub output: String,
    pub width: u32,
    pub height: u32,
    /// Where preview frames appear. Reported rather than assumed by the client,
    /// so the two cannot disagree about it.
    pub preview_path: String,
}

pub fn socket_path() -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(dir).join("studio-effects.sock")
}

/// Quotes and backslashes only. Every string that reaches this is ours -- our
/// messages and a path from the environment -- not text a stranger supplied.
fn escape(text: &str) -> String {
    text.replace('\\', r"\\").replace('"', r#"\""#)
}

fn json(settings: &Settings, fixed: &Fixed, error: Option<&str>) -> String {
    let mut out = format!(
        r#"{{"effect":"{}","blur":{},"passes":{},"dim":{},"desat":{},"framing":{},"zoom":{},"device":"{}","model":"{}","models":[{}],"input":"{}","output":"{}","width":{},"height":{},"background":{},"preview":{},"previewPath":"{}""#,
        settings.effect.as_str(),
        settings.blur,
        settings.passes,
        settings.dim,
        settings.desat,
        settings.framing,
        settings.zoom,
        fixed.device,
        escape(&settings.model),
        fixed
            .models
            .iter()
            .map(|m| format!("\"{}\"", escape(m)))
            .collect::<Vec<_>>()
            .join(","),
        fixed.input,
        fixed.output,
        fixed.width,
        fixed.height,
        settings.has_background,
        settings.preview,
        escape(&fixed.preview_path),
    );
    if let Some(message) = error {
        out.push_str(&format!(r#","error":"{}""#, escape(message)));
    }
    out.push('}');
    out
}

fn handle(line: &str, settings: &Mutex<Settings>, fixed: &Fixed) -> String {
    // Whether this command could have changed something worth remembering.
    // `status` and `preview` are excluded: one changes nothing, and the other
    // tracks a panel being open, which is not a preference.
    let persists = !matches!(
        line.split_whitespace().next().unwrap_or("status"),
        "status" | "preview" | ""
    );
    let mut words = line.split_whitespace();
    let verb = words.next().unwrap_or("status");
    let arg = words.next();

    let mut s = settings.lock().expect("settings mutex poisoned");
    let mut error: Option<String> = None;

    match verb {
        "status" => {}
        "effect" => match arg.map(Effect::from_str_lenient) {
            Some(Some(Effect::Replace)) if !s.has_background => {
                error = Some("no background image was loaded; start the daemon with --background".into())
            }
            Some(Some(e)) => {
                if e != Effect::None {
                    s.resume = e;
                }
                s.effect = e;
            }
            _ => error = Some("usage: effect none|blur|replace".into()),
        },
        "toggle" => {
            s.effect = if s.effect == Effect::None {
                s.resume
            } else {
                Effect::None
            };
        }
        "preview" => match arg {
            Some("on") => s.preview = true,
            Some("off") | None => s.preview = false,
            _ => error = Some("usage: preview on|off".into()),
        },
        "blur" => match arg.and_then(|a| a.parse::<usize>().ok()) {
            Some(n) if n <= 200 => s.blur = n,
            _ => error = Some("usage: blur <0-200>".into()),
        },
        "passes" => match arg.and_then(|a| a.parse::<usize>().ok()) {
            Some(n) if (1..=3).contains(&n) => s.passes = n,
            _ => error = Some("usage: passes <1-3>".into()),
        },
        "framing" => match arg {
            Some("on") => s.framing = true,
            Some("off") | None => s.framing = false,
            _ => error = Some("usage: framing on|off".into()),
        },
        "zoom" => match arg.and_then(|a| a.parse::<u32>().ok()) {
            Some(n) if (100..=300).contains(&n) => s.zoom = n,
            _ => error = Some("usage: zoom <100-300>".into()),
        },
        "dim" => match arg.and_then(|a| a.parse::<u32>().ok()) {
            Some(n) if n <= 100 => s.dim = n,
            _ => error = Some("usage: dim <0-100>".into()),
        },
        "desat" => match arg.and_then(|a| a.parse::<u32>().ok()) {
            Some(n) if n <= 100 => s.desat = n,
            _ => error = Some("usage: desat <0-100>".into()),
        },
        "model" => match arg {
            // Only what is installed. A name the daemon cannot load would
            // leave the frame loop rebuilding a segmenter that fails on every
            // frame, which is a far worse outcome than a refusal.
            Some(name) if fixed.models.iter().any(|m| m == name) => s.model = name.to_string(),
            _ => error = Some(format!("usage: model {}", fixed.models.join("|"))),
        },
        _ => error = Some("unknown command; try status, effect, model, toggle, preview, framing, zoom, blur, passes, dim or desat".into()),
    }

    // Saved on the way out of every change, not on shutdown: systemd stops the
    // daemon with a signal, and anything written only at exit is the thing that
    // never runs.
    if persists && error.is_none() {
        crate::state::save(&s);
    }

    json(&s, fixed, error.as_deref())
}

fn serve_one(stream: UnixStream, settings: &Mutex<Settings>, fixed: &Fixed) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut stream = stream;
    writeln!(stream, "{}", handle(line.trim(), settings, fixed))?;
    Ok(())
}

/// Start listening, on a thread of its own.
///
/// The frame loop must never wait on a client, so this owns its own thread and
/// touches the frame loop only through the mutex.
pub fn serve(settings: Arc<Mutex<Settings>>, fixed: Fixed) -> Result<PathBuf> {
    let path = socket_path();

    // A socket left behind by a killed daemon would make bind() fail forever.
    // Connecting to it is the only way to tell "stale file" from "someone is
    // already listening", and refusing to start is right in the second case.
    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            anyhow::bail!("another studio-effects daemon is already listening on {path:?}");
        }
        std::fs::remove_file(&path).with_context(|| format!("removing stale socket {path:?}"))?;
    }

    let listener = UnixListener::bind(&path).with_context(|| format!("binding {path:?}"))?;
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            // One bad client must not take the control socket down with it.
            let _ = serve_one(stream, &settings, &fixed);
        }
    });
    Ok(path)
}

impl Effect {
    fn from_str_lenient(s: &str) -> Option<Effect> {
        match s.to_ascii_lowercase().as_str() {
            "none" | "off" => Some(Effect::None),
            "blur" => Some(Effect::Blur),
            "replace" => Some(Effect::Replace),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed() -> Fixed {
        Fixed {
            device: "NPU".into(),
            models: vec!["segmentation".into(), "matting".into()],
            input: "/dev/video0".into(),
            output: "/dev/video10".into(),
            width: 1280,
            height: 720,
            preview_path: "/run/user/1000/studio-effects-preview.jpg".into(),
        }
    }

    fn settings(has_background: bool) -> Mutex<Settings> {
        Mutex::new(Settings {
            effect: Effect::Blur,
            blur: 12,
            passes: 2,
            dim: 0,
            desat: 0,
            framing: false,
            zoom: 200,
            resume: Effect::Blur,
            has_background,
            preview: false,
            model: "segmentation".into(),
        })
    }

    #[test]
    fn status_reports_the_current_effect() {
        let s = settings(false);
        assert!(handle("status", &s, &fixed()).contains(r#""effect":"blur""#));
    }

    /// Toggling off and on again must come back to a replaced background, not
    /// quietly demote it to a blur.
    #[test]
    fn toggle_restores_the_effect_it_turned_off() {
        let s = settings(true);
        handle("effect replace", &s, &fixed());
        assert!(handle("toggle", &s, &fixed()).contains(r#""effect":"none""#));
        assert!(handle("toggle", &s, &fixed()).contains(r#""effect":"replace""#));
    }

    /// Refused, not accepted-and-ignored: silently staying on blur after being
    /// told to replace is the kind of thing a widget reports as success.
    #[test]
    fn replace_without_a_background_is_refused() {
        let s = settings(false);
        let out = handle("effect replace", &s, &fixed());
        assert!(out.contains(r#""error""#), "{out}");
        assert!(out.contains(r#""effect":"blur""#), "{out}");
    }

    /// Every knob is clamped at the socket, so a widget cannot put the daemon
    /// somewhere the daemon does not accept.
    #[test]
    fn out_of_range_settings_are_refused_not_clamped_silently() {
        let s = settings(false);
        for bad in ["passes 0", "passes 9", "dim 101", "desat 500", "dim -1"] {
            let out = handle(bad, &s, &fixed());
            assert!(out.contains(r#""error""#), "{bad} should be refused: {out}");
        }
        assert!(handle("status", &s, &fixed()).contains(r#""passes":2"#));
        assert!(handle("status", &s, &fixed()).contains(r#""dim":0"#));
    }

    #[test]
    fn the_new_knobs_are_settable() {
        let s = settings(false);
        assert!(handle("passes 3", &s, &fixed()).contains(r#""passes":3"#));
        assert!(handle("dim 40", &s, &fixed()).contains(r#""dim":40"#));
        assert!(handle("desat 100", &s, &fixed()).contains(r#""desat":100"#));
    }

    /// Only installed models may be selected, and a refusal must name what is
    /// actually available rather than a fixed pair.
    #[test]
    fn only_installed_models_can_be_selected() {
        let s = settings(false);
        assert!(handle("model matting", &s, &fixed()).contains(r#""model":"matting""#));
        let out = handle("model deepfilternet", &s, &fixed());
        assert!(out.contains(r#""error""#), "{out}");
        assert!(out.contains("segmentation|matting"), "should list what exists: {out}");
        assert!(out.contains(r#""model":"matting""#), "a refusal must not change it");
    }

    #[test]
    fn status_lists_the_models_available() {
        let s = settings(false);
        assert!(handle("status", &s, &fixed()).contains(r#""models":["segmentation","matting"]"#));
    }

    #[test]
    fn zoom_is_clamped_to_a_range_the_framing_accepts() {
        let s = settings(false);
        assert!(handle("zoom 250", &s, &fixed()).contains(r#""zoom":250"#));
        for bad in ["zoom 99", "zoom 301", "zoom 0", "zoom lots"] {
            assert!(handle(bad, &s, &fixed()).contains(r#""error""#), "{bad} should be refused");
        }
        assert!(handle("status", &s, &fixed()).contains(r#""zoom":250"#), "refusals must not change it");
    }

    #[test]
    fn framing_toggles_and_a_bare_word_turns_it_off() {
        let s = settings(false);
        assert!(handle("status", &s, &fixed()).contains(r#""framing":false"#));
        assert!(handle("framing on", &s, &fixed()).contains(r#""framing":true"#));
        assert!(handle("framing", &s, &fixed()).contains(r#""framing":false"#));
    }

    #[test]
    fn nonsense_is_reported_and_changes_nothing() {
        let s = settings(false);
        let out = handle("effect banana", &s, &fixed());
        assert!(out.contains(r#""error""#), "{out}");
        assert!(out.contains(r#""effect":"blur""#), "{out}");
        assert!(handle("blur 9999", &s, &fixed()).contains(r#""blur":12"#));
    }

    /// The widget turns preview on when its panel opens and off when it
    /// closes, so both directions have to work and neither may disturb the
    /// effect.
    #[test]
    fn preview_toggles_without_touching_the_effect() {
        let s = settings(false);
        assert!(handle("status", &s, &fixed()).contains(r#""preview":false"#));
        let out = handle("preview on", &s, &fixed());
        assert!(out.contains(r#""preview":true"#), "{out}");
        assert!(out.contains(r#""effect":"blur""#), "{out}");
        assert!(handle("preview off", &s, &fixed()).contains(r#""preview":false"#));
    }

    /// A panel that closes without being seen to must still stop the encoder,
    /// so a bare `preview` means off rather than an error.
    #[test]
    fn bare_preview_means_off() {
        let s = settings(false);
        handle("preview on", &s, &fixed());
        let out = handle("preview", &s, &fixed());
        assert!(out.contains(r#""preview":false"#), "{out}");
        assert!(!out.contains(r#""error""#), "{out}");
    }

    #[test]
    fn the_reply_says_where_preview_frames_appear() {
        let s = settings(false);
        assert!(handle("status", &s, &fixed()).contains("studio-effects-preview.jpg"));
    }

    #[test]
    fn an_empty_line_is_a_status_request() {
        let s = settings(false);
        assert!(handle("", &s, &fixed()).contains(r#""effect":"blur""#));
    }
}
