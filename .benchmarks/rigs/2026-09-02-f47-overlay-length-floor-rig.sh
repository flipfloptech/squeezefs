#!/usr/bin/env bash
# .benchmarks/rigs/2026-09-02-f47-overlay-length-floor-rig.sh
#
# Finding 47 A/B — the device overlay's length floor (fix/f47-overlay-
# length-floor). Two binaries (A = base dev tip dafa82ca, B = the floor),
# fresh format + mount per row, fio (3.42) as the instrument, the DATA
# namespaces' /proc/diskstats deltas as the write-amplification column
# (device bytes ÷ fio user bytes; writes completed ⇒ wareq-sz), plus the
# routing ledger off the stats inode (overlay_installs /
# overlay_ineligible_sub_cap / extent_parks / fold_passes /
# overlay_gap_seed_bytes / write_through_blocks / patch_writes).
#
# Rows (each on both binaries; the A-leg row in both orders):
#   r4k-sparse   rand-4k O_DIRECT psync into a SPARSE (truncate) file —
#                holes: every op is patch_ineligible_unmapped, end_fsync
#   r4k-fsync1   the field's fill-1 drain shape: the same, --fsync=1
#   aleg-1m      1 MiB sequential O_DIRECT overwrite of a mapped file (the
#                win that must not regress)
#   seq-4k       fat #2: 4 KiB sequential O_DIRECT into a fresh file — the
#                request-size face (wareq-sz)
#   seq-64k      fat #2 at 64 KiB
#
# Usage (root): BIN_A=... BIN_B=... META_URI=sqmeta:///dev/nvmeXn1 \
#   DATA_URI=sqdata:///dev/nvmeYn1,/dev/nvmeZn1 MNT=/mnt/sqz-f47 \
#   RESULTS=/tmp/f47/results $0
set -euo pipefail

BIN_A="${BIN_A:?}"
BIN_B="${BIN_B:?}"
META_URI="${META_URI:?}"
DATA_URI="${DATA_URI:?}"
MNT="${MNT:-/mnt/sqz-f47}"
RESULTS="${RESULTS:-/tmp/f47/results}"
FIO="${FIO:-fio}"
SECS="${SECS:-20}"
mkdir -p "$RESULTS" "$MNT"

die() { echo "FATAL: $*" >&2; exit 1; }

LOAD="$(cut -d' ' -f1 /proc/loadavg)"
NCPU="$(nproc)"
echo "quiet gate: loadavg $LOAD on $NCPU cpus; foreign cargo/rustc: $(pgrep -c -x rustc || true)" | tee "$RESULTS/quiet"

DATA_DEVS=()
IFS=',' read -r -a _dd <<<"${DATA_URI#sqdata://}"
for d in "${_dd[@]}"; do DATA_DEVS+=("$(basename "$d")"); done

disk_col() { # <awk field> — summed across the data namespaces
    local total=0 dev v
    for dev in "${DATA_DEVS[@]}"; do
        v=$(awk -v d="$dev" -v f="$1" '$3 == d { print $f }' /proc/diskstats)
        total=$((total + ${v:-0}))
    done
    echo "$total"
}

stat_get() { # <key>
    python3 -c "
import json
m=json.load(open('$MNT/.stats')).get('metrics',{})
def flat(d,p=''):
    for k,v in d.items():
        if isinstance(v,dict): yield from flat(v,p+k+'.')
        else: yield p+k,v
print(dict(flat(m)).get('$1',0))" 2>/dev/null || echo 0
}

KEYS=(overlay_installs overlay_overwrite_installs overlay_stores overlay_store_bytes
    overlay_ineligible_sub_cap overlay_gap_seeds overlay_gap_seed_bytes overlay_gap_seed_old_bytes
    overlay_open extent_parks fold_passes fold_seed_reads write_through_blocks write_through_bytes
    patch_writes patch_ineligible_unmapped patch_ineligible_adjacent patch_ineligible_oversize)

snap_stats() { # <file>
    : >"$1"
    for k in "${KEYS[@]}"; do echo "$k $(stat_get "$k")" >>"$1"; done
}

