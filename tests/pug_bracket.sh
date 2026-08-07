#!/usr/bin/env bash
# tests/pug_bracket.sh — probe-up-governor acceptance brackets
# (.benchmarks/2026-07-29-probe-up-governor.md).
#
# The field conviction: the pure-BDP depth governor self-limits (it
# converges to sustaining the CURRENT operating point); forced depth 64
# bought +18 % on the 4-node cluster. Acceptance: the DEFAULT governor
# must reach ≈ the forced-optimal-depth throughput on the dialed-latency
# venue (the rig where the depth term shows), while qd1/small-op rows
# stay flat and probe gauges prove engagement + retreat.
#
# Cells (A = branch binary, B = dev-tip b26c293; A-B-B-A + B-A reps):
#   dial4t4m   — elbencho kernel 4t × 4M × 4g (16 GiB) on the DIALED
#                20 ms null_blk nvmet-tcp namespace, DEFAULT governor.
#   dial4t4m_f64 — same cell, SQUEEZEFS_WRITE_PIPELINE_DEPTH_BLOCKS=64
#                (the field's forced-optimal lever) on BOTH binaries:
#                the fraction default/forced is the headline number.
#   q1_4k / q1_4k_od — fio 4k qd1 (buffered psync / O_DIRECT libaio
#                iodepth=1) on the zram data namespaces: the
#                latency-guard row — IOPS/clat must be flat A vs B and
#                branch probe_ups must stay 0.
#
# Per run: throughput, /proc/diskstats deltas on the run's DATA devices
# (aqu-sz, wareq-KiB, amp = device bytes ÷ user bytes), and the
# write_pipeline_* stats-inode deltas incl. the probe gauges
# (depth_target vs depth_target_base, depth_probe_{ups,backoffs}).
#
# FIO ENGINE POLICY (user ruling 2026-08-07 — the matched-instrument law;
# `.benchmarks/2026-08-07-fio-engine-policy.md`): throughput/IOPS rows
# are libaio+direct=1+stated iodepth; A/Bs use the SAME engine both
# sides (this bracket's A/B is binary-vs-binary — engines match per cell
# by construction); psync survives only labeled. Per cell here:
#   * q1_4k_od: libaio iodepth=1 direct=1 (the O_DIRECT qd1 guard row).
#   * q1_4k (buffered): psync EXPLICITLY (rule 4 — libaio silently
#     degrades to sync on buffered I/O; psync is the honest buffered
#     qd1 latency instrument). Guard rows, never headline throughput.
#
# Substrate: pug tcp devsub (meta nvme17-20n1, zram data nvme21-24n1)
# + the dialed namespace nvme25n1 (configfs null_blk sqzpuglat0,
# completion_nsec=20ms, irqmode=2, max_sectors=8192, nvmet-tcp :54141).
# Instruments: elbencho 3.1-10 (dynamic, sync driver, --direct), fio.
# Usage:
#   sudo tests/pug_bracket.sh <results-dir> [cell-filter-regex]
set -u

