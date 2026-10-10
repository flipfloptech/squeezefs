#!/usr/bin/env bash
# One io500 run against a SqueezeFS mount (charter §11; §13 rung 9).
#
#   io500.sh [--scoping] [--substrate file|nvmet-loop|nvmet-tcp] [--np N]
#            [--ini FILE] [--posture P] [--mount-opts O] [--out DIR] [--keep] <binary>
#
# Stands the substrate and ONE mount up exactly as bracket.sh does, runs
# `io500 <ini>` with the datadir on the mount, snapshots `.stats` and
# /proc/diskstats around the run, and copies io500's result directory
# (result_summary.txt, every phase's output) plus the ledgers to
#   bench/brackets/<venue-id>/io500-<date>/
# Under --scoping the bounded `io500-scoping.ini` is the default (60 s
# stonewall, capped sizes — never submittable); the acceptance box runs the
# distribution's config-minimal.ini (the default without --scoping).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
BRK="$HERE/brk.py"
export BRK_TOOLS="${BRK_TOOLS:-/build/tools/bin}"
IO500_SRC="${IO500_SRC:-/build/tools/src/io500}"

SCOPING=0; SUBSTRATE=file; NP=""; INI=""; POSTURE=symmetric; MOUNT_OPTS=""; OUT="$REPO/bench/brackets"; KEEP=0
META_SIZE=$((2 << 30)); DATA_SIZE=$((8 << 30)); DATA_VOLUMES=2; BUDGET_MB=2048; SCRATCH=""
while [ $# -gt 0 ]; do
  case "$1" in
    --scoping) SCOPING=1 ;;
    --substrate) SUBSTRATE=$2; shift ;;
    --np) NP=$2; shift ;;
    --ini) INI=$2; shift ;;
    --posture) POSTURE=$2; shift ;;
    --mount-opts) MOUNT_OPTS=$2; shift ;;
    --out) OUT=$2; shift ;;
    --data-size) DATA_SIZE=$2; shift ;;
    --budget-mb) BUDGET_MB=$2; shift ;;
    --scratch) SCRATCH=$2; shift ;;
    --keep) KEEP=1 ;;
    --*) echo "io500: unknown option $1" >&2; exit 2 ;;
    *) break ;;
  esac
  shift
