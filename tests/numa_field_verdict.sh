#!/usr/bin/env bash
# NUMA-affinity FIELD VERDICT — stage 0 baseline + stage 1 A/B, A-B-B-A-B-A + A0.
#   A  = candidate tip pair (/scratch/tmp/squeezefs.numa + libsqueezefs_il.so.numa)
#        NUMA placement ON by default (session→node + sibling rotation, arena mbind
#        composed before THP collapse, owner→node partition + locality-first pick,
#        service-thread/queue-worker node pins, node-targeted handler lanes,
#        per-queue payload-arena node binding)
#   B  = dev tip 3ea474b pair (/scratch/tmp/squeezefs.devtip + libsqueezefs_il.so.devtip)
#   A0 = A binary, SQUEEZEFS_NUMA=0 (placement OFF, instrument ALIVE) — the leg is
#        BOTH the attribution control and THE BASELINE CROSSING-RATE CAPTURE:
#        its numa_{local,remote} / fuse3_numa_{local,remote} splits are the
#        campaign's ~50 %-crossing proof on the un-placed topology.
# Venue: cluster_reset v3 nullblk 4-wide ns=2, cache-less, dual-200GbE.
# Store ages across legs: order A-B-B-A-B-A, rows fill+order labeled.
# Engagement (per row): the numa gauge split must swing local on A rows and show
# the crossing split on A0; B rows read 0 (gauges absent) by construction.
set -u
M=/scratch/tmp/test
OUT=/scratch/tmp/numa_verdict
EL=/scratch/tmp/elbencho
BIN_A=/scratch/tmp/squeezefs.numa
SHIM_A=/scratch/tmp/libsqueezefs_il.so.numa
BIN_B=/scratch/tmp/squeezefs.devtip
SHIM_B=/scratch/tmp/libsqueezefs_il.so.devtip
BIN_DEV=/scratch/tmp/squeezefs           # the standing deployed pair — restore target
MOUNT_ARGS="sqmeta:///dev/nvme0n1,/dev/nvme2n1 $M --daemon --interception --allow-other --log-file /tmp/sqz.log"
mkdir -p "$OUT"
CSV="$OUT/verdict.csv"
[ -f "$CSV" ] || echo "leg,row,binary,rep,mibs,fill_pct,elapsed_s,numa_l_d,numa_r_d,numa_local_frac,f3_l_d,f3_r_d,f3_local_frac,numa_nodes,svc_threads,nt_bytes_d,ipc_ops_w_d,ipc_ops_r_d,ipc_bytes_in_d,ipc_bytes_out_d,placed_severs_d,placed_elides_d,seed_read_d,reclaim_queued_d,adm_waits_d,shmem_pmd_max_kb" >"$CSV"
J() { echo "$(date -u +%FT%TZ) numa-affinity agent: $*" >>/scratch/tmp/agent_runs.log; }
sg() { python3 -c "import json;print(json.load(open('$M/.stats'))['metrics'].get('$1',0))" 2>/dev/null || echo 0; }
fill() { df -B1G "$M" 2>/dev/null | awk 'NR==2{print $5}' | tr -d '%'; }
gsnap() {
    python3 -c "
import json
m=json.load(open('$M/.stats'))['metrics']
keys=['numa_local_bytes','numa_remote_bytes','fuse3_numa_local_bytes','fuse3_numa_remote_bytes',
      'numa_nodes','ipc_service_threads','nt_copy_bytes','ipc_ops_write','ipc_ops_read',
      'ipc_bytes_in','ipc_bytes_out','ipc_placed_severs','placed_merge_elides',
      'write_path_seed_read_bytes','block_free_reclaim_queued','write_pipeline_admission_waits',
      'ipc_sessions_total','ipc_descriptor_rejects','ipc_sessions_poisoned']
print(json.dumps({k:m.get(k,0) for k in keys}))
" >"$1"
}
pmd_sampler_start() { # samples daemon ShmemPmdMapped (kB) every 5 s -> $1
    (while :; do
        pid=$(pgrep -f "squeezefs(\.numa|\.devtip)? mount" | head -1)
        [ -n "$pid" ] && awk '/ShmemPmdMapped/ {print $2}' "/proc/$pid/smaps_rollup" 2>/dev/null
        sleep 5
    done) >"$1" &
    echo $!
}
settle() {
    local t0 n qb
    t0=$(date +%s); n=0
    while [ $n -lt 3 ]; do
        qb=$(sg block_free_reclaim_queue_bytes)
        [ "$qb" = "0" ] && n=$((n + 1)) || n=0
        [ $(($(date +%s) - t0)) -gt 1800 ] && { echo "SETTLE TIMEOUT qb=$qb"; return 1; }
        sleep 2
    done
    return 0
}
remount() { # <bin> <envs> <tag>
    J "remount ($3): $1 [$2]"
    "$BIN_DEV" umount $M >>"$OUT/remount.log" 2>&1 || "$BIN_A" umount $M >>"$OUT/remount.log" 2>&1 || "$BIN_B" umount $M >>"$OUT/remount.log" 2>&1 || umount $M 2>/dev/null || true
    for _ in $(seq 1 90); do pgrep -f "squeezefs(\.numa|\.devtip)? mount|squeezefs mount" >/dev/null || break; sleep 1; done
    sleep 2
    (cd /scratch/tmp && env $2 "$1" mount $MOUNT_ARGS >>"$OUT/remount.log" 2>&1) || { echo "MOUNT FAILED ($3)"; J "ABORT: mount failed ($3)"; exit 1; }
    sleep 3
    mount | grep -q "$M" || { echo "MOUNT MISSING ($3)"; J "ABORT: mount missing ($3)"; exit 1; }
}
row() { # <leg> <row> <blabel> <rep> <settle:0|1> <ilshim|-> <extra_client_env> <elbencho-args...>
    local leg="$1" rowname="$2" blabel="$3" rep="$4" dosettle="$5" ilshim="$6" cenv="$7"
    shift 7
    [ "$dosettle" = 1 ] && settle >/dev/null
    local f t0 t1 el sp mibs pmdmax
    f=$(fill)
    gsnap "$OUT/$leg.$rowname.g0"
    sp=$(pmd_sampler_start "$OUT/$leg.$rowname.pmd.samples")
    t0=$(date +%s)
    if [ "$ilshim" != "-" ]; then
        env LD_PRELOAD="$ilshim" $cenv $EL "$@" >"$OUT/$leg.$rowname.elbencho.txt" 2>&1
    else
        $EL "$@" >"$OUT/$leg.$rowname.elbencho.txt" 2>&1
    fi
    t1=$(date +%s); el=$((t1 - t0))
    kill "$sp" 2>/dev/null; wait "$sp" 2>/dev/null
    gsnap "$OUT/$leg.$rowname.g1"
    mibs=$(awk '/Throughput MiB\/s/ {v=$NF} END {print v+0}' "$OUT/$leg.$rowname.elbencho.txt")
    pmdmax=$(sort -n "$OUT/$leg.$rowname.pmd.samples" 2>/dev/null | tail -1); pmdmax=${pmdmax:-0}
    python3 -c "
import json
g0=json.load(open('$OUT/$leg.$rowname.g0')); g1=json.load(open('$OUT/$leg.$rowname.g1'))
d=lambda k: g1.get(k,0)-g0.get(k,0)
nl,nr=d('numa_local_bytes'),d('numa_remote_bytes')
fl,fr=d('fuse3_numa_local_bytes'),d('fuse3_numa_remote_bytes')
lf=(nl/(nl+nr)) if (nl+nr)>0 else -1
ff=(fl/(fl+fr)) if (fl+fr)>0 else -1
print(f\"$leg,$rowname,$blabel,$rep,{float('$mibs')},$f,{$el},{nl},{nr},{lf:.3f},{fl},{fr},{ff:.3f},{g1.get('numa_nodes',0)},{g1.get('ipc_service_threads',0)},{d('nt_copy_bytes')},{d('ipc_ops_write')},{d('ipc_ops_read')},{d('ipc_bytes_in')},{d('ipc_bytes_out')},{d('ipc_placed_severs')},{d('placed_merge_elides')},{d('write_path_seed_read_bytes')},{d('block_free_reclaim_queued')},{d('write_pipeline_admission_waits')},$pmdmax\")" >>"$CSV"
    tail -1 "$CSV"
}
files=""
for i in $(seq 1 16); do files="$files $M/numa/f$i"; done

leg() { # <blabel> <bin> <shim> <denvs> <cenvs> <rep>
    local blabel="$1" bin="$2" shim="$3" denvs="$4" cenvs="$5" rep="$6" tag="leg_${1}_r${6}"
    remount "$bin" "$denvs" "$tag"
    settle >/dev/null
    J "$tag: rm set + settle + fresh/wrkern/wril/rdkern/rdil rows (fill+order labeled)"
    rm -rf $M/numa; sync
    settle >/dev/null
    mkdir -p $M/numa
    row "$tag" fresh   "$blabel" "$rep" 0 - "" --write --direct -t 32 -b 4m -s 8g --nolive $files
    row "$tag" wrkern  "$blabel" "$rep" 1 - "" --write --direct -t 32 -b 4m -s 8g --timelimit 60 --infloop --nolive $files
    row "$tag" wril    "$blabel" "$rep" 1 "$shim" "$cenvs" --write --direct -t 32 -b 4m -s 8g --timelimit 60 --infloop --nolive $files
    row "$tag" rdkern  "$blabel" "$rep" 1 - "" --read --direct -t 32 -b 4m -s 8g --timelimit 60 --infloop --nolive $files
    row "$tag" rdil    "$blabel" "$rep" 1 "$shim" "$cenvs" --read --direct -t 32 -b 4m -s 8g --timelimit 60 --infloop --nolive $files
}

J "FIELD VERDICT numa START: A=$("$BIN_A" --version 2>/dev/null | head -1) [placement ON] vs B=$("$BIN_B" --version 2>/dev/null | head -1) [dev tip 3ea474b]; venue nullblk4w reset-v3; order A-B-B-A-B-A + A0 (A0 = placement OFF = the baseline crossing capture)"
leg A  "$BIN_A" "$SHIM_A" "" "" 1
leg B  "$BIN_B" "$SHIM_B" "" "" 1
leg B  "$BIN_B" "$SHIM_B" "" "" 2
leg A  "$BIN_A" "$SHIM_A" "" "" 2
leg B  "$BIN_B" "$SHIM_B" "" "" 3
leg A  "$BIN_A" "$SHIM_A" "" "" 3
# Attribution + BASELINE leg: tip binary, placement OFF (instrument alive).
leg A0 "$BIN_A" "$SHIM_A" "SQUEEZEFS_NUMA=0" "" 1
remount "$BIN_DEV" "" restore_dev
rm -rf $M/numa; sync
settle >/dev/null || true
J "FIELD VERDICT numa END — standing dev pair restored, bench tree removed, store settled"
echo "=== NUMA VERDICT DONE -> $CSV ==="
