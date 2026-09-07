"""Run the daemon with exactly the arguments the systemd unit would give it."""

import shlex
import subprocess
import sys
from pathlib import Path

UNIT = Path("packaging/studio-effects.service")
DAEMON = Path("daemon/target/release/studio-effects-daemon")


def unit_argv() -> list[str]:
    """The unit's ExecStart, with its own Environment defaults substituted.

    shlex rather than a hand-rolled split, because the unit quotes "Studio
    Camera" as one argument and losing that is a bug in the test rather than in
    what it is testing.
    """
    text = UNIT.read_text()
    env = dict(
        line[len("Environment=") :].split("=", 1)
        for line in text.splitlines()
        if line.startswith("Environment=")
    )

    lines, collecting = [], False
    for line in text.splitlines():
        if line.startswith("ExecStart="):
            collecting = True
            line = line[len("ExecStart=") :]
        if not collecting:
            continue
        lines.append(line.rstrip("\\").strip())
        if not line.rstrip().endswith("\\"):
            break

    argv = shlex.split(" ".join(lines))[1:]  # drop the binary
    return substitute(argv, env)


def substitute(argv: list[str], env: dict[str, str]) -> list[str]:
    out = []
    for arg in argv:
        for key, value in env.items():
            arg = arg.replace("${" + key + "}", value)
        # Paths the unit points at inside an install, which a checkout lacks.
        arg = arg.replace(
            "/usr/share/studio-effects/models/selfie_segmentation.xml",
            "models/selfie_segmentation.xml",
        )
        arg = arg.replace("%C", "/tmp")
        out.append(arg)
    return out


def run(args: list[str]) -> tuple[bool, str]:
    result = subprocess.run(
        [str(DAEMON), *args, "--list-devices"], capture_output=True, text=True
    )
    return result.returncode == 0, (result.stderr or result.stdout).strip()


def main() -> None:
    if not DAEMON.exists():
        sys.exit("build the daemon first: cargo build --release --manifest-path daemon/Cargo.toml")

    failures = []
    argv = unit_argv()
    ok, message = run(argv)
    if ok:
        print("ok   the unit's own arguments parse")
    else:
        failures.append(f"the daemon rejected the unit's arguments:\n       "
                        f"{message.splitlines()[0]}\n     built from: {' '.join(argv)}")

    # Every value a config file can put into a switch, including an unset one:
    # the unit passes the argument regardless, so empty has to be legal.
    for value in ("on", "off", "true", "false", ""):
        if not run([f"--framing={value}"])[0]:
            failures.append(f"--framing={value!r} was rejected; a config file can produce it")
    if not failures:
        print("ok   every switch value a config file can produce is accepted")

    # And a typo must be refused rather than silently meaning off.
    if run(["--framing=banana"])[0]:
        failures.append("--framing=banana was accepted; a typo would silently mean off")

    if failures:
        print(f"{len(failures)} failed\n")
        for f in failures:
            print(f"  {f}")
        sys.exit(1)
    print("unit arguments ok")


if __name__ == "__main__":
    main()
