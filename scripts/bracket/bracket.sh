#!/usr/bin/env bash
# THE BRACKET HARNESS (charter §11 "Measurement law"): the brackets every
# headline bar is judged with.
#
#   bracket.sh [options] <row> <A-binary> <B-binary>
#
# Runs the row's instrument A → B → B → A on ONE aging store (the same set,
# the same files, formatted once before leg 1, never reformatted), each leg a
# fresh mount of the leg's binary, a warm-up span EXCLUDED, then a sustained
# measured span (≥ 60 s for a headline row; shorter only under --scoping).
# Per leg it records the instrument's numbers, `device bytes ÷ user bytes`
# (from the daemon's device-write arms AND from /proc/diskstats deltas on the
# backing device(s) — the substrate chain is named in the record), `wareq-sz`
# vs the block size, the reclaim ledger (`queued + elided ≡ terminal frees`),
# the admission ledger (`Σ admit_write_* ≡ user bytes`), the read copy ledger,
# `invariant_tripwires` and every must-stay-0, the tick rescues, the solo
# mount's wire census, the fsync phases' exact sum, and the engagement law
# (a shim row whose `ipc_ops_*` deltas do not account for its ops is INVALID).
# A row whose ledgers do not close is marked INVALID with the reason — never
# silently kept.
#
# Output: a JSON record + a markdown row under
#   bench/brackets/<venue-id>/<row>-<date>.{json,md}
#
# Options:
#   --scoping              label the row SCOPING (a dev-box row is never acceptance)
#   --substrate S          file | nvmet-loop | nvmet-tcp (default file)
#   --duration S           measured span per leg (default 60; < 60 needs --scoping)
#   --warmup S             warm-up span excluded from the record (default 10)
#   --lever binary|posture what A and B differ in: the binary (default) or the
#                          FORMAT posture (A = symmetric default, B = --single-writer,
#                          two sets, each aging across its own legs; one binary)
#   --posture P            symmetric | single-writer for the whole row (lever binary)
#   --mount-opts O         extra `-o` options for every leg (e.g. direct_device_true)
#   --numjobs N            instrument parallelism (default = cpus/2, floor 2)
#   --iodepth N            instrument queue depth (default 32)
#   --meta-size B --data-size B --data-volumes N   the set's geometry
#   --budget-mb N          SQUEEZEFS_MEM_BUDGET_MB for every leg (recorded; default 6144:
#                          above the daemon's unreclaimable floor — 2048 ran Red on the venue's
#                          shared cgroup, an INVALID verdict, not a number; the IL admission cap
#                          is budget/8 and the per-session arena yields to a 2 × cpus session
#                          floor, so the cap holds the fleet at any budget: 21 sessions at 2048)
#   --scratch DIR          default /build/brk-<pid>
#   --out DIR              default <repo>/bench/brackets
#   --legs N               run only the first N legs of A B B A (scoping/debug only)
#   --keep                 keep the scratch (volumes, logs) after the run
#
# Rows live in rows/<row>.sh; fio job files in jobs/; substrates in substrates/.
# Root on the Linux venue. Every child is pid-tagged; only our own children are
# ever killed; the venue is left clean (umount, disconnect, unshare).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
BRK="$HERE/brk.py"
export BRK_JOBS="$HERE/jobs"
export BRK_TOOLS="${BRK_TOOLS:-/build/tools/bin}"

SCOPING=0
SUBSTRATE=file
DURATION=60
WARMUP=10
LEVER=binary
POSTURE=symmetric
MOUNT_OPTS=""
NUMJOBS=""
IODEPTH=32
META_SIZE=$((2 << 30))
DATA_SIZE=$((6 << 30))
DATA_VOLUMES=2
BUDGET_MB=""
SCRATCH=""
OUT="$REPO/bench/brackets"
KEEP=0
LEGS=4

