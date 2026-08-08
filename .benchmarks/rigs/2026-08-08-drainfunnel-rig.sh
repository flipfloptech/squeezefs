#!/usr/bin/env bash
# 2026-08-08 r3 drain-funnel decomposition rig (strixhalo, tcp devsub —
# `.benchmarks/2026-08-08-shim-reap-fanin-r3.md`). ONE binary pair; each
# leg is a CONFIG row (daemon env ∥ client env), fresh format + fileset +
# remount per leg (aging rule). Default row shape: rand-4k il 32×qd32 —
# the funnel shape the field ingress histogram convicted. Engagement
# gates FATAL per row (the r2 analyzer's set + ingress n ≡ ops).
# Leg spec: "NAME/DAEMON_ENV/CLIENT_ENV" ('-' = none), e.g.
#   R0/-/-  R1/SQUEEZEFS_IPC_SERVICE_THREADS=4/-  R2/-/SQUEEZEFS_IL_SESSIONS=12
set -u

PAIR_T="${PAIR_T:-/home/justin/Source/.sqz-drainfunnel-target}"
META="${META:-sqmeta:///dev/nvme1n1,/dev/nvme2n1,/dev/nvme3n1,/dev/nvme4n1}"
DATA="${DATA:-sqdata:///dev/nvme5n1,/dev/nvme6n1,/dev/nvme7n1,/dev/nvme8n1}"
MNT="${MNT:-/mnt/sqz-drainfunnel}"
OUT="${OUT:-/tmp/drainfunnel-0808}"
RUNTIME="${RUNTIME:-60}"
RAMP="${RAMP:-5}"
QDS_STR="${QDS_STR:-32}"
COOL_MAX="${COOL_MAX:-70}"
LEGS_STR="${LEGS_STR:-R0/-/- R1/SQUEEZEFS_IPC_SERVICE_THREADS=4/- R2/-/SQUEEZEFS_IL_SESSIONS=12 R3/-/SQUEEZEFS_IL_SESSIONS=16}"

read -r -a LEGS <<<"$LEGS_STR"
read -r -a QDS <<<"$QDS_STR"
mkdir -p "$OUT"
fatal() { echo "FATAL: $*" >&2; exit 1; }

BIN="$PAIR_T/release/squeezefs"
SHIM="$PAIR_T/preload-release/libsqueezefs_il.so"
[ -x "$BIN" ] || fatal "missing $BIN"
[ -f "$SHIM" ] || fatal "missing $SHIM"

cooldown() {
  local t
  for _ in $(seq 1 240); do
    t=$(sensors 2>/dev/null | awk '/Tctl/ {gsub(/[+°C]/,"",$2); print int($2); exit}')
    [ -n "$t" ] || fatal "no Tctl reading"
    [ "$t" -le "$COOL_MAX" ] && { echo "  Tctl=${t}C ok"; return 0; }
    sleep 5
  done
  fatal "Tctl never cooled to ${COOL_MAX}C"
}

my_daemon() { pgrep -f "squeezefs mount.*$MNT" 2>/dev/null; }

do_umount() {
  if mountpoint -q "$MNT"; then
    "$BIN" umount "$MNT" >/dev/null 2>&1 || umount "$MNT" 2>/dev/null || true
  fi
  for _ in $(seq 1 120); do mountpoint -q "$MNT" || break; sleep 1; done
  mountpoint -q "$MNT" && fatal "mountpoint still mounted — wedge class"
  for _ in $(seq 1 120); do my_daemon >/dev/null || break; sleep 1; done
  my_daemon >/dev/null && fatal "our daemon still alive after umount"
  return 0
}

pidstat_sampler() { # per-thread CPU of the daemon, two 5 s windows mid-row
  local tag=$1 pid
  pid=$(my_daemon | head -1)
  [ -n "$pid" ] || return 0
  sleep 20
  pidstat -t -p "$pid" 5 2 > "$OUT/leg$tag-pidstat.txt" 2>/dev/null
}

run_leg() { # $1 = "NAME/DAEMON_ENV/CLIENT_ENV"
  local spec=$1 leg denv cenv
  leg="${spec%%/*}"
  denv="$(echo "$spec" | cut -d/ -f2)"
  cenv="$(echo "$spec" | cut -d/ -f3)"
  [ "$denv" = "-" ] && denv=""
  [ "$cenv" = "-" ] && cenv=""
  echo "=== leg $leg (daemon: '${denv}' client: '${cenv}') ==="

  cooldown
  "$BIN" format "$META" "$DATA" --force >"$OUT/leg$leg-format.log" 2>&1 \
    || fatal "leg $leg: format failed"
  mkdir -p "$MNT"
  env $denv SQUEEZEFS_DIRECT_DEVICE_TRUE=1 "$BIN" mount "$META" "$MNT" --daemon \
    --allow-other --interception \
    --log-file "$OUT/leg$leg-mount.log" >/dev/null 2>&1
  local ok=0
  for _ in $(seq 1 60); do
    if mountpoint -q "$MNT" && cat "$MNT/.stats" >/dev/null 2>&1; then ok=1; break; fi
    sleep 1
  done
  [ "$ok" = 1 ] || fatal "leg $leg: mount gate failed"
  grep -q "FUSE-over-io_uring transport armed" "$OUT/leg$leg-mount.log" \
    || fatal "leg $leg: over-uring never armed"

  mkdir -p "$MNT/d"
  fio --name=pf --directory="$MNT/d" --nrfiles=1 --filesize=128m \
    --numjobs=32 --rw=write --bs=1M --direct=1 --ioengine=libaio \
    --iodepth=8 --group_reporting --fallocate=none \
    --output-format=json --output="$OUT/leg$leg-prefill.json" >/dev/null 2>&1 \
    || fatal "leg $leg: prefill failed"

  local qd
  for qd in "${QDS[@]}"; do
    cooldown
    pidstat_sampler "$leg-qd$qd" & local sampler=$!
    cat "$MNT/.stats" > "$OUT/leg$leg-qd$qd-pre.json"
    env $cenv LD_PRELOAD="$SHIM" fio --name=pf --directory="$MNT/d" --nrfiles=1 \
      --filesize=128m --numjobs=32 --rw=randread --bs=4k --direct=1 \
      --randrepeat=0 --ioengine=libaio --iodepth="$qd" --time_based \
      --runtime="$RUNTIME" --ramp_time="$RAMP" --group_reporting \
      --output-format=json --output="$OUT/leg$leg-qd$qd-fio.json" \
      >/dev/null 2>&1 || fatal "leg $leg qd$qd: fio row failed"
    cat "$MNT/.stats" > "$OUT/leg$leg-qd$qd-post.json"
    wait "$sampler" 2>/dev/null || true
    python3 "$(dirname "$0")/2026-08-08-drainfunnel-analyze.py" \
      "$OUT" "$leg" "$qd" || fatal "leg $leg qd$qd: engagement verdict failed"
  done

  do_umount
}

command -v fio >/dev/null || fatal "fio not installed"
command -v pidstat >/dev/null || fatal "pidstat not installed (sysstat)"
mountpoint -q "$MNT" && fatal "$MNT already mounted — refusing"
my_daemon >/dev/null && fatal "a daemon on OUR mountpoint already runs — refusing"

for spec in "${LEGS[@]}"; do run_leg "$spec"; done
echo "ALL LEGS DONE — table:"
python3 "$(dirname "$0")/2026-08-08-drainfunnel-analyze.py" "$OUT" table "${LEGS[@]}"
