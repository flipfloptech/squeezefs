#!/usr/bin/env bash
# tests/wpd_bracket.sh — write-pipeline-depth acceptance brackets
# (.benchmarks/2026-07-27-write-pipeline-depth.md).
#
# A-B-B-A alternating-order brackets (standing 2026-07-27 comparison rule)
# of elbencho seq-write on the nvmet-tcp devsub rig (instance 'wpd'):
# {16t, 64t} x {1M, 4M} x {kernel, shim} x {relaxed, durable(--sync)},
# BINARY_A (branch tip) vs BINARY_B (dev tip), fresh format per run.
#
# Per run: elbencho last-done MiB/s, device aqu-sz + wareq-sz + write
# amplification from /proc/diskstats deltas on the DATA namespaces, and
# the write_pipeline_* / write_through_* stats-inode deltas (engagement).
#
# Substrate: SQZ_DEVSUB_TRANSPORT=tcp SQZ_DEVSUB_INSTANCE=wpd (state
# /run/squeezefs-devsub-tcp-wpd). Instrument: elbencho 3.1-10 (dynamic),
# --direct, sync driver. Usage:
#   sudo tests/wpd_bracket.sh <results-dir> [cell-filter-regex]
set -u

RESULTS="${1:?results dir}"
FILTER="${2:-.}"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
BIN_A="${WPD_BIN_A:-$REPO/target/release/squeezefs}"
BIN_B="${WPD_BIN_B:-/tmp/sqz-dev-tip/target/release/squeezefs}"
# KD-7 build-commit equality: each binary gets ITS OWN same-commit shim —
# a mismatched pair HELLO-refuses into silent passthrough and the shim row
# reads as kernel-path (ipc_write_delta 0 = INVALID row).
SO_A="${WPD_SO_A:-$REPO/target/preload-release/libsqueezefs_il.so}"
SO_B="${WPD_SO_B:-/tmp/sqz-dev-tip/target/preload-release/libsqueezefs_il.so}"
META="${WPD_META:-sqmeta:///dev/nvme17n1,/dev/nvme18n1,/dev/nvme19n1,/dev/nvme20n1}"
DATA="${WPD_DATA:-sqdata:///dev/nvme21n1,/dev/nvme22n1,/dev/nvme23n1,/dev/nvme24n1}"
read -r -a DATA_DEVS <<<"${WPD_DATA_DEVS:-nvme21n1 nvme22n1 nvme23n1 nvme24n1}"
MNT=/mnt/sqz_wpd
CSV="$RESULTS/bracket.csv"

[ "$(id -u)" -eq 0 ] || { echo "run as root"; exit 1; }
[ -x "$BIN_A" ] && [ -x "$BIN_B" ] || { echo "missing binaries"; exit 1; }
mkdir -p "$RESULTS" "$MNT"
echo "cell,binary,rep,mibs,aqu_sz,wareq_kib,amp,adm_waits,depth_target,wt_blocks,fence_drops,ipc_write_delta" >"$CSV"

ds_snap() { # -> "writes sectors weighted_ms" summed over DATA_DEVS
    awk -v devs="${DATA_DEVS[*]}" '
        BEGIN { split(devs, d, " "); for (i in d) want[d[i]] = 1 }
        want[$3] { w += $8; s += $10; q += $14 }
        END { print w, s, q }' /proc/diskstats
}

stat_get() { # <file> <key>
    python3 -c "import json,sys;print(json.load(open('$1'))['metrics'].get('$2',0))"
}

