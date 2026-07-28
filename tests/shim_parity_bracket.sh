#!/usr/bin/env bash
# tests/shim_parity_bracket.sh — the shim-parity campaign's A-B-B-A
# acceptance bracket (2026-07-28): CAMP (placed sever) vs BASE (dev tip)
# on the TCP devsub substrate (the fabric-sensitive venue — MANDATORY for
# write rows, two-substrate rule).
#
# THE GOVERNING RULE (user directive, verbatim): "The kernel and IPC
# should always at minimum be at par with the IPC out pacing the kernel
# in the majority of benchmarks." The known violation this bracket
# adjudicates: kernel out-streamed the ring path ~15 % at t16×4MiB
# (ingest-economy §3 — 11.7 vs 9.9–10.1 GiB/s).
#
# Sides run CAMP-BASE-BASE-CAMP (the standing alternating-order rule for
# aging stores); each side gets fresh blkdiscard + format + interception
# mount; KD-7 same-commit daemon+shim pairs. Instrument (stated): fio
# psync --zero_buffers --direct=1 (zeros ≈ free on zram — device
# exonerated, client path measured), medians of 3. Engagement EXACT per
# il row (ipc_bytes_in Δ == row bytes); placed-sever engagement
# (ipc_placed_severs / placed_adoptions / placed_merge_elides) printed
# per row; per-row /proc/diskstats write-amplification columns on the
# data namespaces; write_path_seed_read_bytes tripwire asserted 0.
#
# Usage:
#   sudo SQZ_CAMP_BIN=... SQZ_CAMP_SO=... SQZ_BASE_BIN=... SQZ_BASE_SO=... \
#        tests/shim_parity_bracket.sh [rowfilter]
set -u

META_URI="${SQZ_META_URI:-sqmeta:///dev/nvme1n1,/dev/nvme2n1,/dev/nvme3n1,/dev/nvme4n1}"
DATA_URI="${SQZ_DATA_URI:-sqdata:///dev/nvme5n1,/dev/nvme6n1,/dev/nvme7n1,/dev/nvme8n1}"
DATA_DEVS=(${SQZ_DATA_DEVS:-/dev/nvme5n1 /dev/nvme6n1 /dev/nvme7n1 /dev/nvme8n1})
META_DEVS=(${SQZ_META_DEVS:-/dev/nvme1n1 /dev/nvme2n1 /dev/nvme3n1 /dev/nvme4n1})
MOUNT_DIR="${MOUNT_DIR:-/mnt/sqz_parity}"
RESULTS="${SQZ_PB_RESULTS:-/tmp/shim_parity_bracket_$(date +%Y%m%d_%H%M%S)}"
REPS="${SQZ_PB_REPS:-3}"
ROW_FILTER="${1:-.}"
ORDER=(CAMP BASE BASE CAMP)

CAMP_BIN="${SQZ_CAMP_BIN:?}"; CAMP_SO="${SQZ_CAMP_SO:?}"
BASE_BIN="${SQZ_BASE_BIN:?}"; BASE_SO="${SQZ_BASE_SO:?}"

[ "$(id -u)" -eq 0 ] || { echo "run as root"; exit 1; }
for f in "$CAMP_BIN" "$CAMP_SO" "$BASE_BIN" "$BASE_SO"; do
    [ -e "$f" ] || { echo "missing $f"; exit 1; }
done
mkdir -p "$RESULTS" "$MOUNT_DIR"
CSV="$RESULTS/rows.csv"
echo "side,pass,row,rep,iops,bw_mib_s,ipc_bytes_in,placed_severs,placed_adoptions,placed_elides,seed_read_bytes,dev_w_bytes,amp,engage" > "$CSV"

fail() { echo "FAIL: $*"; exit 1; }

kill_daemon() {
    umount "$MOUNT_DIR" 2>/dev/null || umount -l "$MOUNT_DIR" 2>/dev/null || true
    for _ in $(seq 30); do pidof squeezefs >/dev/null || break; sleep 0.5; done
    killall -9 squeezefs 2>/dev/null || true
    sleep 1
}
trap kill_daemon EXIT