RESULTS="${1:?results dir}"
FILTER="${2:-.}"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
BIN_A="${PUG_BIN_A:-/tmp/sqz-pug-target/release/squeezefs}"
BIN_B="${PUG_BIN_B:-/tmp/sqz-dev-tip/target/release/squeezefs}"
META="${PUG_META:-sqmeta:///dev/nvme17n1,/dev/nvme18n1,/dev/nvme19n1,/dev/nvme20n1}"
DIAL_DATA="${PUG_DIAL_DATA:-sqdata:///dev/nvme25n1}"
DIAL_DEV="${PUG_DIAL_DEV:-$(basename "${DIAL_DATA#sqdata://}")}"
ZRAM_DATA="${PUG_ZRAM_DATA:-sqdata:///dev/nvme21n1,/dev/nvme22n1,/dev/nvme23n1,/dev/nvme24n1}"
MNT=/mnt/sqz_pug
CSV="$RESULTS/bracket.csv"

[ "$(id -u)" -eq 0 ] || { echo "run as root"; exit 1; }
[ -x "$BIN_A" ] && [ -x "$BIN_B" ] || { echo "missing binaries"; exit 1; }
mkdir -p "$RESULTS" "$MNT"
echo "cell,binary,rep,result,unit,aqu_sz,wareq_kib,amp,adm_waits,depth_target,depth_target_base,probe_ups,probe_backoffs,fence_drops" >"$CSV"

ds_snap() { # devs... -> "writes sectors weighted_ms"
    awk -v devs="$*" '
        BEGIN { split(devs, d, " "); for (i in d) want[d[i]] = 1 }
        want[$3] { w += $8; s += $10; q += $14 }
        END { print w, s, q }' /proc/diskstats
}

stat_get() { # <file> <key>
    python3 -c "import json,sys;print(json.load(open('$1'))['metrics'].get('$2',0))" 2>/dev/null || echo 0
}

mount_fs() { # bin data_uri depth_env
    local bin="$1" data="$2" depth="$3" tag="$4"
    "$bin" format "$META" "$data" --force >>"$RESULTS/$tag.log" 2>&1 || { echo "format FAILED ($tag)"; exit 1; }
    udevadm settle --timeout=10 2>/dev/null || true
    local envp=(env)
    [ -n "$depth" ] && envp+=("SQUEEZEFS_WRITE_PIPELINE_DEPTH_BLOCKS=$depth")
    local m_ok=0
    for _ in 1 2 3 4 5; do
        if "${envp[@]}" "$bin" mount "$META" "$MNT" --daemon --allow-others \
            --log-file "$RESULTS/$tag.daemon.log" >>"$RESULTS/$tag.log" 2>&1; then
            m_ok=1; break
        fi
        sleep 2
    done
    [ "$m_ok" = 1 ] || { echo "mount FAILED ($tag)"; exit 1; }
    sleep 1
    mkdir -p "$MNT/bench"
}

umount_fs() { # bin tag
    "$1" umount "$MNT" >>"$RESULTS/$2.log" 2>&1 || umount "$MNT" || true
    for _ in $(seq 1 120); do
        pgrep -f "squeezefs mount .* $MNT" >/dev/null || break
        sleep 0.5
    done
    sleep 1
}

emit_row() { # cell blabel rep result unit devs t0 t1 s0 s1 user_bytes st_pre... (stats read live before umount)
    local cell="$1" blabel="$2" rep="$3" result="$4" unit="$5"
    local t0="$6" t1="$7" s0="$8" s1="$9" user_bytes="${10}"
    local st="$MNT/.stats"
    local aw dt dtb pu pb fd
    aw=$(stat_get "$st" write_pipeline_admission_waits)
    dt=$(stat_get "$st" write_pipeline_depth_target)
    dtb=$(stat_get "$st" write_pipeline_depth_target_base)
    pu=$(stat_get "$st" write_pipeline_depth_probe_ups)
    pb=$(stat_get "$st" write_pipeline_depth_probe_backoffs)
    fd=$(stat_get "$st" write_pipeline_fence_drops)
    local row
    row=$(python3 -c "
w0,s0,q0 = '$s0'.split(); w1,s1,q1 = '$s1'.split()
el = $t1 - $t0
dw = int(w1)-int(w0); ds = int(s1)-int(s0); dq = int(q1)-int(q0)
dev_bytes = ds*512
aqu = dq/1000.0/el if el>0 else 0
wareq = dev_bytes/dw/1024.0 if dw>0 else 0
amp = dev_bytes/$user_bytes if $user_bytes>0 else 0
print(f'{aqu:.2f},{wareq:.0f},{amp:.3f}')")
    echo "$cell,$blabel,$rep,$result,$unit,$row,$aw,$dt,$dtb,$pu,$pb,$fd" >>"$CSV"
    tail -1 "$CSV"
}

run_dial() { # cell blabel bin rep depth_env
    local cell="$1" blabel="$2" bin="$3" rep="$4" depth="$5"
    local tag="${cell}_${blabel}_r${rep}"
    echo "=== $tag"
    mount_fs "$bin" "$DIAL_DATA" "$depth" "$tag"
    local s0 s1 t0 t1 out
    s0=$(ds_snap "$DIAL_DEV")
    t0=$(date +%s.%N)
    out=$(elbencho --write --direct -t 4 -b 4M -s 4g --nolive \
        "$MNT/bench/f1" "$MNT/bench/f2" "$MNT/bench/f3" "$MNT/bench/f4" 2>&1)
    t1=$(date +%s.%N)
    s1=$(ds_snap "$DIAL_DEV")
    echo "$out" >"$RESULTS/$tag.elbencho.txt"
    local mibs
    mibs=$(echo "$out" | awk '/Throughput MiB\/s/ { print $NF }' | tail -1)
    emit_row "$cell" "$blabel" "$rep" "${mibs:-0}" "MiB/s" "$t0" "$t1" "$s0" "$s1" $((4 * 4 * 1024 * 1024 * 1024))
    umount_fs "$bin" "$tag"
}

run_q1() { # cell blabel bin rep direct
    local cell="$1" blabel="$2" bin="$3" rep="$4" direct="$5"
    local tag="${cell}_${blabel}_r${rep}"
    echo "=== $tag"
    mount_fs "$bin" "$ZRAM_DATA" "" "$tag"
    local s0 s1 t0 t1 out
    # Engine per the header policy: O_DIRECT guard row = libaio qd1;
    # buffered guard row = psync explicitly (libaio degrades to sync on
    # buffered I/O — rule 4).
    local engine=psync engflags=()
    [ "$direct" = 1 ] && { engine=libaio; engflags=(--iodepth=1); }
    s0=$(ds_snap nvme21n1 nvme22n1 nvme23n1 nvme24n1)
    t0=$(date +%s.%N)
    out=$(fio --name=q1 --filename="$MNT/bench/q1" --size=256m --ioengine="$engine" \
        "${engflags[@]}" \
        --rw=randwrite --bs=4k --direct="$direct" --runtime=10 --time_based \
        --group_reporting 2>&1)
    t1=$(date +%s.%N)
    s1=$(ds_snap nvme21n1 nvme22n1 nvme23n1 nvme24n1)
    echo "$out" >"$RESULTS/$tag.fio.txt"
    local iops clat
    iops=$(echo "$out" | awk -F'[=,]' '/write: IOPS=/ { print $2 }' | head -1)
    clat=$(echo "$out" | grep -m1 " clat " | sed 's/^ *//')
    echo "  $clat" >>"$RESULTS/$tag.fio.txt"
    emit_row "$cell" "$blabel" "$rep" "${iops:-0}" "IOPS" "$t0" "$t1" "$s0" "$s1" 0
    umount_fs "$bin" "$tag"
}

cellq() { echo "$1" | grep -Eq "$FILTER"; }

# --- dial4t4m: DEFAULT governor, A-B-B-A + B-A (3 reps each) ------------
if cellq dial4t4m_default; then
    run_dial dial4t4m_default A "$BIN_A" 1 ""
    run_dial dial4t4m_default B "$BIN_B" 1 ""
    run_dial dial4t4m_default B "$BIN_B" 2 ""
    run_dial dial4t4m_default A "$BIN_A" 2 ""
    run_dial dial4t4m_default B "$BIN_B" 3 ""
    run_dial dial4t4m_default A "$BIN_A" 3 ""
fi

# --- dial4t4m_f64: forced depth 64 (the field lever), both binaries -----
if cellq dial4t4m_f64; then
    run_dial dial4t4m_f64 A "$BIN_A" 1 64
    run_dial dial4t4m_f64 B "$BIN_B" 1 64
    run_dial dial4t4m_f64 B "$BIN_B" 2 64
    run_dial dial4t4m_f64 A "$BIN_A" 2 64
    run_dial dial4t4m_f64 B "$BIN_B" 3 64
    run_dial dial4t4m_f64 A "$BIN_A" 3 64
fi

# --- q1 rows: the latency guard (flat A vs B, branch probe_ups == 0) ----
if cellq q1_4k; then
    run_q1 q1_4k A "$BIN_A" 1 0
    run_q1 q1_4k B "$BIN_B" 1 0
    run_q1 q1_4k B "$BIN_B" 2 0
    run_q1 q1_4k A "$BIN_A" 2 0
    run_q1 q1_4k B "$BIN_B" 3 0
    run_q1 q1_4k A "$BIN_A" 3 0
fi
if cellq q1_4k_od; then
    run_q1 q1_4k_od A "$BIN_A" 1 1
    run_q1 q1_4k_od B "$BIN_B" 1 1
    run_q1 q1_4k_od B "$BIN_B" 2 1
    run_q1 q1_4k_od A "$BIN_A" 2 1
    run_q1 q1_4k_od B "$BIN_B" 3 1
    run_q1 q1_4k_od A "$BIN_A" 3 1
fi

echo "DONE -> $CSV"
