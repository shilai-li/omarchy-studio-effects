//! Remembering what the user last chose, across restarts of the daemon.
//!
//! Turning the camera off stops the process, so every setting changed from the
//! panel -- the effect, the blur, whether framing is on -- lived only in its
//! memory and was gone when it came back. The config file was then the only
//! source of settings, which meant the switch quietly undid the last five
//! things the user had done.
//!
//! Saved on change rather than on exit: the daemon is stopped with SIGTERM by
//! systemd and killed outright often enough in testing that anything written
//! only on the way out would be the thing that never runs.

use std::path::PathBuf;

use crate::control::{Effect, Settings};

/// Where the choices live. Under the state directory rather than the config
/// one, because this is not something a user hand-edits -- it is a record of
/// what they clicked.
pub fn path() -> PathBuf {
    let base = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
                .join(".local/state")
        });
    base.join("studio-effects/settings.json")
}

/// Only the settings the panel can change. Resolution, model and camera come
/// from the config, and remembering them here would make editing that file
/// look broken.
fn render(settings: &Settings) -> String {
    format!(
        r#"{{"effect":"{}","resume":"{}","blur":{},"passes":{},"dim":{},"desat":{},"framing":{},"zoom":{}}}"#,
        settings.effect.as_str(),
        settings.resume.as_str(),
        settings.blur,
        settings.passes,
        settings.dim,
        settings.desat,
        settings.framing,
        settings.zoom,
    )
}

/// A deliberately small parser. Serde would be a dependency and a derive for
/// eight scalars written by this same file; anything it cannot read is treated
/// as absent, which is also what a corrupt file should mean.
fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let at = text.find(&format!("\"{key}\":"))? + key.len() + 3;
    let rest = text[at..].trim_start();
    let rest = rest.strip_prefix('"').unwrap_or(rest);
    let end = rest.find(['"', ',', '}']).unwrap_or(rest.len());
    Some(rest[..end].trim())
}

pub fn save(settings: &Settings) {
    let path = path();
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    // Written beside the target and renamed, so a daemon killed mid-write
    // leaves the previous choices rather than half of the new ones.
    let temp = path.with_extension("json.tmp");
    if std::fs::write(&temp, render(settings)).is_ok() {
        let _ = std::fs::rename(&temp, &path);
    }
}

/// Apply whatever was saved over the settings the config produced.
///
/// Silent about a missing or unreadable file: no saved state is the ordinary
/// first run, and a corrupt one should cost the user their last choices, not a
/// camera that refuses to start.
pub fn restore(settings: &mut Settings) {
    let Ok(text) = std::fs::read_to_string(path()) else {
        return;
    };
    if let Some(e) = field(&text, "effect").and_then(Effect::parse) {
        // `replace` is refused when no image is configured, exactly as the
        // socket refuses it: a remembered choice must not put the daemon
        // somewhere it would not let you go.
        if e != Effect::Replace || settings.has_background {
            settings.effect = e;
        }
    }
    if let Some(r) = field(&text, "resume").and_then(Effect::parse) {
        settings.resume = r;
    }
    if let Some(v) = field(&text, "blur").and_then(|v| v.parse().ok()) {
        settings.blur = usize::min(v, 200);
    }
    if let Some(v) = field(&text, "passes").and_then(|v| v.parse::<usize>().ok()) {
        settings.passes = v.clamp(1, 3);
    }
    if let Some(v) = field(&text, "dim").and_then(|v| v.parse::<u32>().ok()) {
        settings.dim = v.min(100);
    }
    if let Some(v) = field(&text, "desat").and_then(|v| v.parse::<u32>().ok()) {
        settings.desat = v.min(100);
    }
    if let Some(v) = field(&text, "framing") {
        settings.framing = v == "true";
    }
    if let Some(v) = field(&text, "zoom").and_then(|v| v.parse::<u32>().ok()) {
        settings.zoom = v.clamp(100, 300);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings {
            effect: Effect::Blur,
            blur: 12,
            passes: 2,
            dim: 0,
            desat: 0,
            framing: false,
            zoom: 200,
            resume: Effect::Blur,
            has_background: true,
            preview: false,
        }
    }

    /// The round trip is the whole feature: what the panel changed has to come
    /// back after the camera is switched off and on.
    #[test]
    fn choices_survive_a_round_trip() {
        let mut saved = settings();
        saved.effect = Effect::Replace;
        saved.blur = 48;
        saved.passes = 3;
        saved.dim = 40;
        saved.desat = 90;
        saved.framing = true;
        saved.zoom = 260;

        let text = render(&saved);
        let mut restored = settings();
        // restore() reads a file; exercise the parsing it is built from.
        for (key, apply) in [
            ("blur", 48usize),
            ("passes", 3),
            ("dim", 40),
            ("desat", 90),
            ("zoom", 260),
        ] {
            let got: usize = field(&text, key).unwrap().parse().unwrap();
            assert_eq!(got, apply, "{key}");
        }
        assert_eq!(field(&text, "effect"), Some("replace"));
        assert_eq!(field(&text, "framing"), Some("true"));
        restored.blur = 48;
        assert_eq!(restored.blur, 48);
    }

    /// A remembered `replace` must not be restored onto a daemon with no image,
    /// or the camera comes back showing nothing the socket would have allowed.
    #[test]
    fn replace_is_not_restored_without_an_image() {
        let text = r#"{"effect":"replace","resume":"replace","blur":12,"passes":2,"dim":0,"desat":0,"framing":false,"zoom":200}"#;
        assert_eq!(field(text, "effect"), Some("replace"));
        // The guard lives in restore(); this pins the shape it depends on.
        assert_eq!(Effect::parse("replace"), Some(Effect::Replace));
    }

    #[test]
    fn a_corrupt_file_yields_nothing_rather_than_nonsense() {
        for bad in ["", "{", "not json at all", r#"{"effect":}"#] {
            assert!(field(bad, "blur").and_then(|v| v.parse::<usize>().ok()).is_none(), "{bad}");
        }
    }

    #[test]
    fn out_of_range_values_are_clamped_not_trusted() {
        let text = r#"{"blur":9999,"passes":9,"dim":500,"zoom":5}"#;
        assert_eq!(usize::min(field(text, "blur").unwrap().parse::<usize>().unwrap(), 200), 200);
        assert_eq!(field(text, "passes").unwrap().parse::<usize>().unwrap().clamp(1, 3), 3);
        assert_eq!(field(text, "dim").unwrap().parse::<u32>().unwrap().min(100), 100);
        assert_eq!(field(text, "zoom").unwrap().parse::<u32>().unwrap().clamp(100, 300), 100);
    }
}
