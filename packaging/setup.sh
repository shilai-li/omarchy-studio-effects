#!/usr/bin/env bash
#
# Installs the Studio Effects daemon from a release or from source.
#
#   bash packaging/setup.sh             choose release or source in the terminal
#   bash packaging/setup.sh --release   download, verify, and install the release
#   bash packaging/setup.sh --build     build, then install (asks for sudo)
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
method=""
for arg in "$@"; do
    case "$arg" in
        --dry-run) dry=1 ;;
        --release|--build)
            [ -z "$method" ] || { echo "choose only one installation method" >&2; exit 2; }
            method=${arg#--} ;;
        *) echo "usage: ${0##*/} [--release|--build] [--dry-run]" >&2; exit 2 ;;
    esac
done

say()  { printf '%s\n' "$*"; }
die()  { printf '\nCannot set up: %s\n' "$*" >&2; exit 1; }

# ---- What has to be true before it is worth starting.

[ "$(id -u)" -ne 0 ] || die "run this as yourself, not root. Installation asks for sudo in this terminal."

if [ -z "$method" ]; then
    say "Choose how to install Studio Effects:"
    say "  1) Install release — no compilation; x86-64, OpenVINO 2026.3.1"
    say "  2) Build from source — uses your installed libraries; takes a few minutes"
    if [ "$dry" -eq 1 ]; then
        say "Use --release --dry-run or --build --dry-run to inspect either option."
        exit 0
    fi
    read -r -p "Choice [1/2, or q to cancel]: " answer || exit 130
    case "$answer" in
        1) method=release ;;
        2) method=build ;;
        [Qq]|"") say "Nothing done."; exit 130 ;;
        *) die "choose 1 or 2." ;;
    esac
fi

tools=(pacman sudo)
if [ "$method" = build ]; then tools+=(makepkg git); else tools+=(curl sha256sum uname mktemp); fi
for tool in "${tools[@]}"; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is not installed. This needs an Arch-based system."
done

if [ "$method" = release ]; then
    [ "$(uname -m)" = x86_64 ] || die "the release is for x86-64. Choose Build from source instead."
    # Refuse an incompatible installed version instead of asking pacman to
    # downgrade it. If it is absent, check the locally synced repository version.
    if version=$(pacman -Q openvino 2>/dev/null); then
        version=${version#* }
    else
        version=$(LC_ALL=C pacman -Si openvino 2>/dev/null | awk '$1 == "Version" {print $3; exit}') \
            || die "cannot determine OpenVINO's repository version. Choose Build from source."
    fi
    version=${version%-*}
    version=${version#*:}
    [ "$version" = 2026.3.1 ] || die "this release needs OpenVINO 2026.3.1; yours is ${version:-unknown}. Choose Build from source instead."

    package=omarchy-studio-effects-0.1.0-2-x86_64.pkg.tar.zst
    url="https://github.com/shilai-li/omarchy-studio-effects/releases/download/v0.1.0/$package"
    # Pinned to the published SHA256SUMS; downloaded bytes cannot supply their
    # own expected checksum. Update these together when publishing a new binary.
    checksum=4c3476db8ebc80044a2fa0a67f565bcf248cea3424c93b0dc6ed881e013138e9
    say "Install the v0.1.0 release (about 41 MB), verify its checksum, then run pacman."
    say "Installation asks for sudo and creates Studio Camera; camera and microphone stay off."
    if [ "$dry" -eq 1 ]; then
        say "Would download $url"
        say "Would verify SHA-256: $checksum"
        say "Would run: sudo pacman -U <download folder>/$package"
        exit 0
    fi
else

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
Build Studio Effects from this checkout and install it as a package.

  builds    the daemon and converts its two models   (a few minutes; downloads
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
fi

read -r -p "Continue? [Y/n] " answer || exit 130
case "${answer:-y}" in
    [Yy]*) ;;
    *) say "Nothing done."; exit 130 ;;   # 130: declined, which is not a failure to report
esac

if [ "$method" = release ]; then
    download=$(mktemp -d)
    trap 'rm -rf "$download"' EXIT
    curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fL --retry 2 \
        "$url" -o "$download/$package" \
        || die "the release download failed. Retry, or choose Build from source."
    printf '%s  %s\n' "$checksum" "$download/$package" | sha256sum -c - \
        || die "the downloaded package failed its checksum. Nothing was installed."
    sudo pacman -U "$download/$package"
else
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
fi

# ---- Check it took, rather than assuming.

echo
if [ -x /usr/bin/studio-effects-daemon ]; then
    say "Installed: $(/usr/bin/studio-effects-daemon --version)"
else
    die "installation finished but /usr/bin/studio-effects-daemon is not there."
fi

if grep -qFx "Studio Camera" /sys/class/video4linux/*/name 2>/dev/null; then
    say "Studio Camera exists. Pick it in your call app, and turn effects on from the bar."
else
    say "Studio Camera was not created. Make it with:"
    say "    sudo systemctl enable --now studio-effects-loopback"
fi
