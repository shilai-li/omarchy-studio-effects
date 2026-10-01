#!/usr/bin/env bash

# Checks packaging/studio-effects-loopback-prepare against a made-up /sys and
# stand-in commands, so it needs no kernel module and no root.
#
# The cases that matter are the ones where deleting a device would be wrong:
# the module was loaded by somebody else, the device is open, or the device is
# not the module's own default. Each of those has to leave everything alone.
#
#   bash test/loopback-prepare-test.sh

set -uo pipefail
cd "$(dirname "$0")/.."
script="$PWD/packaging/studio-effects-loopback-prepare"

failed=0
check() {  # name, then a command that must succeed
    local name=$1; shift
    if "$@" >/dev/null 2>&1; then echo "ok   $name"; else echo "FAIL $name"; failed=$((failed + 1)); fi
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# One fresh world per case: a fake /sys with the named devices, a fake
# /proc/modules, and stand-ins that record what they were asked to do.
world() {  # dir, module-loaded(yes|no), then "videoN=Card name" pairs, then --open videoN ...
    local dir=$1 loaded=$2; shift 2
    rm -rf "$dir"; mkdir -p "$dir/sys" "$dir/bin"
    : > "$dir/calls"; : > "$dir/open"
    if [ "$loaded" = yes ]; then echo "v4l2loopback 61440 0 - Live 0x0000" > "$dir/modules"; else : > "$dir/modules"; fi
    while [ $# -gt 0 ]; do
        case "$1" in
            --open) echo "/dev/$2" >> "$dir/open"; shift 2 ;;
            *=*) mkdir -p "$dir/sys/${1%%=*}"; printf '%s\n' "${1#*=}" > "$dir/sys/${1%%=*}/name"; shift ;;
        esac
    done
    printf '#!/bin/bash\necho "modprobe $*" >> "%s/calls"\n' "$dir" > "$dir/bin/modprobe"
    printf '#!/bin/bash\necho "ctl $*" >> "%s/calls"\n' "$dir" > "$dir/bin/v4l2loopback-ctl"
    printf '#!/bin/bash\ngrep -qxF "$1" "%s/open"\n' "$dir" > "$dir/bin/fuser"
    chmod +x "$dir/bin/"*
}

run() {  # dir, then the script's arguments
    local dir=$1; shift
    PATH="$dir/bin:$PATH" LOOPBACK_SYS="$dir/sys" LOOPBACK_MODULES="$dir/modules" bash "$script" "$@"
}

calls() { cat "$1/calls" 2>/dev/null | tr '\n' ';'; }

DUMMY="Dummy video device (0x0000)"

# We load the module, and it brings its default device with it: remove that.
w="$tmp/a"; world "$w" no "video2=$DUMMY"
run "$w"
check "loads the module when nobody has"          bash -c 'grep -q "^modprobe v4l2loopback" "$1/calls"' _ "$w"
check "and removes the default device it made"    bash -c 'grep -q "^ctl delete /dev/video2" "$1/calls"' _ "$w"

# Somebody else loaded it -- Omarchy's camera setup, say. Their devices are theirs.
w="$tmp/b"; world "$w" yes "video2=$DUMMY" "video50=Hardware ISP Camera"
run "$w"
check "an already-loaded module is left alone"    bash -c '[ ! -s "$1/calls" ]' _ "$w"

# Our own device and the vendor's are never touched, whatever the module state.
w="$tmp/c"; world "$w" no "video2=$DUMMY" "video3=Studio Camera" "video50=Hardware ISP Camera"
run "$w"
check "only the default device is removed"        bash -c '[ "$(grep -c "^ctl delete" "$1/calls")" = 1 ]' _ "$w"
check "Studio Camera is not removed"              bash -c '! grep -q "video3" "$1/calls"' _ "$w"
check "the vendor camera is not removed"          bash -c '! grep -q "video50" "$1/calls"' _ "$w"

# An open device is in use. Not ours to take away.
w="$tmp/d"; world "$w" no "video2=$DUMMY" --open video2
run "$w"
check "a default device that is open is kept"     bash -c '! grep -q "^ctl delete" "$1/calls"' _ "$w"

# No default device -- already gone, or a module configured not to make one.
w="$tmp/e"; world "$w" no "video3=Studio Camera"
run "$w"
check "nothing to remove is not an error"         bash -c 'grep -q "^modprobe" "$1/calls" && ! grep -q "^ctl" "$1/calls"' _ "$w"

# The upgrade path: the module is loaded, so the normal route does nothing, and
# this is how an existing install sheds the device it already has.
w="$tmp/f"; world "$w" yes "video2=$DUMMY" "video3=Studio Camera"
run "$w" --remove-stray
check "--remove-stray works on a loaded module"   bash -c 'grep -q "^ctl delete /dev/video2" "$1/calls" && ! grep -q "^modprobe" "$1/calls"' _ "$w"
w="$tmp/g"; world "$w" yes "video2=$DUMMY" --open video2
run "$w" --remove-stray
check "--remove-stray still keeps an open one"    bash -c '! grep -q "^ctl delete" "$1/calls"' _ "$w"

# A failed load is a failed start: no pretending the device will be there.
w="$tmp/h"; world "$w" no "video2=$DUMMY"; printf '#!/bin/bash\nexit 1\n' > "$w/bin/modprobe"
check "a module that will not load fails the unit" bash -c '! PATH="$1/bin:$PATH" LOOPBACK_SYS="$1/sys" LOOPBACK_MODULES="$1/modules" bash "$2"' _ "$w" "$script"

check "an unknown argument is refused"            bash -c '! bash "$1" --bogus' _ "$script"
check "the script parses"                         bash -n "$script"

[ "$failed" -eq 0 ] && echo "loopback-prepare ok" || { echo "$failed failed"; exit 1; }