usage() { sed -n '2,45p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 2; }

while [ $# -gt 0 ]; do
  case "$1" in
    --scoping) SCOPING=1 ;;
    --substrate) SUBSTRATE=$2; shift ;;
    --duration) DURATION=$2; shift ;;
    --warmup) WARMUP=$2; shift ;;
    --lever) LEVER=$2; shift ;;
    --posture) POSTURE=$2; shift ;;
    --mount-opts) MOUNT_OPTS=$2; shift ;;
    --numjobs) NUMJOBS=$2; shift ;;
    --iodepth) IODEPTH=$2; shift ;;
    --meta-size) META_SIZE=$2; shift ;;
    --data-size) DATA_SIZE=$2; shift ;;
    --data-volumes) DATA_VOLUMES=$2; shift ;;
    --budget-mb) BUDGET_MB=$2; shift ;;
    --scratch) SCRATCH=$2; shift ;;
    --out) OUT=$2; shift ;;
    --keep) KEEP=1 ;;
    --legs) LEGS=$2; shift ;;
    -h|--help) usage ;;
    --*) echo "bracket: unknown option $1" >&2; usage ;;
    *) break ;;
  esac
  shift
done
[ $# -eq 3 ] || usage
ROW=$1; BIN_A=$(readlink -f "$2"); BIN_B=$(readlink -f "$3")

# ---- preflight ------------------------------------------------------------
[ "$(uname -s)" = Linux ] || { echo "bracket: Linux only (charter §1.2)" >&2; exit 1; }
[ "$(id -u)" = 0 ] || { echo "bracket: root required (mount(2), nvmet configfs)" >&2; exit 1; }
[ -x "$BIN_A" ] && [ -x "$BIN_B" ] || { echo "bracket: binaries must be executable" >&2; exit 1; }
ROW_FILE="$HERE/rows/$ROW.sh"
[ -f "$ROW_FILE" ] || { echo "bracket: no row $ROW_FILE" >&2; exit 1; }
if [ "$DURATION" -lt 60 ] && [ "$SCOPING" = 0 ]; then
  echo "bracket: a measured span under 60 s is scoping only (charter §11); pass --scoping" >&2; exit 1
fi
if [ "$LEGS" != 4 ] && [ "$SCOPING" = 0 ]; then echo "bracket: fewer than 4 legs is scoping only; pass --scoping" >&2; exit 1; fi
case "$LEVER" in binary|posture) ;; *) echo "bracket: --lever binary|posture" >&2; exit 1 ;; esac
case "$SUBSTRATE" in file|nvmet-loop|nvmet-tcp) ;; *) echo "bracket: --substrate file|nvmet-loop|nvmet-tcp" >&2; exit 1 ;; esac
[ -n "$NUMJOBS" ] || NUMJOBS=$(( $(nproc) / 2 )); [ "$NUMJOBS" -ge 2 ] || NUMJOBS=2
export BRK_NUMJOBS=$NUMJOBS BRK_IODEPTH=$IODEPTH BRK_DURATION=$DURATION

PID=$$
[ -n "$SCRATCH" ] || SCRATCH=/build/brk-$PID
mkdir -p "$SCRATCH"
DATE=$(date -u +%Y%m%dT%H%M%SZ)
LOG="$SCRATCH/bracket.log"
log() { printf '%s bracket[%s]: %s\n' "$(date -u +%H:%M:%S)" "$ROW" "$*" | tee -a "$LOG" >&2; }

