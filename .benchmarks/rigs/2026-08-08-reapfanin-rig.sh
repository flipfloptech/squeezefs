#!/usr/bin/env bash
# 2026-08-08 reap-fanin local A-B-B-A bracket (strixhalo, tcp devsub —
# `.benchmarks/2026-08-08-shim-reap-fanin.md`). Binary-PAIR bracket
# (KD-7 same-commit pairs, remount + fresh format + fresh fileset per
# leg — the A-B-B-A aging rule): C = perf/shim-reap-fanin candidate,
# B = dev tip d10e5a22 base. Rows per leg: rand-4k il 32×8 and 32×32,
# 60 s time_based + 5 s ramp, matched libaio, direct=1, randrepeat=0
# (the shim-iops posture verbatim). Every counted row behind the
# Tctl ≤ 70 °C cooldown gate. Engagement gates FATAL per row:
#   * ipc_ops_read delta ≥ 0.99 × fio ios (silent-passthrough check)
#   * ipc_sessions_poisoned / ipc_descriptor_rejects /
#     ipc_direct_reap_stalls / transport_lease_overlong deltas = 0
#   * candidate legs: ipc_ingress_ns count delta ≥ 0.99 × ipc_ops_read
#     delta (the lever-3 instrument must see the row)
# Recorded per row: fio IOPS + clat p50/p99/p99.9, ipc_direct_phase_ns
# delta histograms (admit/inflight/finish/total), ipc_ingress_ns delta
# (cand), ipc_cqe_wake_{writes,elided} deltas (the lever-1 wake-economy
# instrument), ipc_direct_inline_reaps, per-second iops logs (thirds).
set -u

BASE_T="${BASE_T:-/home/justin/Source/.sqz-reapfanin-base}"
CAND_T="${CAND_T:-/home/justin/Source/.sqz-reapfanin-cand}"
META="${META:-sqmeta:///dev/nvme1n1,/dev/nvme2n1,/dev/nvme3n1,/dev/nvme4n1}"
DATA="${DATA:-sqdata:///dev/nvme5n1,/dev/nvme6n1,/dev/nvme7n1,/dev/nvme8n1}"
MNT="${MNT:-/mnt/sqz-reapfanin}"
OUT="${OUT:-/tmp/reapfanin-0808}"
RUNTIME="${RUNTIME:-60}"
RAMP="${RAMP:-5}"
LEGS_STR="${LEGS_STR:-C1 B1 B2 C2}"
QDS_STR="${QDS_STR:-8 32}"
COOL_MAX="${COOL_MAX:-70}"

read -r -a LEGS <<<"$LEGS_STR"
read -r -a QDS <<<"$QDS_STR"
mkdir -p "$OUT"
fatal() { echo "FATAL: $*" >&2; exit 1; }

pair_dir() { case "$1" in C*) echo "$CAND_T";; B*) echo "$BASE_T";; *) fatal "leg $1";; esac; }

cooldown() { # Tctl ≤ COOL_MAX gate (the shim-iops thermal discipline)
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
  local bin=$1
  if mountpoint -q "$MNT"; then
    "$bin" umount "$MNT" >/dev/null 2>&1 || umount "$MNT" 2>/dev/null || true
  fi
  for _ in $(seq 1 120); do mountpoint -q "$MNT" || break; sleep 1; done
  mountpoint -q "$MNT" && fatal "mountpoint still mounted — wedge class"
  # Scoped to OUR daemon only (a foreign agent cycles its own test
  # daemons on this box — theirs are not ours to wait on or touch).
  for _ in $(seq 1 120); do my_daemon >/dev/null || break; sleep 1; done
  my_daemon >/dev/null && fatal "our daemon still alive after umount"
  return 0
}