done
[ $# -eq 1 ] || { sed -n '2,16p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 2; }
BIN=$(readlink -f "$1"); BIN_A=$BIN; export BRK_BIN=$BIN
[ "$(id -u)" = 0 ] || { echo "io500: root required" >&2; exit 1; }
[ -x "$IO500_SRC/io500" ] || { echo "io500: no $IO500_SRC/io500 (build it: see bench/brackets/README.md)" >&2; exit 1; }
[ -n "$NP" ] || NP=$(( $(nproc) / 2 )); [ "$NP" -ge 2 ] || NP=2
if [ -z "$INI" ]; then
  if [ "$SCOPING" = 1 ]; then INI="$HERE/io500-scoping.ini"; else INI="$IO500_SRC/config-minimal.ini"; fi
fi
ROW=io500
PID=$$; [ -n "$SCRATCH" ] || SCRATCH=/build/brk-io500-$PID; mkdir -p "$SCRATCH"
DATE=$(date -u +%Y%m%dT%H%M%SZ); LOG="$SCRATCH/io500.log"
log() { printf '%s io500: %s\n' "$(date -u +%H:%M:%S)" "$*" | tee -a "$LOG" >&2; }
# shellcheck source=lib.sh
source "$HERE/lib.sh"
# shellcheck source=/dev/null
source "$HERE/substrates/$SUBSTRATE.sh"
BUILD=$(python3 "$BRK" build "$BIN")
VENUE=$(python3 "$BRK" venue-id "$(backing_disk_of /build)")
VENUE_ID=$(printf '%s' "$VENUE" | python3 -c 'import json,sys; print(json.load(sys.stdin)["venue_id"])')
OUT_DIR="$OUT/$VENUE_ID/io500-$DATE"; mkdir -p "$OUT_DIR"

cleanup() {
  local rc=$?; set +e; trap - EXIT INT TERM
  log "teardown (rc $rc)"
  leg_umount 2>/dev/null; substrate_down 2>/dev/null
  [ "$KEEP" = 0 ] && rm -rf "$SCRATCH"
  exit $rc
}
trap cleanup EXIT INT TERM

substrate_up "$BIN" 1
META_URI="sqmeta://${META_DEV[0]}"; DATA_URI="sqdata://${DATA_DEVS_BY_SET[0]}"
flags=(); [ "$POSTURE" = single-writer ] && flags+=(--single-writer)
"$BIN" format "${flags[@]}" "$META_URI" "$DATA_URI" > "$SCRATCH/format.log" 2>&1 || { cat "$SCRATCH/format.log" >&2; exit 1; }
MNT="$SCRATCH/mnt"; mkdir -p "$MNT"
leg_mount "$BIN" "$META_URI" "$MNT" "$SCRATCH/daemon.log" "$MOUNT_OPTS"

# io500 runs from a copy of its tree with the ini's datadir pointed at the mount.
RUN="$SCRATCH/io500-run"; mkdir -p "$RUN/results"
cp "$INI" "$RUN/config.ini"
sed -i "s|^datadir = .*|datadir = $MNT/io500|" "$RUN/config.ini"
grep -q '^\[global\]' "$RUN/config.ini" && sed -i "/^\[global\]/a resultdir = $RUN/results" "$RUN/config.ini"
mkdir -p "$MNT/io500"
log "io500 $INI → $RUN/config.ini; np $NP; mount $MNT; build $BUILD"
load0=$(cut -d' ' -f1-3 /proc/loadavg)
snapshot "$MNT" "$SCRATCH/before"
t0=$(date +%s)
set +e
(cd "$RUN" && mpirun --allow-run-as-root --oversubscribe -np "$NP" "$IO500_SRC/io500" "$RUN/config.ini" > "$RUN/io500.out" 2>&1)
rc=$?
set -e
t1=$(date +%s)
settle "$MNT"
snapshot "$MNT" "$SCRATCH/after"
load1=$(cut -d' ' -f1-3 /proc/loadavg)
log "io500 exit $rc after $((t1 - t0)) s"
cp -r "$RUN/results" "$OUT_DIR/" 2>/dev/null || true
cp "$RUN/io500.out" "$RUN/config.ini" "$OUT_DIR/"
RESULT=$(find "$RUN/results" -name result_summary.txt | head -1)
[ -n "$RESULT" ] && cp "$RESULT" "$OUT_DIR/result_summary.txt"
# The ledgers over the whole run, through the leg machinery (no instrument metrics beyond io500's own).
python3 - "$SCRATCH/metrics.json" "$((t1 - t0))" <<'EOF'
import json, sys
json.dump({"instrument": "io500", "ops": 0, "bytes": 0, "elapsed_s": int(sys.argv[2]), "direction": None,
           "headline": {"name": "io500 score", "value": 0}}, open(sys.argv[1], "w"))
EOF
M_LEG=1 M_ARM=A M_BIN=$BIN M_BUILD=$BUILD M_KIND=io500 M_SHIM=0 M_POSTURE=$POSTURE M_SET=0 M_OPTS="$MOUNT_OPTS" \
M_STARTED=$(date -u -d "@$t0" +%Y-%m-%dT%H:%M:%SZ) M_LOAD0="$load0" M_LOAD1="$load1" M_DISKS="${DISK_DEVS[*]}" \
M_META_DISKS="${META_DISK_DEVS_BY_SET[0]}" M_DATA_DISKS="${DATA_DISK_DEVS_BY_SET[0]}" \
python3 - "$SCRATCH/meta.json" <<'EOF'
import json, os, sys
e = os.environ
json.dump({"leg": 1, "arm": "A", "binary": e["M_BIN"], "build": json.loads(e["M_BUILD"]), "row_kind": "io500", "shim": False,
  "solo": True, "posture": e["M_POSTURE"], "set": 0, "mount_opts": e["M_OPTS"], "started_at": e["M_STARTED"],
  "loadavg_before": e["M_LOAD0"], "loadavg_after": e["M_LOAD1"], "disk_devices": e["M_DISKS"].split(),
  "meta_disk_devices": e["M_META_DISKS"].split(), "data_disk_devices": e["M_DATA_DISKS"].split()}, open(sys.argv[1], "w"))
EOF
python3 "$BRK" leg "$SCRATCH/before.stats.json" "$SCRATCH/after.stats.json" "$SCRATCH/before.diskstats" "$SCRATCH/after.diskstats" "$SCRATCH/metrics.json" "$SCRATCH/meta.json" > "$OUT_DIR/ledgers.json"
H_SCOPING=$SCOPING H_INI=$INI H_NP=$NP H_SUB=$SUBSTRATE H_SUBDESC="$SUBSTRATE_DESC" H_VENUE="$VENUE" H_BUILD="$BUILD" H_RC=$rc H_SECS=$((t1 - t0)) H_DATE=$DATE H_POSTURE=$POSTURE H_OPTS="$MOUNT_OPTS" \
python3 - "$OUT_DIR/header.json" <<'EOF'
import json, os, sys
e = os.environ
json.dump({"row": "io500", "scoping": e["H_SCOPING"] == "1", "ini": e["H_INI"], "np": int(e["H_NP"]), "substrate": e["H_SUB"],
  "substrate_desc": e["H_SUBDESC"], "venue": json.loads(e["H_VENUE"]), "build": json.loads(e["H_BUILD"]), "exit": int(e["H_RC"]),
  "elapsed_s": int(e["H_SECS"]), "date": e["H_DATE"], "posture": e["H_POSTURE"], "mount_opts": e["H_OPTS"]}, open(sys.argv[1], "w"), indent=1)
EOF
cp "$SCRATCH/daemon.log" "$OUT_DIR/daemon.log" 2>/dev/null || true
log "results in $OUT_DIR"
[ -n "$RESULT" ] && cat "$RESULT" || tail -40 "$RUN/io500.out"
