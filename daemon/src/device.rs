//! Finding a v4l2 device by what it is called rather than what number it got.

use anyhow::{Context, Result};
use std::path::Path;

const V4L_CLASS: &str = "/sys/class/video4linux";

/// Resolve a device spec to a `/dev/videoN` path.
///
/// A spec is either a path, used as given, or a card label, looked up.
///
/// Numbers are not stable and cannot be configured to be. The loopback this
/// daemon writes to is created at boot by a service, so it takes whatever number
/// is free at the time: it was /dev/video51 one boot and /dev/video10 the next,
/// on the same machine with the same setup. Omarchy's own camera relay resolves
/// its sink the same way, by grepping the card labels under
/// /sys/devices/virtual/video4linux for the one it was configured with.
pub fn resolve(spec: &str) -> Result<String> {
    if spec.starts_with('/') {
        anyhow::ensure!(Path::new(spec).exists(), "no such device: {spec}");
        return Ok(spec.to_string());
    }

    let mut matches = Vec::new();
    let entries = std::fs::read_dir(V4L_CLASS)
        .with_context(|| format!("listing {V4L_CLASS}; is this a v4l2 system?"))?;

    for entry in entries.flatten() {
        let name_file = entry.path().join("name");
        let Ok(label) = std::fs::read_to_string(&name_file) else {
            continue;
        };
        if label.trim() == spec {
            matches.push(format!("/dev/{}", entry.file_name().to_string_lossy()));
        }
    }

    matches.sort();
    match matches.len() {
        1 => Ok(matches.remove(0)),
        // A capture device often exposes a second metadata node under the same
        // label. The lower number is the one that carries video.
        n if n > 1 => Ok(matches.remove(0)),
        _ => {
            let known = list().unwrap_or_default();
            anyhow::bail!(
                "no video device is called {spec:?}. Present:\n{}",
                if known.is_empty() {
                    "  (none)".to_string()
                } else {
                    known
                        .iter()
                        .map(|(dev, label)| format!("  {dev:<16} {label:?}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            )
        }
    }
}

/// Every v4l2 device and its card label, for error messages and `--list`.
pub fn list() -> Result<Vec<(String, String)>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(V4L_CLASS)?.flatten() {
        if let Ok(label) = std::fs::read_to_string(entry.path().join("name")) {
            found.push((
                format!("/dev/{}", entry.file_name().to_string_lossy()),
                label.trim().to_string(),
            ));
        }
    }
    found.sort();
    Ok(found)
}