foreign_sampler() { # background: foreign squeezefs/cargo activity per 5 s
  local tag=$1
  : > "$OUT/leg$tag-foreign.log"
  while [ ! -e "$OUT/.stop-$tag" ]; do
    { date +%s; pgrep -af "squeezefs" | grep -v "$MNT" | grep -v pgrep || true; \
      pgrep -c -f "cargo (test|build|clippy)" || true; } >> "$OUT/leg$tag-foreign.log"
    sleep 5
  done
}

run_leg() { # $1 = leg tag (C1/B1/...)
  local leg=$1 tdir bin shim
  tdir=$(pair_dir "$leg")
  bin="$tdir/release/squeezefs"
  shim="$tdir/preload-release/libsqueezefs_il.so"
  [ -x "$bin" ] || fatal "missing $bin"
  [ -f "$shim" ] || fatal "missing $shim"
  echo "=== leg $leg (pair $tdir) ==="
  "$bin" --version | tee "$OUT/leg$leg-version.txt"

  cooldown
  "$bin" format "$META" "$DATA" --force >"$OUT/leg$leg-format.log" 2>&1 \
    || fatal "leg $leg: format failed"
  mkdir -p "$MNT"
  SQUEEZEFS_DIRECT_DEVICE_TRUE=1 "$bin" mount "$META" "$MNT" --daemon \
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

  # Prefill 32 × 128 MiB (kernel lane — no shim), same fio name/dir the
  # rows reuse.
  mkdir -p "$MNT/d"
  fio --name=pf --directory="$MNT/d" --nrfiles=1 --filesize=128m \
    --numjobs=32 --rw=write --bs=1M --direct=1 --ioengine=libaio \
    --iodepth=8 --group_reporting --fallocate=none \
    --output-format=json --output="$OUT/leg$leg-prefill.json" >/dev/null 2>&1 \
    || fatal "leg $leg: prefill failed"

  local qd tag
  for qd in "${QDS[@]}"; do
    tag="$leg-qd$qd"
    cooldown
    rm -f "$OUT/.stop-$tag"
    foreign_sampler "$tag" & local sampler=$!
    cat "$MNT/.stats" > "$OUT/leg$leg-qd$qd-pre.json"
    # CLIENT_ENV: optional extra client-side lever (e.g.
    # SQUEEZEFS_IL_REAP_PARK_MAX=4096 = the flat-event-park herd row).
    env ${CLIENT_ENV:-} LD_PRELOAD="$shim" fio --name=pf --directory="$MNT/d" --nrfiles=1 \
      --filesize=128m --numjobs=32 --rw=randread --bs=4k --direct=1 \
      --randrepeat=0 --ioengine=libaio --iodepth="$qd" --time_based \
      --runtime="$RUNTIME" --ramp_time="$RAMP" --group_reporting \
      --write_iops_log="$OUT/leg$leg-qd$qd" --log_avg_msec=1000 \
      --output-format=json --output="$OUT/leg$leg-qd$qd-fio.json" \
      >/dev/null 2>&1 || fatal "leg $leg qd$qd: fio row failed"
    cat "$MNT/.stats" > "$OUT/leg$leg-qd$qd-post.json"
    touch "$OUT/.stop-$tag"; wait "$sampler" 2>/dev/null || true
    python3 "$(dirname "$0")/2026-08-08-reapfanin-analyze.py" \
      "$OUT" "$leg" "$qd" || fatal "leg $leg qd$qd: engagement verdict failed"
  done

  do_umount "$bin"
}

command -v fio >/dev/null || fatal "fio not installed"
mountpoint -q "$MNT" && fatal "$MNT already mounted — refusing (touch nothing you don't own)"
my_daemon >/dev/null && fatal "a squeezefs daemon on OUR mountpoint is already running — refusing"

for leg in "${LEGS[@]}"; do run_leg "$leg"; done
echo "ALL LEGS DONE — table:"
python3 "$(dirname "$0")/2026-08-08-reapfanin-analyze.py" "$OUT" table "${LEGS[@]}"