CUR_BIN="$BIN_A"
daemon_alive() { # any A/B daemon still holding this mountpoint
    pgrep -f "$(basename "$BIN_A") mount .*$MNT" >/dev/null 2>&1 ||
        pgrep -f "$(basename "$BIN_B") mount .*$MNT" >/dev/null 2>&1
}
teardown() {
    "$CUR_BIN" umount "$MNT" >/dev/null 2>&1 || umount -l "$MNT" 2>/dev/null || true
    for _ in $(seq 30); do
        mountpoint -q "$MNT" || break
        sleep 1
    done
    # The next mount's FUSE-over-io_uring REGISTER fails while the previous
    # daemon process is still tearing down: wait for the PROCESS, not just
    # the mountpoint (the binaries here are not named `squeezefs`).
    for _ in $(seq 60); do
        daemon_alive || break
        sleep 1
    done
    if daemon_alive; then
        pkill -f "$(basename "$BIN_A") mount .*$MNT" 2>/dev/null || true
        pkill -f "$(basename "$BIN_B") mount .*$MNT" 2>/dev/null || true
        sleep 2
    fi
}

fresh_mount() { # <label> <bin>
    local label="$1" bin="$2"
    CUR_BIN="$bin"
    teardown
    "$bin" format "$META_URI" "$DATA_URI" --force >"$RESULTS/$label.format" 2>&1 ||
        die "$label: format failed ($(tail -2 "$RESULTS/$label.format"))"
    # --allow-other: the daemon adopts the sudo caller's uid as the mount
    # owner, and this rig's probes + fio run as root.
    "$bin" mount "$META_URI" "$MNT" --daemon --allow-other --log-file "$RESULTS/$label.daemon.log" \
        >"$RESULTS/$label.mount" 2>&1 || die "$label: mount failed ($(tail -2 "$RESULTS/$label.mount"))"
    for _ in $(seq 60); do
        [ -e "$MNT/.stats" ] && break
        sleep 1
    done
    [ -e "$MNT/.stats" ] || die "$label: mount never became ready"
}

settle_reclaim() {
    for _ in $(seq 90); do
        local qb
        qb=$(python3 -c "import json;m=json.load(open('$MNT/.stats'))['metrics'];print(int(m.get('block_free_reclaim_queue_bytes',0))+int(m.get('block_free_elided_debt_bytes',0)))" 2>/dev/null || echo 0)
        [ "${qb:-0}" -eq 0 ] && break
        sleep 1
    done
}

# measure <label> <fio args...>: diskstats + ledger brackets around one fio
# run; prints one summary line.
measure() {
    local label="$1"; shift
    settle_reclaim
    sync; sleep 1
    local w0 wr0 r0 rd0
    w0=$(disk_col 10); wr0=$(disk_col 8); r0=$(disk_col 6); rd0=$(disk_col 4)
    snap_stats "$RESULTS/$label.stats0"
    "$FIO" --output-format=json --output="$RESULTS/$label.fio.json" "$@" >"$RESULTS/$label.fio.out" 2>&1 ||
        die "$label: fio failed ($(tail -3 "$RESULTS/$label.fio.out"))"
    # Drain what the row owes (the fsync in the job is the durability
    # boundary; reclaim/discard streams are not user bytes and are excluded
    # by sampling before the settle).
    local w1 wr1 r1 rd1
    w1=$(disk_col 10); wr1=$(disk_col 8); r1=$(disk_col 6); rd1=$(disk_col 4)
    snap_stats "$RESULTS/$label.stats1"
    python3 - "$label" "$RESULTS" "$w0" "$w1" "$wr0" "$wr1" "$r0" "$r1" "$rd0" "$rd1" <<'PY'
import json, sys
label, res = sys.argv[1], sys.argv[2]
w0, w1, wr0, wr1, r0, r1, rd0, rd1 = map(int, sys.argv[3:11])
j = json.load(open(f"{res}/{label}.fio.json"))
ub = sum(job["write"]["io_bytes"] for job in j["jobs"])
iops = sum(job["write"]["iops"] for job in j["jobs"])
bw = sum(job["write"]["bw_bytes"] for job in j["jobs"]) / 1e6
clat = max(job["write"]["clat_ns"]["mean"] for job in j["jobs"]) / 1000
dev_w = (w1 - w0) * 512
dev_r = (r1 - r0) * 512
nw = wr1 - wr0
nr = rd1 - rd0
wareq = dev_w / nw / 1024 if nw else 0.0
def led(f):
    return dict(l.split() for l in open(f))
a, b = led(f"{res}/{label}.stats0"), led(f"{res}/{label}.stats1")
d = {k: int(b[k]) - int(a[k]) for k in a}
print(f"ROW {label}: user_w={ub/1e6:.1f}MB dev_w={dev_w/1e6:.1f}MB amp_w={dev_w/ub if ub else 0:.2f}x "
      f"dev_r={dev_r/1e6:.1f}MB amp_r={dev_r/ub if ub else 0:.2f}x writes={nw} wareq-sz={wareq:.1f}KiB "
      f"reads={nr} iops={iops:.0f} bw={bw:.1f}MB/s clat_mean={clat:.0f}us")
print("   ledger:", " ".join(f"{k}={v}" for k, v in d.items() if v))
PY
}