snap_stats() {
    python3 -c "import json;print(json.dumps(json.load(open('$MOUNT_DIR/.stats'))['metrics']))" \
        > "$1" 2>/dev/null || echo '{}' > "$1"
}

dev_write_bytes() { # sum of sectors-written × 512 on the data namespaces
    local total=0
    for d in "${DATA_DEVS[@]}"; do
        local name=${d#/dev/}
        local sect
        sect=$(awk -v n="$name" '$3==n{print $10}' /proc/diskstats)
        total=$((total + ${sect:-0} * 512))
    done
    echo "$total"
}

side_up() { # side_up CAMP|BASE
    local side="$1" bin so
    if [ "$side" = CAMP ]; then bin="$CAMP_BIN"; so="$CAMP_SO"; else bin="$BASE_BIN"; so="$BASE_SO"; fi
    kill_daemon
    for d in "${DATA_DEVS[@]}" "${META_DEVS[@]}"; do blkdiscard -f "$d" 2>/dev/null || true; done
    "$bin" format "$META_URI" "$DATA_URI" --force >> "$RESULTS/format.log" 2>&1 || fail "format ($side)"
    sleep 1
    # One retry: the D0 writer flock of the just-exited format process can
    # still be observed for an instant on adjacent invocations.
    for attempt in 1 2; do
        RUST_LOG=warn "$bin" mount "$META_URI" "$MOUNT_DIR" --daemon --allow-other \
            --interception --log-file "$RESULTS/daemon-$side.log" && break
        [ "$attempt" = 2 ] && fail "mount ($side)"
        sleep 3
    done
    for _ in $(seq 40); do mountpoint -q "$MOUNT_DIR" && break; sleep 0.5; done
    mountpoint -q "$MOUNT_DIR" || fail "mount ($side) not up"
    chmod 1777 "$MOUNT_DIR"
    ACTIVE_SO="$so"
}

fio_write_row() { # fio_write_row <side> <pass> <row> <shim01> <rw> <bs> <size> <dirname> <fresh|keep> <extra...>
    local side="$1" pass="$2" row="$3" shim="$4" rw="$5" bs="$6" size="$7" dname="$8" mode="$9"
    shift 9
    echo "$row" | grep -Eq "$ROW_FILTER" || return 0
    local dir="$MOUNT_DIR/$dname"; mkdir -p "$dir"; chmod 1777 "$dir"
    for rep in $(seq "$REPS"); do
        # `fresh` rows recreate their files per rep; `keep` rows (rand
        # overwrites) reuse the prep row's prealloc'd striped files.
        [ "$mode" = fresh ] && rm -f "$dir"/f* 2>/dev/null
        sync -f "$MOUNT_DIR" 2>/dev/null || sync; sleep 2
        local pre="$RESULTS/$side.$pass.$row.r$rep.pre.json" post="$RESULTS/$side.$pass.$row.r$rep.post.json"
        local out="$RESULTS/$side.$pass.$row.r$rep.fio.json"
        local pfx=(env)
        [ "$shim" = 1 ] && pfx=(env LD_PRELOAD="$ACTIVE_SO")
        local dw0; dw0=$(dev_write_bytes)
        snap_stats "$pre"
        "${pfx[@]}" fio --name="$row" --directory="$dir" --filename_format='f$jobnum' \
            --numjobs=16 --thread --group_reporting --ioengine=psync --rw="$rw" \
            --bs="$bs" --direct=1 --zero_buffers --size="$size" "$@" \
            --output-format=json --output="$out" >/dev/null 2>&1 || fail "fio $side/$row r$rep"
        snap_stats "$post"
        local dw1; dw1=$(dev_write_bytes)
        python3 - "$side" "$pass" "$row" "$rep" "$pre" "$post" "$out" "$shim" "$((dw1-dw0))" "$CSV" <<'EOF'
import json, sys
side, pss, row, rep, pre, post, out, shim, devw, csv = sys.argv[1:11]
b = json.load(open(pre)); a = json.load(open(post)); j = json.load(open(out))
d = lambda k: a.get(k, 0) - b.get(k, 0)
wr = [job['write'] for job in j['jobs']]
iops = round(sum(w['iops'] for w in wr))
bw = round(sum(w['bw_bytes'] for w in wr) / 1048576)
user_bytes = sum(w['io_bytes'] for w in wr)
ipc_in = d('ipc_bytes_in')
engage = 'ok'
if shim == '1' and ipc_in != user_bytes:
    engage = f'INVALID ipc_bytes_in {ipc_in} != user {user_bytes}'
if shim == '0' and ipc_in != 0:
    engage = f'INVALID kernel leak {ipc_in}'
amp = (int(devw) / user_bytes) if user_bytes else 0
seed = d('write_path_seed_read_bytes')
line = (f"{side},{pss},{row},{rep},{iops},{bw},{ipc_in},{d('ipc_placed_severs')},"
        f"{d('placed_adoptions')},{d('placed_merge_elides')},{seed},{devw},{amp:.3f},{engage}")
open(csv, 'a').write(line + '\n')
print(f"  [{side} p{pss} {row} r{rep}] bw={bw}MiB/s iops={iops} amp={amp:.3f} "
      f"placed={d('ipc_placed_severs')}/{d('placed_adoptions')}/{d('placed_merge_elides')} {engage}")
if engage != 'ok':
    sys.exit(9)
EOF
        [ $? -eq 0 ] || fail "engagement $side/$row r$rep"
    done
}

for i in "${!ORDER[@]}"; do
    side="${ORDER[$i]}"; pass=$((i+1))
    echo "=== pass $pass: $side ==="
    side_up "$side"
    # The wall venue: t16 × 4 MiB streaming (16 × 1 GiB zero-buffer files).
    fio_write_row "$side" "$pass" "il-t16-b4m" 1 write 4m 1g il-t16-b4m fresh --fallocate=none
    fio_write_row "$side" "$pass" "kern-t16-b4m" 0 write 4m 1g kern-t16-b4m fresh --fallocate=none
    # Small-op guard rows (never trade IOPS for streaming): rand-4k
    # overwrites of prealloc'd striped whole-block files (the W1 shape).
    fio_write_row "$side" "$pass" "il-rand4k-prep" 1 write 1m 512m il-rand4k keep --fallocate=none
    fio_write_row "$side" "$pass" "il-rand4k" 1 randwrite 4k 512m il-rand4k keep \
        --time_based --runtime=10
    fio_write_row "$side" "$pass" "kern-rand4k-prep" 0 write 1m 512m kern-rand4k keep \
        --fallocate=none
    fio_write_row "$side" "$pass" "kern-rand4k" 0 randwrite 4k 512m kern-rand4k keep \
        --time_based --runtime=10
    kill_daemon
done

echo
echo "=== bracket complete → $CSV ==="
column -s, -t "$CSV"
python3 - "$CSV" <<'EOF'
import csv, sys, statistics
rows = list(csv.DictReader(open(sys.argv[1])))
def med(side, row, key='bw_mib_s'):
    v = [float(r[key]) for r in rows if r['side']==side and r['row']==row]
    return statistics.median(v) if v else None
print("\nmedians (of all reps across both passes, and per pass):")
for row in sorted({r['row'] for r in rows}):
    for side in ('CAMP','BASE'):
        per_pass = {}
        for p in sorted({r['pass'] for r in rows if r['side']==side and r['row']==row}):
            v=[float(r['bw_mib_s']) for r in rows if r['side']==side and r['row']==row and r['pass']==p]
            per_pass[p]=statistics.median(v) if v else None
        m = med(side,row)
        print(f"  {row:18s} {side}: median={m} per-pass={per_pass}")
EOF
