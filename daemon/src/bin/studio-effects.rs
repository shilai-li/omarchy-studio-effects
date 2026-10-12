//! Command-line control for a running studio-effects daemon.
//!
//! Deliberately a separate short-lived process rather than a library the bar
//! widget links: an Omarchy plugin runs inside the shell process, and spawning
//! a command whose stdout is one line of JSON is the shape those plugins
//! already use.

use anyhow::{Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

const USAGE: &str = "\
usage: studio-effects [command]

  status                       what the daemon is doing now (JSON)
  effect none|blur|replace     choose an effect
  toggle                       turn effects off, or back on to the last one
  blur <0-200>                 background blur radius
  passes <1-3>                 blur repeats; 1 is boxy, 2 looks Gaussian
  dim <0-100>                  darken the background
  desat <0-100>                drain colour from the background
  framing on|off               track the subject and keep them centred
  zoom <100-300>               how far framing may crop in; 200 is 2x
  model <name>                 which model segments; `status` lists the
                               installed ones. Swaps live, in about 15 ms
  preview on|off               publish preview frames for the bar widget

With no command, prints status. Every command answers with the daemon's full
state, so a caller never has to ask twice.";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return Ok(());
    }

    let path = studio_effects_daemon::control::socket_path();
    let stream = UnixStream::connect(&path).with_context(|| {
        format!(
            "no daemon is listening on {path:?}. Start it with:\n    \
             systemctl --user start studio-effects"
        )
    })?;

    let mut writer = stream.try_clone()?;
    writeln!(writer, "{}", args.join(" "))?;
    writer.flush()?;

    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    print!("{reply}");

    // A refusal has to be an exit code too, or a caller that only checks the
    // status silently believes a rejected change took effect.
    if reply.contains(r#""error""#) {
        std::process::exit(1);
    }
    Ok(())
}
