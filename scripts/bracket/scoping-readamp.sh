#!/usr/bin/env bash
# The read-amplification scoping matrix (decision 0003 §K): every bounded read
# shape once through the harness, SCOPING legs, one substrate. Usage:
#   scoping-readamp.sh <out-dir> <A-binary> <B-binary> [legs] [duration] [substrate] [rows…]
# Records land under <out-dir>/<venue-id>/; the summary table is printed last.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT=$1; A=$2; B=$3; LEGS=${4:-1}; DUR=${5:-30}; SUB=${6:-nvmet-loop}
shift 6 2>/dev/null || shift $#
ROWS=("$@")
if [ ${#ROWS[@]} -eq 0 ]; then
  ROWS=(rand-4k-read rand-512b-read rand-4k-read-unaligned rand-256k-read rand-1m-read seq-read-1m-direct seq-read-early-stop read-after-small-write shim-rand-4k-read
        "rand-4k-read --format-opts=--compress zstd" "rand-4k-read --format-opts=--encrypt aes-256-gcm --encrypt-key @KEY" "rand-1m-read --format-opts=--compress zstd")
fi
mkdir -p "$OUT"
for spec in "${ROWS[@]}"; do
  row=${spec%% --format-opts=*}
  fopts=""
  case "$spec" in *" --format-opts="*) fopts=${spec#* --format-opts=} ;; esac
  args=(--scoping --legs "$LEGS" --duration "$DUR" --warmup 10 --substrate "$SUB" --numjobs 4 --out "$OUT" --scratch "${SQZ_BUILD_ROOT:-/build}/tmp/readamp-brk-$$")
  [ -n "$fopts" ] && args+=(--format-opts "$fopts")
  echo "=== $(date -u +%H:%M:%S) $row ${fopts:+(format $fopts)} ==="
  timeout 3000 "$HERE/bracket.sh" "${args[@]}" "$row" "$A" "$B" 2>&1 | grep -E "bracket\[|^\| [0-9]|read device|INVALID|verdict" || true
  sync -f ${SQZ_BUILD_ROOT:-/build}; fstrim -v ${SQZ_BUILD_ROOT:-/build} > /dev/null 2>&1 || true
done
python3 "$HERE/brk.py" summary "$OUT"/*/