# The row defines its instrument; the substrate its devices.
# shellcheck source=/dev/null
source "$HERE/lib.sh"
# shellcheck source=/dev/null
source "$HERE/substrates/$SUBSTRATE.sh"
MNT="$SCRATCH/mnt"; mkdir -p "$MNT"
export BRK_MNT="$MNT" BRK_SCRATCH="$SCRATCH"
# shellcheck source=/dev/null
source "$ROW_FILE"
: "${ROW_KIND:?row must set ROW_KIND}" "${ROW_INSTRUMENT:?row must set ROW_INSTRUMENT}"
ROW_NEEDS_SHIM=${ROW_NEEDS_SHIM:-0}
ROW_NEEDS_STAGING=${ROW_NEEDS_STAGING:-0}
# The budget must clear the daemon's UNRECLAIMABLE floor (≈ 2.7 GiB on this
# geometry: 9 queues × 32 MiB kmbuf rings + payload arenas + per-lane read
# slabs + fiber stacks) or the mount runs Red from its first tick and sheds —
# a leg that ran under pressure is INVALID (`mem_budget_green`).
if [ -z "$BUDGET_MB" ]; then BUDGET_MB=6144; fi

# ---- identities ---------------------------------------------------------
# The binaries (and the shim beside each) are COPIED into the scratch first:
# a row measures one immutable build even if the tree it came from is rebuilt
# under it (the shared venue did exactly that mid-matrix); the copies' sha256
# ride the record.
snapshot_binary() {   # src-bin dest-dir → prints the copied binary path
  local src=$1 dst=$2 so
  mkdir -p "$dst/bin" "$dst/lib"
  cp "$src" "$dst/bin/squeezefs"
  so="$(dirname "$src")/../lib/libsqueezefs_il.so"
  [ -f "$so" ] && cp "$so" "$dst/lib/libsqueezefs_il.so"
  printf '%s' "$dst/bin/squeezefs"
}
A_EQ_B=false; [ "$BIN_A" = "$BIN_B" ] && A_EQ_B=true
SRC_A=$BIN_A; SRC_B=$BIN_B
BIN_A=$(snapshot_binary "$SRC_A" "$SCRATCH/A")
if [ "$A_EQ_B" = true ]; then BIN_B=$BIN_A; else BIN_B=$(snapshot_binary "$SRC_B" "$SCRATCH/B"); fi
BUILD_A=$(python3 "$BRK" build "$BIN_A" "$SRC_A")
BUILD_B=$(python3 "$BRK" build "$BIN_B" "$SRC_B")
VENUE=$(python3 "$BRK" venue-id "$(backing_disk_of /build)")
VENUE_ID=$(printf '%s' "$VENUE" | python3 -c 'import json,sys; print(json.load(sys.stdin)["venue_id"])')
OUT_DIR="$OUT/$VENUE_ID"; mkdir -p "$OUT_DIR"
log "venue $VENUE_ID; A=$BUILD_A; B=$BUILD_B; substrate $SUBSTRATE; lever $LEVER; posture $POSTURE; ${DURATION}s legs (+${WARMUP}s warm-up); numjobs $NUMJOBS iodepth $IODEPTH; scratch $SCRATCH"

# The shim pairs on build-commit equality; a dirty build pairs only through the
# seam, which the record then names (a dev-box row is scoping anyway).
shim_env_for() {
  local bin=$1 build=$2 so
  so="$(dirname "$bin")/../lib/libsqueezefs_il.so"
  [ -f "$so" ] || { log "no shim beside $bin ($so)"; return 1; }
  printf 'LD_PRELOAD=%s' "$(readlink -f "$so")"
  if printf '%s' "$build" | grep -q '"dirty": true'; then printf ' SQUEEZEFS_TEST_IPC_ALLOW_DIRTY=1'; fi
}

# ---- teardown -----------------------------------------------------------
cleanup() {
  local rc=$?
  set +e
  trap - EXIT INT TERM
  log "teardown (rc $rc)"
  leg_umount || log "teardown: leg_umount rc $?"
  substrate_down || log "teardown: substrate_down rc $?"
  log "teardown: venue clean (nvmet residue $(ls /sys/kernel/config/nvmet/subsystems 2>/dev/null | grep -c "brk-$$-" || true), mounts $(mount | grep -c "$SCRATCH" || true))"
  if [ "$KEEP" = 0 ]; then rm -rf "$SCRATCH"; else log "kept $SCRATCH"; fi
  exit $rc
}
trap cleanup EXIT INT TERM

