#!/usr/bin/env bash
# The venue's SCOPING matrix (docs/evidence/rung-09-brackets.md): every row
# A-B-B-A with A = B = the given binary (the noise band first), each leg 60 s
# sustained + 10 s warm-up, on the substrate the measurement law assigns —
# nvmet-tcp for anything bandwidth-shaped, nvmet-loop for anything
# latency-shaped, the file substrate as the /build baseline. Sequential: two
# rows at once would measure each other.
#
#   scoping-run.sh <binary> [out-dir]
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN=$(readlink -f "${1:?binary}")
OUT=${2:-$(cd "$HERE/../.." && pwd)/bench/brackets}
LOG=/build/brk-scoping-run.log
# The venue's sizing: 4 × 512 MiB files (2 GiB datasets) and a 7 GiB budget keep
# the daemon Green beside its ≈ 2.7 GiB unreclaimable floor on a 16 GiB VM shared
# with two other agents; the box uses BRK_FILE_SIZE=4g and the default budget.
export BRK_FILE_SIZE=${BRK_FILE_SIZE:-512m}
BUDGET=${BRK_BUDGET_MB:-7168}
run() {   # substrate row [extra bracket options…]
  local sub=$1 row=$2; shift 2
  echo "$(date -u +%H:%M:%S) >>> $row on $sub $*" | tee -a "$LOG"
  timeout 3600 "$HERE/bracket.sh" --scoping --substrate "$sub" --out "$OUT" --budget-mb "$BUDGET" "$@" "$row" "$BIN" "$BIN" >> "$LOG" 2>&1 \
    || echo "$(date -u +%H:%M:%S) !!! $row on $sub failed (rc $?)" | tee -a "$LOG"
}
# Fabric-sensitive (bandwidth): nvmet-tcp on 127.0.0.1.
run nvmet-tcp seq-write-1m-direct
run nvmet-tcp seq-read-1m-direct
run nvmet-tcp seq-write-1m-buffered
run nvmet-tcp seq-read-1m-buffered
run nvmet-tcp shim-seq-write-1m
run nvmet-tcp shim-seq-read-1m
# Latency-shaped: nvmet-loop.
run nvmet-loop rand-4k-read
run nvmet-loop rand-4k-read --mount-opts direct_device_true
run nvmet-loop rand-4k-write
run nvmet-loop shim-rand-4k-read
run nvmet-loop shim-rand-4k-write
run nvmet-loop fsync-4k
run nvmet-loop creates-mdtest
run nvmet-loop rename-unlink
run nvmet-loop tar-x
run nvmet-loop creates-mdtest --lever posture
# The /build baseline (file-backed, shared disk).
run file seq-write-1m-direct
run file rand-4k-write
echo "$(date -u +%H:%M:%S) >>> io500 (scoping ini) on nvmet-tcp" | tee -a "$LOG"
timeout 7200 "$HERE/io500.sh" --scoping --substrate nvmet-tcp --out "$OUT" --budget-mb "$BUDGET" "$BIN" >> "$LOG" 2>&1 || echo "io500 failed (rc $?)" | tee -a "$LOG"
echo "$(date -u +%H:%M:%S) done" | tee -a "$LOG"
