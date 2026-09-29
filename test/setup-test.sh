#!/usr/bin/env bash

# Checks packaging/setup.sh without building anything or asking for sudo: it has
# a --dry-run for exactly this. The three layouts matter because the script
# builds from `git archive HEAD` of its own checkout, and a wrong layout builds
# the wrong project or none at all.
#
#   bash test/setup-test.sh

set -uo pipefail
cd "$(dirname "$0")/.."
repo=$PWD

failed=0
check() {  # name, then a command that must succeed
    local name=$1; shift
    if "$@" >/dev/null 2>&1; then echo "ok   $name"; else echo "FAIL $name"; failed=$((failed + 1)); fi
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# The files as they are now, tracked or not, minus anything ignored -- so this
# tests uncommitted work too, which a `git archive HEAD` would not.
copy_to() { git ls-files --cached --others --exclude-standard -z | xargs -0 cp --parents -t "$1"; }

check "the script parses"                     bash -n packaging/setup.sh
check "an unknown argument is refused"        bash -c '! bash packaging/setup.sh --bogus'

# A real checkout: a clone of this one.
git clone --quiet "$repo" "$tmp/clone" && cp packaging/setup.sh "$tmp/clone/packaging/"
plan=$(bash "$tmp/clone/packaging/setup.sh" --dry-run 2>&1)
check "a real checkout gets a plan"           test $? -eq 0
check "the plan builds in the cache, not in place" \
      bash -c '[[ "$1" == *"/omarchy-studio-effects-build/checkout"* && "$1" != *"BUILDDIR"* ]]' _ "$plan"
check "the plan reuses downloaded sources"    bash -c '[[ "$1" == *"SRCDEST="* ]]' _ "$plan"
check "the plan rebuilds rather than reusing an old package" \
      bash -c '[[ "$1" == *"makepkg -sfi"* ]]' _ "$plan"

# Files with no .git: nothing to `git archive`.
mkdir "$tmp/plain" && copy_to "$tmp/plain"
check "a copy with no .git is refused"        bash -c '! bash "$1" --dry-run' _ "$tmp/plain/packaging/setup.sh"
msg=$(bash "$tmp/plain/packaging/setup.sh" --dry-run 2>&1)
check "and says why"                          bash -c '[[ "$1" == *"not a git checkout"* ]]' _ "$msg"

# A folder that sits inside somebody's dotfiles repository would build HEAD of
# that repository, which is a different project.
mkdir -p "$tmp/dotfiles/plug" && git -C "$tmp/dotfiles" init -q && copy_to "$tmp/dotfiles/plug"
check "a folder inside another repo is refused" \
      bash -c '! bash "$1" --dry-run' _ "$tmp/dotfiles/plug/packaging/setup.sh"

# Omarchy refuses a plugin folder that contains a symlink, and makepkg leaves
# `src` and `pkg` symlinks beside its PKGBUILD. Running the script must not
# create any in the checkout it was run from.
check "the checkout has no symlinks after a run" bash -c '! find "$1" -path "*/.git" -prune -o -type l -print | grep -q .' _ "$tmp/clone"

[ "$failed" -eq 0 ] && echo "setup script ok" || { echo "$failed failed"; exit 1; }