# ---- the store(s) -------------------------------------------------------
# Lever binary: one set, posture POSTURE. Lever posture: set A symmetric, set B
# single-writer, each formatted once with the one binary.
NSETS=1; [ "$LEVER" = posture ] && NSETS=2
# Sets META_DEV[set], DATA_DEVS_BY_SET[set] (comma list), META_DISK_DEVS_BY_SET[set],
# DATA_DISK_DEVS_BY_SET[set] (diskstats names), DISK_DEVS (every name to sample), SUBSTRATE_DESC.
substrate_up "$BIN_A" "$NSETS"
set_uris() {   # $1 = set index
  META_URI="sqmeta://${META_DEV[$1]}"
  DATA_URI="sqdata://${DATA_DEVS_BY_SET[$1]}"
}
format_set() {  # $1 = set index, $2 = posture
  set_uris "$1"
  local flags=()
  [ "$2" = single-writer ] && flags+=(--single-writer)
  # A row of the staged arm (`ROW_NEEDS_STAGING=1`) formats the set with a
  # staging root under the scratch: cache paths are a FORMAT decision
  # (charter §6.1); without them a file past the inline bound goes striped
  # and the row would measure the wrong arm.
  local staging_note=""
  if [ "${ROW_NEEDS_STAGING:-0}" = 1 ]; then
    mkdir -p "$SCRATCH/cache-$1"
    flags+=(--disk-cache-paths "$SCRATCH/cache-$1")
    staging_note=" (staging root $SCRATCH/cache-$1)"
  fi
  log "format set $1 ($2): $META_URI $DATA_URI$staging_note"
  "$BIN_A" format "${flags[@]}" "$META_URI" "$DATA_URI" > "$SCRATCH/format-$1.log" 2>&1 || { cat "$SCRATCH/format-$1.log" >&2; exit 1; }
}
if [ "$LEVER" = posture ]; then
  format_set 0 symmetric
  format_set 1 single-writer
else
  format_set 0 "$POSTURE"
fi

# A daemon that dies mid-leg (the venue's other agents sweep `pkill -9 -x
# squeezefs`; a crash) makes the leg INVALID with the reason — recorded, never
# dropped — and the bracket continues on the same store with the next leg.
leg_dead() {
  log "leg $leg_n: DAEMON DIED $1 (pid $LEG_DAEMON_PID) — leg INVALID"
  dmesg 2>/dev/null | tail -20 > "$LEG_DIR/dmesg.tail" || true
  D_LEG=$leg_n D_ARM=$arm D_BIN=$BIN D_BUILD=$BUILD D_WHEN="$1" D_PID=$LEG_DAEMON_PID D_START=${started_at:-} \
  python3 "$BRK" dead-leg "$LEG_DIR/leg.json"
  LEG_RECORDS+=("$LEG_DIR/leg.json")
  cp "$LEG_DIR/daemon.log" "$OUT_DIR/$ROW-$DATE-leg$leg_n-$arm.daemon.log" 2>/dev/null || true
  umount -l "$MNT" 2>/dev/null || true
  LEG_MNT=""; LEG_DAEMON_PID=""
}

