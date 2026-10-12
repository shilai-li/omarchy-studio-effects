#!/usr/bin/env bash

# Checks that the arguments the systemd unit builds are ones the daemon accepts.
#
#   bash test/unit-args-test.sh
#
# This exists because of a bug that reached the service: --framing was declared
# as a bare flag while the unit passed --framing=${FRAMING}, since a unit has no
# way to omit an argument. The daemon exited 2/INVALIDARGUMENT and systemd
# restart-looped it, and none of it showed up in testing because every manual
# run typed the arguments by hand rather than taking them from the unit.
#
# Nothing here needs a camera, an NPU or an install: --list-devices makes the
# daemon parse its arguments and exit.

set -uo pipefail
cd "$(dirname "$0")/.."
exec /usr/bin/python3 test/unit-args-test.py
