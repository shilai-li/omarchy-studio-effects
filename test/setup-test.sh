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
plan=$(bash "$tmp/clone/packaging/setup.sh" --build --dry-run 2>&1)
check "a real checkout gets a plan"           test $? -eq 0
check "the plan builds in the cache, not in place" \
      bash -c '[[ "$1" == *"/omarchy-studio-effects-build/checkout"* && "$1" != *"BUILDDIR"* ]]' _ "$plan"
check "the plan reuses downloaded sources"    bash -c '[[ "$1" == *"SRCDEST="* ]]' _ "$plan"
check "the plan rebuilds rather than reusing an old package" \
      bash -c '[[ "$1" == *"makepkg -sfi"* ]]' _ "$plan"

# Files with no .git: nothing to `git archive`.
mkdir "$tmp/plain" && copy_to "$tmp/plain"
check "a source build with no .git is refused" bash -c '! bash "$1" --build --dry-run' _ "$tmp/plain/packaging/setup.sh"
msg=$(bash "$tmp/plain/packaging/setup.sh" --build --dry-run 2>&1)
check "and says why"                          bash -c '[[ "$1" == *"not a git checkout"* ]]' _ "$msg"

# A folder that sits inside somebody's dotfiles repository would build HEAD of
# that repository, which is a different project.
mkdir -p "$tmp/dotfiles/plug" && git -C "$tmp/dotfiles" init -q && copy_to "$tmp/dotfiles/plug"
check "a folder inside another repo is refused" \
      bash -c '! bash "$1" --build --dry-run' _ "$tmp/dotfiles/plug/packaging/setup.sh"

menu=$(bash packaging/setup.sh --dry-run 2>&1)
check "the terminal offers release and source" \
      bash -c '[[ "$1" == *"1) Use prebuilt"* && "$1" == *"2) Build from source"* ]]' _ "$menu"
check "conflicting methods are refused" bash -c '! bash packaging/setup.sh --release --build --dry-run'

# Exercise the release branch with local command doubles: no network, no sudo.
mkdir "$tmp/bin"
cat > "$tmp/bin/pacman" <<'SH'
#!/usr/bin/env bash
if [ "$1" = -Q ]; then
    [ "${TEST_OPENVINO:-2026.3.1}" != absent ] || exit 1
    echo "openvino ${TEST_OPENVINO:-2026.3.1}-1"
elif [ "$1" = -Si ]; then
    echo "Version : ${TEST_REPO_OPENVINO:-2026.3.1}-1"
else
    exit 99
fi
SH
cat > "$tmp/bin/uname" <<'SH'
#!/usr/bin/env bash
echo "${TEST_ARCH:-x86_64}"
SH
cat > "$tmp/bin/curl" <<'SH'
#!/usr/bin/env bash
while [ "$#" -gt 0 ]; do
    if [ "$1" = -o ]; then printf 'tampered package\n' > "$2"; exit 0; fi
    shift
done
exit 1
SH
cat > "$tmp/bin/sudo" <<'SH'
#!/usr/bin/env bash
touch "$TEST_SUDO_MARKER"
exit 99
SH
chmod +x "$tmp/bin/"*
release=$(PATH="$tmp/bin:$PATH" bash "$tmp/plain/packaging/setup.sh" --release --dry-run 2>&1)
check "a release needs no git checkout or compiler" test $? -eq 0
check "the release plan downloads and verifies before pacman" \
      bash -c '[[ "$1" == *"Would download https://github.com/"* && "$1" == *"Would verify SHA-256:"* && "$1" == *"sudo pacman -U"* && "$1" != *"makepkg"* ]]' _ "$release"
check "an incompatible OpenVINO refuses the release" \
      bash -c '! PATH="$1:$PATH" TEST_OPENVINO=2026.3.0 bash packaging/setup.sh --release --dry-run' _ "$tmp/bin"
check "an incompatible architecture refuses the release" \
      bash -c '! PATH="$1:$PATH" TEST_ARCH=aarch64 bash packaging/setup.sh --release --dry-run' _ "$tmp/bin"
check "an absent OpenVINO uses the repository version" \
      env "PATH=$tmp/bin:$PATH" TEST_OPENVINO=absent bash packaging/setup.sh --release --dry-run
check "an incompatible repository version refuses the release" \
      bash -c '! PATH="$1:$PATH" TEST_OPENVINO=absent TEST_REPO_OPENVINO=2026.3.2 bash packaging/setup.sh --release --dry-run' _ "$tmp/bin"
printf 'y\n' | PATH="$tmp/bin:$PATH" TEST_SUDO_MARKER="$tmp/sudo-called" bash packaging/setup.sh --release > "$tmp/release.log" 2>&1
check "a tampered download fails" test $? -ne 0
check "checksum failure never invokes sudo" test ! -e "$tmp/sudo-called"
check "checksum failure explains that nothing was installed" \
      rg -q 'failed its checksum. Nothing was installed' "$tmp/release.log"

# Omarchy refuses a plugin folder that contains a symlink, and makepkg leaves
# `src` and `pkg` symlinks beside its PKGBUILD. Running the script must not
# create any in the checkout it was run from.
check "the checkout has no symlinks after a run" bash -c '! find "$1" -path "*/.git" -prune -o -type l -print | grep -q .' _ "$tmp/clone"

[ "$failed" -eq 0 ] && echo "setup script ok" || { echo "$failed failed"; exit 1; }