# ---- legs ---------------------------------------------------------------
ORDER=(A B B A)
ORDER=("${ORDER[@]:0:$LEGS}")
LEG_RECORDS=()
PREPARED_SETS=""
leg_n=0
for arm in "${ORDER[@]}"; do
  leg_n=$((leg_n + 1))
  LEG_DIR="$SCRATCH/leg$leg_n-$arm"; mkdir -p "$LEG_DIR"
  export BRK_LEG_DIR="$LEG_DIR" BRK_LEG=$leg_n BRK_ARM=$arm
  if [ "$LEVER" = posture ]; then
    BIN=$BIN_A; BUILD=$BUILD_A
    if [ "$arm" = A ]; then set_idx=0; leg_posture=symmetric; else set_idx=1; leg_posture=single-writer; fi
  else
    set_idx=0; leg_posture=$POSTURE
    if [ "$arm" = A ]; then BIN=$BIN_A; BUILD=$BUILD_A; else BIN=$BIN_B; BUILD=$BUILD_B; fi
  fi
  set_uris $set_idx
  export BRK_BIN="$BIN"
  opts="$MOUNT_OPTS"
  BRK_PRELOAD_ENV=""
  if [ "$ROW_NEEDS_SHIM" = 1 ]; then
    opts="${opts:+$opts,}interception"
    BRK_PRELOAD_ENV=$(shim_env_for "$BIN" "$BUILD") || exit 1
  fi
  export BRK_PRELOAD_ENV
  log "leg $leg_n arm $arm: $BIN (set $set_idx, $leg_posture) -o '${opts:-}'"
  leg_mount "$BIN" "$META_URI" "$MNT" "$LEG_DIR/daemon.log" "$opts"
  if ! printf '%s' "$PREPARED_SETS" | grep -q ":$set_idx:"; then
    log "leg $leg_n: preparing the aging store's dataset (once per set)"
    row_prepare "$MNT" >> "$LEG_DIR/prepare.log" 2>&1
    PREPARED_SETS="$PREPARED_SETS:$set_idx:"
  fi
  # Warm-up: the same instrument, its output discarded.
  if [ "$WARMUP" -gt 0 ]; then
    log "leg $leg_n: warm-up ${WARMUP}s (excluded)"
    warm_rc=0
    row_run "$MNT" "$WARMUP" "$LEG_DIR/warmup" warmup >> "$LEG_DIR/warmup.log" 2>&1 || warm_rc=$?
    if ! daemon_alive; then leg_dead "during the warm-up (instrument rc $warm_rc)"; continue; fi
    if [ "$warm_rc" != 0 ]; then
      log "leg $leg_n: warm-up instrument rc $warm_rc — leg INVALID; see $LEG_DIR/warmup.log"
      tail -5 "$LEG_DIR/warmup.log" >&2
      D_LEG=$leg_n D_ARM=$arm D_BIN=$BIN D_BUILD=$BUILD D_WHEN="(warm-up instrument rc $warm_rc, daemon alive)" D_PID=$LEG_DAEMON_PID D_START=$(date -u +%Y-%m-%dT%H:%M:%SZ) D_CHECK=instrument_ok \
      python3 "$BRK" dead-leg "$LEG_DIR/leg.json"
      LEG_RECORDS+=("$LEG_DIR/leg.json")
      row_cleanup "$MNT" >> "$LEG_DIR/cleanup.log" 2>&1 || true
      leg_umount
      cp "$LEG_DIR/daemon.log" "$OUT_DIR/$ROW-$DATE-leg$leg_n-$arm.daemon.log"
      continue
    fi
    settle "$MNT"
  fi
  started_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  load0=$(cut -d' ' -f1-3 /proc/loadavg)
  if ! daemon_alive; then leg_dead "before the measured span"; continue; fi
  snapshot "$MNT" "$LEG_DIR/before"
  log "leg $leg_n: measured span ${DURATION}s"
  row_rc=0
  row_run "$MNT" "$DURATION" "$LEG_DIR/measure" measure >> "$LEG_DIR/measure.log" 2>&1 || row_rc=$?
  if ! daemon_alive; then leg_dead "during the measured span (instrument rc $row_rc)"; continue; fi
  # An instrument that failed against a live mount (EAGAIN/EIO storms) is a
  # finding: the leg is recorded INVALID with the instrument's rc and the
  # daemon's `answered EAGAIN` count, and the bracket goes on.
  [ "$row_rc" = 0 ] || { log "leg $leg_n: instrument rc $row_rc — leg INVALID ($(grep -c 'io_u error' "$LEG_DIR/measure.log" || true) fio I/O errors); see $LEG_DIR/measure.log"; }
  settle "$MNT"
  snapshot "$MNT" "$LEG_DIR/after"
  load1=$(cut -d' ' -f1-3 /proc/loadavg)
  if ! row_metrics "$LEG_DIR/measure" > "$LEG_DIR/metrics.json" 2> "$LEG_DIR/metrics.err" || [ ! -s "$LEG_DIR/metrics.json" ]; then
    printf '{"instrument": "%s", "ops": 0, "bytes": 0, "elapsed_s": %d, "headline": {"name": "—", "value": 0}}\n' "$ROW" "$DURATION" > "$LEG_DIR/metrics.json"
  fi
  python3 - "$LEG_DIR/metrics.json" "$row_rc" "$(grep -c 'answered EAGAIN' "$LEG_DIR/daemon.log" || true)" "$(grep -c 'io_u error' "$LEG_DIR/measure.log" || true)" <<'PYEOF'