run_one() { # cell binary_label binary rep threads bs size_per_thread sync shim
    local cell="$1" blabel="$2" bin="$3" rep="$4" t="$5" bs="$6" sz="$7" sync="$8" shim="$9"
    local tag="${cell}_${blabel}_r${rep}"
    echo "=== $tag"
    "$bin" format "$META" "$DATA" --force >>"$RESULTS/$tag.log" 2>&1 || { echo "format FAILED"; exit 1; }
    # Post-format, udevd's change-event probe holds its own BSD flock on
    # the device node briefly; the D0 writer guard refuses while it does.
    udevadm settle --timeout=10 2>/dev/null || true
    local mount_flags=(--daemon --allow-others --log-file "$RESULTS/$tag.daemon.log")
    [ "$shim" = shim ] && mount_flags+=(-o interception)
    local m_ok=0
    for _ in 1 2 3 4 5; do
        if "$bin" mount "$META" "$MNT" "${mount_flags[@]}" >>"$RESULTS/$tag.log" 2>&1; then
            m_ok=1
            break
        fi
        sleep 2
    done
    [ "$m_ok" = 1 ] || { echo "mount FAILED"; exit 1; }
    sleep 1
    mkdir -p "$MNT/bench"

    local s0 s1 st="$MNT/.stats"
    local aw0 wt0 fd0 ipc0
    aw0=$(stat_get "$st" write_pipeline_admission_waits)
    wt0=$(stat_get "$st" write_through_blocks)
    fd0=$(stat_get "$st" write_pipeline_fence_drops)
    ipc0=$(stat_get "$st" ipc_ops_write)
    s0=$(ds_snap)
    local t0 t1
    t0=$(date +%s.%N)
    local eb=(elbencho --write --direct -t "$t" -b "$bs" -s "$sz" --nolive)
    [ "$sync" = sync ] && eb+=(--sync)
    local files=()
    for i in $(seq 1 "$t"); do files+=("$MNT/bench/f$i"); done
    local out so="$SO_A"
    [ "$blabel" = B ] && so="$SO_B"
    if [ "$shim" = shim ]; then
        out=$(env LD_PRELOAD="$so" "${eb[@]}" "${files[@]}" 2>&1)
    else
        out=$("${eb[@]}" "${files[@]}" 2>&1)
    fi
    t1=$(date +%s.%N)
    s1=$(ds_snap)
    echo "$out" >"$RESULTS/$tag.elbencho.txt"

    local mibs aw1 wt1 fd1 ipc1 dt
    mibs=$(echo "$out" | awk '/Throughput MiB\/s/ { print $NF }' | tail -1)
    aw1=$(stat_get "$st" write_pipeline_admission_waits)
    wt1=$(stat_get "$st" write_through_blocks)
    fd1=$(stat_get "$st" write_pipeline_fence_drops)
    ipc1=$(stat_get "$st" ipc_ops_write)
    dt=$(stat_get "$st" write_pipeline_depth_target)
    read -r row <<<"$(python3 -c "
w0,s0,q0 = '$s0'.split(); w1,s1,q1 = '$s1'.split()
el = $t1 - $t0
dw = int(w1)-int(w0); ds = int(s1)-int(s0); dq = int(q1)-int(q0)
dev_bytes = ds*512
user_bytes = $t * $(numfmt --from=iec "${sz^^}" 2>/dev/null || echo 0)
aqu = dq/1000.0/el if el>0 else 0
wareq = dev_bytes/dw/1024.0 if dw>0 else 0
amp = dev_bytes/user_bytes if user_bytes>0 else 0
print(f'{aqu:.2f} {wareq:.0f} {amp:.3f}')")"
    local aqu wareq amp
    read -r aqu wareq amp <<<"$row"
    echo "$cell,$blabel,$rep,${mibs:-0},$aqu,$wareq,$amp,$((aw1-aw0)),$dt,$((wt1-wt0)),$((fd1-fd0)),$((ipc1-ipc0))" >>"$CSV"
    tail -1 "$CSV"

    "$bin" umount "$MNT" >>"$RESULTS/$tag.log" 2>&1 || umount "$MNT" || true
    # Wait for the daemon to exit fully (D0 writer flock release): the next
    # run's mount is refused while the previous holder lives.
    for _ in $(seq 1 60); do
        pgrep -f "squeezefs mount .* $MNT" >/dev/null || break
        sleep 0.5
    done
    sleep 1
}

bracket() { # cell threads bs size sync shim
    local cell="$1"
    echo "$cell" | grep -Eq "$FILTER" || return 0
    # A-B-B-A + B-A: three reps each, both orders represented.
    run_one "$cell" A "$BIN_A" 1 "$2" "$3" "$4" "$5" "$6"
    run_one "$cell" B "$BIN_B" 1 "$2" "$3" "$4" "$5" "$6"
    run_one "$cell" B "$BIN_B" 2 "$2" "$3" "$4" "$5" "$6"
    run_one "$cell" A "$BIN_A" 2 "$2" "$3" "$4" "$5" "$6"
    run_one "$cell" B "$BIN_B" 3 "$2" "$3" "$4" "$5" "$6"
    run_one "$cell" A "$BIN_A" 3 "$2" "$3" "$4" "$5" "$6"
}

# threads bs size/thread sync shim   (16 GiB user bytes per run)
bracket k4t4m 4 4M 4g nosync kernel
bracket k16t1m 16 1M 1g nosync kernel
bracket k64t1m 64 1M 256m nosync kernel
bracket k16t4m 16 4M 1g nosync kernel
bracket k64t4m 64 4M 256m nosync kernel
bracket s16t1m 16 1M 1g nosync shim
bracket s16t4m 16 4M 1g nosync shim
bracket k16t1m_sync 16 1M 1g sync kernel
bracket k16t4m_sync 16 4M 1g sync kernel

echo "DONE -> $CSV"
