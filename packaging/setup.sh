#!/usr/bin/env bash
#
# Builds and installs the Studio Effects daemon.
#
#   bash packaging/setup.sh             build, then install (asks for sudo)
#   bash packaging/setup.sh --dry-run   say what it would do, and change nothing
#
# This is what the bar widget opens in a terminal the first time it finds the
# daemon missing. `omarchy plugin add` clones files and nothing else -- it never
# builds, never runs an install hook, never asks for sudo -- so after adding the
# plugin there is no daemon, and something has to build one. That something is
# this, run by the person at the keyboard in a window they can see, which is
# also the only place a sudo prompt can be answered. The widget itself does none
# of it.
#
# The package is built from `git archive HEAD` of this checkout, so what is
# installed is what is committed here and not whatever a working tree holds.

set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(dirname "$here")

# Everything is built here, in a clone of this checkout, and nothing is built in
# the plugin's own folder. makepkg leaves `src` and `pkg` symlinks beside the
# PKGBUILD, and Omarchy refuses a plugin folder that contains a symlink -- so a
# build in place would leave the plugin unable to be validated or updated.
build="${XDG_CACHE_HOME:-$HOME/.cache}/omarchy-studio-effects-build"
tree="$build/checkout"

dry=0
case "${1:-}" in
    --dry-run) dry=1 ;;
    "") ;;
    *) echo "usage: ${0##*/} [--dry-run]" >&2; exit 2 ;;
esac

say()  { printf '%s\n' "$*"; }
die()  { printf '\nCannot set up: %s\n' "$*" >&2; exit 1; }

# ---- What has to be true before it is worth starting.

[ "$(id -u)" -ne 0 ] || die "run this as yourself, not root. makepkg refuses root, and asks for sudo itself when it needs it."

for tool in makepkg pacman git; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is not installed. This needs an Arch-based system."
done

# The PKGBUILD builds from `git archive HEAD` of the directory above it, so this
# has to be a checkout of its own -- not a copy of the files, and not a folder
# that only happens to sit inside somebody's dotfiles repository, where HEAD
# would be the wrong project.
top=$(git -C "$repo" rev-parse --show-toplevel 2>/dev/null) \
    || die "$repo is not a git checkout. Add the plugin with 'omarchy plugin add', which clones it."
[ "$top" = "$repo" ] \
    || die "$repo sits inside a different git repository ($top), so the package would be built from the wrong tree."
[ -f "$here/PKGBUILD" ] || die "no PKGBUILD in $here."

# ---- Say what is about to happen.

cat <<EOF
Studio Effects needs its daemon, which is built here and installed as a package.

  builds    the daemon and converts its two models   (a few minutes; fetches about
                                                      model sources and Rust crates)
  installs  the package system-wide                  (asks for sudo)
  creates   the "Studio Camera" device                (asks for sudo, via the package)
  does not  start the camera or the microphone filter -- those stay off until
            you turn them on from the bar

  build folder   $build
  source         $repo   (HEAD: $(git -C "$repo" rev-parse --short HEAD))
                 built from a clone of it; the plugin's own folder is left alone

EOF

# SRCDEST keeps model weights and source archives across rebuilds, since the
# clone it is built in is thrown away each time.
command=(env "SRCDEST=$build/sources" makepkg -sfi)

if [ "$dry" -eq 1 ]; then
    say "Would clone $repo to $tree, then run there, in packaging/:"
    say "  ${command[*]}"
    exit 0
fi

read -r -p "Continue? [Y/n] " answer
case "${answer:-y}" in
    [Yy]*) ;;
    *) say "Nothing done."; exit 130 ;;   # 130: declined, which is not a failure to report
esac

# A fresh clone every time: what is installed is what is committed, and a
# half-finished earlier build cannot leak into this one. The PKGBUILD reads
# `git archive HEAD` of the directory above it, so it has to be a repository.
mkdir -p "$build/sources"
rm -rf "$tree"
git clone --quiet --no-hardlinks "$repo" "$tree"

# -s installs the build dependencies (Rust, python-openvino) and -i installs the
# result, each asking for sudo. -f rebuilds even when an older package is lying
# around: without it makepkg reuses that one and installs it, which is how an
# install once "succeeded" and left the old daemon in place.
cd "$tree/packaging"
"${command[@]}"

# ---- Check it took, rather than assuming.

echo
if [ -x /usr/bin/studio-effects-daemon ]; then
    say "Installed: $(/usr/bin/studio-effects-daemon --version)"
else
    die "the build finished but /usr/bin/studio-effects-daemon is not there."
fi

if grep -qFx "Studio Camera" /sys/class/video4linux/*/name 2>/dev/null; then
    say "Studio Camera exists. Pick it in your call app, and turn effects on from the bar."
else
    say "Studio Camera was not created. Make it with:"
    say "    sudo systemctl enable --now studio-effects-loopback"
fi