import json, sys
m = json.load(open(sys.argv[1]))
m["instrument_rc"] = int(sys.argv[2])
m["daemon_eagain_lines"] = int(sys.argv[3])
m["instrument_errors"] = max(int(m.get("instrument_errors", 0) or 0), int(sys.argv[4]))
if m["instrument_errors"] and not m.get("instrument_error_text"):
    m["instrument_error_text"] = "fio io_u errors (see measure.log)"
json.dump(m, open(sys.argv[1], "w"))
PYEOF
  M_LEG=$leg_n M_ARM=$arm M_BIN=$BIN M_BUILD=$BUILD M_KIND=$ROW_KIND M_SHIM=$ROW_NEEDS_SHIM M_POSTURE=$leg_posture \
  M_SET=$set_idx M_OPTS="${opts:-}" M_STARTED=$started_at M_LOAD0="$load0" M_LOAD1="$load1" \
  M_DISKS="${DISK_DEVS[*]}" M_META_DISKS="${META_DISK_DEVS_BY_SET[$set_idx]}" M_DATA_DISKS="${DATA_DISK_DEVS_BY_SET[$set_idx]}" \
  M_EAGAIN="$(grep -c 'answered EAGAIN' "$LEG_DIR/daemon.log" || true)" M_EXCL="${DATA_DEVICES_EXCLUSIVE:-0}" \
  python3 - "$LEG_DIR/meta.json" <<'EOF'
import json, os, sys
e = os.environ
json.dump({"leg": int(e["M_LEG"]), "arm": e["M_ARM"], "binary": e["M_BIN"], "build": json.loads(e["M_BUILD"]),
  "row_kind": e["M_KIND"], "shim": e["M_SHIM"] == "1", "solo": True, "posture": e["M_POSTURE"], "set": int(e["M_SET"]),
  "mount_opts": e["M_OPTS"], "started_at": e["M_STARTED"], "loadavg_before": e["M_LOAD0"], "loadavg_after": e["M_LOAD1"],
  "disk_devices": e["M_DISKS"].split(), "meta_disk_devices": e["M_META_DISKS"].split(),
  "data_disk_devices": e["M_DATA_DISKS"].split(), "data_devices_exclusive": e["M_EXCL"] == "1",
  "daemon_eagain_lines": int(e.get("M_EAGAIN") or 0)}, open(sys.argv[1], "w"))
