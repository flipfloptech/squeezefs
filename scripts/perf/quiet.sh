#!/usr/bin/env bash
# quiet.sh <session> [rep] — one baseline.sh session holding the WHOLE local venue: the gate lock and
# every build slot (so no build, test or gate shares the box with the measurement), with the
# provisioning-loaded brd unloaded first (ramnvme.sh owns brd for the session and rmmods it at down).
# Run as root in the tree whose binary is measured (a worktree measures its own branch; the main
# tree at dev is the A leg):   sudo MX_RUNTIME=20 scripts/perf/quiet.sh ram-sqz A-r1
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROOT=${SQZ_BUILD_ROOT:-/build}
[ "$(id -u)" = 0 ] || { echo "quiet: root is required" >&2; exit 1; }
SLOTS=$(ls "$ROOT"/.build-slot.* 2>/dev/null | wc -l)
exec 200>"$ROOT/.gate.lock"; flock 200
for ((i = 0; i < SLOTS; i++)); do eval "exec $((201 + i))>\"$ROOT/.build-slot.$i\""; flock $((201 + i)); done
if [ -d /sys/module/brd ] && [ ! -e /run/sqz-ramnvme/devs ]; then
  for d in /dev/ram*; do findmnt -rn -S "$d" >/dev/null 2>&1 && { echo "quiet: $d is mounted; refusing to unload brd" >&2; exit 1; }; done
  rmmod brd
fi
exec bash "$HERE/baseline.sh" "$@"
