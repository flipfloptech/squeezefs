#!/usr/bin/env bash
# The box re-run's ATTRIBUTION leg for gate 1's mdstorm DELTA rows: the flat
# mdstorm on the file-backed /dev/shm substrate (the same shape as
# tests/run_mdstorm.sh's leg at 100 % scale — mkdir 20k → create 100k →
# rename 100k → unlink 100k → manydirs → rmdir), with the daemon under
# `perf record -g` ONE PHASE AT A TIME, so a phase that reads DELTA in the
# A-B-B-A (rename / unlink on PR 13c's binary) is attributed against a phase
# that reads PAR (mkdir / create) on the same daemon in the same leg. The
# previous rung's leg (`/scratch/tmp/sym-box/box-perf-leg.sh`) recorded the
# whole storm in one 45 s window and could not separate the phases.
# Attribution only — no number from this leg is a verdict.
#
#   sudo bash 2026-09-22-sym-box-perf-phases.sh <ARM-LABEL> <BIN> [OUT]
#   e.g. sudo bash ... A /scratch/tmp/sym-box/squeezefs-A
#        sudo bash ... B /scratch/tmp/sym-box/squeezefs-B-77f4da1d
#
# Artifacts: $OUT/<phase>.data (perf, 999 Hz, -g), $OUT/<phase>.row (the
# storm's ops/s line), $OUT/<phase>.comms.txt (perf report by comm),
# $OUT/<phase>.tpc-agg.txt (the fuse3-tpc* flat LEAF table, crate hashes
# folded — the sibling aggregator 2026-09-22-sym-box-perf-agg.py; the
# first build's `perf report --comm fuse3-tpc` matched no `fuse3-tpcN`
# comm and wrote an EMPTY table on every leg), and on a DWARF leg
# $OUT/<phase>.callers-<leaf>.txt for each CALLER_LEAVES entry (default
# memcpy_avx512 memmove memcmp — the callers of glibc's frame-pointer-less
# copies, depth CALLER_DEPTH = 4), $OUT/stats_{pre,post}.json.
set -euo pipefail
ARM="${1:?arm label (A|B)}"
BIN="${2:?binary path}"
OUT="${3:-/scratch/tmp/sym-box/perf-phases-$ARM}"
REPO="${REPO:-/scratch/tmp/sym-box/repo}"
DIR="${SQZ_MDSTORM_DIR:-/dev/shm/sqz_mdstorm_perf}"
MNT="$DIR/mnt"
STORM="$DIR/mdstorm"
THREADS="${SQZ_MDSTORM_THREADS:-8}"
SCALE="${SQZ_MDSTORM_SCALE:-100}"
PHASES="${PHASES:-mkdir create rename unlink}"
HZ="${HZ:-999}"
AGG="$(dirname "$0")/2026-09-22-sym-box-perf-agg.py"
[ -f "$AGG" ] || AGG="$REPO/.benchmarks/rigs/2026-09-22-sym-box-perf-agg.py"
[ -f "$AGG" ] || { echo "no aggregator beside this rig or under $REPO/.benchmarks/rigs"; exit 1; }
CALLER_LEAVES="${CALLER_LEAVES:-memcpy_avx512 memmove memcmp}"
CALLER_DEPTH="${CALLER_DEPTH:-4}"
# CALLGRAPH=fp (default, `-g`) or dwarf (`--call-graph dwarf,<bytes>` — names
# the callers glibc's frame-pointer-less memmove/memcmp hide; the `release`
# profile keeps DWARF in-binary; ~16 KiB of stack per sample).
CALLGRAPH="${CALLGRAPH:-fp}"
case "$CALLGRAPH" in
fp) CG=(-g) ;;
dwarf) CG=(--call-graph "dwarf,${DWARF_STACK:-16384}") ;;
*) echo "CALLGRAPH takes fp|dwarf"; exit 1 ;;
esac
[ "$(id -u)" -eq 0 ] || { echo "root required"; exit 1; }
uname -r | grep -q sqz || { echo "not the sqz kernel: $(uname -r)"; exit 1; }
[ -x "$BIN" ] || { echo "no binary $BIN"; exit 1; }
command -v perf >/dev/null || { echo "no perf"; exit 1; }
mkdir -p "$OUT"
echo "== perf-phases arm $ARM: $("$BIN" --version | head -1) sha256 $(sha256sum "$BIN" | cut -c1-16) scale $SCALE threads $THREADS phases [$PHASES] callgraph $CALLGRAPH hz $HZ $(date -u +%FT%TZ)" | tee "$OUT/leg.log"
load="$(cut -d' ' -f1 /proc/loadavg)"
awk -v l="$load" 'BEGIN{exit !(l<2.0)}' || { echo "box not quiet (load1=$load)"; exit 1; }
rm -rf "$DIR"
mkdir -p "$MNT" "$DIR/stage"
cc -O2 -pthread -o "$STORM" "$REPO/tests/mdstorm.c"
truncate -s 2G "$DIR/meta.img"
truncate -s 8G "$DIR/data.img"
"$BIN" format "sqmeta://$DIR/meta.img" "sqdata://$DIR/data.img" --disk-cache-paths "$DIR/stage" --force >"$DIR/format.log" 2>&1
"$BIN" mount "sqmeta://$DIR/meta.img" "$MNT" --daemon --allow-other --log-file "$DIR/mount.log" >>"$DIR/mount.log" 2>&1
for _ in $(seq 1 100); do mountpoint -q "$MNT" && break; sleep 0.2; done
mountpoint -q "$MNT" || { echo "mount never appeared"; cat "$DIR/mount.log"; exit 1; }
PID="$(pgrep -f "mount sqmeta://$DIR/meta.img" | head -1)"
[ -n "$PID" ] || { echo "no daemon pid"; exit 1; }
echo "daemon pid $PID" | tee -a "$OUT/leg.log"
cat "$MNT/.stats" >"$OUT/stats_pre.json"
work="$MNT/storm"; mwork="$MNT/manydirs"
mkdir -p "$work" "$mwork"
n_small=$((20000 * SCALE / 100)); n_big=$((100000 * SCALE / 100))
run_phase() { # phase dir count
  local ph="$1" d="$2" n="$3"
  case " $PHASES " in
  *" $ph "*)
    perf record "${CG[@]}" -F "$HZ" -p "$PID" -o "$OUT/$ph.data" -- "$STORM" "$d" "$THREADS" "$n" "$ph" >"$OUT/$ph.row" 2>"$OUT/$ph.perf-record.log" || true
    ;;
  *) "$STORM" "$d" "$THREADS" "$n" "$ph" >"$OUT/$ph.row" ;;
  esac
  echo "  [$ARM] $(cat "$OUT/$ph.row")" | tee -a "$OUT/leg.log"
}
run_phase mkdir "$work" "$n_small"
run_phase create "$work" "$n_big"
run_phase stat "$work" "$n_big"
run_phase rename "$work" "$n_big"
run_phase unlink "$work" "$n_big"
run_phase manydirs "$mwork" "$n_big"
run_phase rmdir "$work" "$n_small"
cat "$MNT/.stats" >"$OUT/stats_post.json"
"$BIN" umount "$MNT" >/dev/null 2>&1 || true
for _ in $(seq 1 100); do mountpoint -q "$MNT" || break; sleep 0.2; done
mountpoint -q "$MNT" && fusermount3 -uz "$MNT"
for _ in $(seq 1 100); do kill -0 "$PID" 2>/dev/null || break; sleep 0.2; done
for ph in $PHASES; do
  [ -s "$OUT/$ph.data" ] || continue
  perf report -i "$OUT/$ph.data" --no-children --sort comm --stdio 2>/dev/null | grep -v "^#" | grep -v "^$" | head -20 >"$OUT/$ph.comms.txt" || true
  python3 "$AGG" flat "$OUT/$ph.data" fuse3-tpc 400 >"$OUT/$ph.tpc-agg.txt" 2>"$OUT/$ph.tpc-agg.err" ||
    echo "aggregator FAILED on $ph (see $OUT/$ph.tpc-agg.err)" | tee -a "$OUT/leg.log"
  [ -s "$OUT/$ph.tpc-agg.txt" ] || echo "WARN: $ph.tpc-agg.txt is EMPTY — no fuse3-tpc* samples decoded" | tee -a "$OUT/leg.log"
  if [ "$CALLGRAPH" = dwarf ]; then
    for leaf in $CALLER_LEAVES; do
      python3 "$AGG" callers "$OUT/$ph.data" fuse3-tpc "$leaf" "$CALLER_DEPTH" >"$OUT/$ph.callers-$leaf.txt" 2>>"$OUT/$ph.tpc-agg.err" ||
        echo "caller aggregation FAILED on $ph/$leaf" | tee -a "$OUT/leg.log"
    done
  fi
done
rm -rf "$DIR"
echo "DONE $ARM $(date -u +%FT%TZ)" | tee -a "$OUT/leg.log"