EOF
  python3 "$BRK" leg "$LEG_DIR/before.stats.json" "$LEG_DIR/after.stats.json" "$LEG_DIR/before.diskstats" "$LEG_DIR/after.diskstats" "$LEG_DIR/metrics.json" "$LEG_DIR/meta.json" > "$LEG_DIR/leg.json"
  LEG_RECORDS+=("$LEG_DIR/leg.json")
  log "leg $leg_n: $(python3 -c 'import json,sys; r=json.load(open(sys.argv[1])); m=r["metrics"]; h=m.get("headline") or {}; print(h.get("name","ops/s"), round(h.get("value",0),1), "valid" if r["valid"] else "INVALID " + str([v["check"] for v in r["verdicts"] if not v["ok"]]))' "$LEG_DIR/leg.json")"
  row_cleanup "$MNT" >> "$LEG_DIR/cleanup.log" 2>&1 || true
  leg_umount
  # The daemon's log rides the record (an `error(` line is a finding; the
  # ledger verdicts do not read it, a reviewer does).
  cp "$LEG_DIR/daemon.log" "$OUT_DIR/$ROW-$DATE-leg$leg_n-$arm.daemon.log"
  errs=$(grep -c 'error(' "$LEG_DIR/daemon.log" || true)
  [ "$errs" = 0 ] || log "leg $leg_n: daemon log has $errs error( line(s)"
done

# ---- fold -----------------------------------------------------------------
HEADER="$SCRATCH/header.json"
H_ROW=$ROW H_DATE=$DATE H_SCOPING=$SCOPING H_INSTR="$ROW_INSTRUMENT" H_KIND=$ROW_KIND H_SHIM=$ROW_NEEDS_SHIM \
H_SUB=$SUBSTRATE H_SUBDESC="$SUBSTRATE_DESC" H_VENUE="$VENUE" H_VENUE_ID=$VENUE_ID H_BUILD_A="$BUILD_A" H_BUILD_B="$BUILD_B" \
H_AEQB=$A_EQ_B H_LEVER=$LEVER H_POSTURE=$POSTURE H_OPTS="$MOUNT_OPTS" H_DUR=$DURATION H_WARM=$WARMUP H_NJ=$NUMJOBS H_QD=$IODEPTH \
H_BUDGET=$BUDGET_MB H_META=$META_SIZE H_DATA=$DATA_SIZE H_NVOL=$DATA_VOLUMES H_LOAD="$(cut -d' ' -f1-3 /proc/loadavg)" \
H_NPROC=$(nproc) H_FIO="$(fio --version 2>/dev/null || echo absent)" H_TOOLS=$BRK_TOOLS \
python3 - "$HEADER" <<'EOF'
import json, os, sys
e = os.environ
json.dump({"row": e["H_ROW"], "date": e["H_DATE"], "scoping": e["H_SCOPING"] == "1", "instrument": e["H_INSTR"],
  "row_kind": e["H_KIND"], "shim": e["H_SHIM"] == "1", "substrate": e["H_SUB"], "substrate_desc": e["H_SUBDESC"],
  "venue": json.loads(e["H_VENUE"]), "venue_id": e["H_VENUE_ID"], "build_a": json.loads(e["H_BUILD_A"]),
  "build_b": json.loads(e["H_BUILD_B"]), "a_equals_b": e["H_AEQB"] == "true", "lever": e["H_LEVER"],
  "posture": e["H_POSTURE"], "mount_opts": e["H_OPTS"], "duration_s": int(e["H_DUR"]), "warmup_s": int(e["H_WARM"]),
  "order": "A-B-B-A", "numjobs": int(e["H_NJ"]), "iodepth": int(e["H_QD"]), "mem_budget_mb": int(e["H_BUDGET"]),
  "set_geometry": {"meta_bytes": int(e["H_META"]), "data_bytes": int(e["H_DATA"]), "data_volumes": int(e["H_NVOL"])},
  "loadavg_at_start": e["H_LOAD"], "nproc": int(e["H_NPROC"]), "fio": e["H_FIO"], "tools": e["H_TOOLS"]}, open(sys.argv[1], "w"))
EOF
REC="$OUT_DIR/$ROW-$DATE.json"
python3 "$BRK" fold "$HEADER" "${LEG_RECORDS[@]}" > "$REC" 2> "$OUT_DIR/$ROW-$DATE.md"
cp "$LOG" "$OUT_DIR/$ROW-$DATE.log" 2>/dev/null || true
log "record $REC"
cat "$OUT_DIR/$ROW-$DATE.md"