FIO_COMMON=(--ioengine=psync --direct=1 --randrepeat=1 --group_reporting=1 --thread)

row_r4k_sparse() { # <label> <bin> [extra fio args]
    local label="$1" bin="$2"; shift 2
    fresh_mount "$label" "$bin"
    mkdir -p "$MNT/f47"
    truncate -s 1G "$MNT/f47/sparse"
    measure "$label" --name=r4k --filename="$MNT/f47/sparse" --rw=randwrite --bs=4k \
        --size=1G --numjobs=4 --time_based=1 --runtime="$SECS" --end_fsync=1 \
        --fallocate=none "${FIO_COMMON[@]}" "$@"
}

row_aleg() { # <label> <bin>
    local label="$1" bin="$2"
    fresh_mount "$label" "$bin"
    mkdir -p "$MNT/f47"
    # Mint pass (untimed): a mapped 1 GiB file, then the measured window is
    # PURE 1 MiB aligned overwrite.
    "$FIO" --name=mint --filename="$MNT/f47/aleg" --rw=write --bs=1m --size=1G --end_fsync=1 \
        "${FIO_COMMON[@]}" >"$RESULTS/$label.mint.out" 2>&1 || die "$label: mint failed"
    measure "$label" --name=aleg --filename="$MNT/f47/aleg" --rw=write --bs=1m --size=1G \
        --numjobs=1 --loops=3 --end_fsync=1 "${FIO_COMMON[@]}"
}

row_seq() { # <label> <bin> <bs>
    local label="$1" bin="$2" bs="$3"
    fresh_mount "$label" "$bin"
    mkdir -p "$MNT/f47"
    measure "$label" --name=seq --filename="$MNT/f47/seq-$bs" --rw=write --bs="$bs" --size=512m \
        --numjobs=1 --end_fsync=1 "${FIO_COMMON[@]}"
}

echo "=== A = $($BIN_A --version 2>/dev/null | head -1)"
echo "=== B = $($BIN_B --version 2>/dev/null | head -1)"

row_r4k_sparse r4k-sparse-A "$BIN_A"
row_r4k_sparse r4k-sparse-B "$BIN_B"
SECS_SAVE="$SECS"; SECS=8
row_r4k_sparse r4k-fsync1-A "$BIN_A" --fsync=1
row_r4k_sparse r4k-fsync1-B "$BIN_B" --fsync=1
SECS="$SECS_SAVE"
row_aleg aleg-1m-A1 "$BIN_A"
row_aleg aleg-1m-B1 "$BIN_B"
row_aleg aleg-1m-B2 "$BIN_B"
row_aleg aleg-1m-A2 "$BIN_A"
row_seq seq-4k-A "$BIN_A" 4k
row_seq seq-4k-B "$BIN_B" 4k
row_seq seq-64k-A "$BIN_A" 64k
row_seq seq-64k-B "$BIN_B" 64k
teardown
echo "=== done; artifacts in $RESULTS"
