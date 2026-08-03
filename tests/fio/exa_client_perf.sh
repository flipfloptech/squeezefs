#!/usr/bin/env bash
# tests/fio/exa_client_perf.sh — our version of the EXA client perf script:
# one command that runs the EXA-parity fio battery WITHOUT the shim (kernel
# FUSE path), then WITH the shim (LD_PRELOAD interception), and prints every
# row formatted side-by-side.
#
# Execution rides tests/fio/run_fio_row.sh verbatim — NUMA fan-out,
# engagement verification off the stats inode, diskstats amp columns, and
# venue labels all belong to the runner; this script owns the battery, the
# per-pass engine law, FAIRNESS (matched offered concurrency + cold-read
# discipline), the KD-7 identity screen, and the report.
#
# Battery (default; --rows subsets): the four EXA-parity canon rows —
#   write_bw   exa_write_bw.job        bs=1M  (also the read rows' prefill)
#   read_bw    exa_read_bw.job         bs=1M
#   randwrite  exa_randwrite_iops.job  bs=4k
#   randread   exa_randread_iops.job   bs=4k
# EXA shape fidelity on the KERNEL pass: libaio, direct=1, qd=8,
# 30 s + 10 s ramp, size=1g/job, njobs=nproc NUMA-split. --sustain lifts
# runtime to 60 s (the house sustain law; the report states which ran).
#
# FAIRNESS, visible by construction (2026-08-02 il-anomalies window —
# `.benchmarks/2026-08-02-il-anomalies.md`):
#   * Matched offered concurrency: psync is structurally qd=1, so a psync
#     shim pass at the kernel pass's njobs offers 1/qd the in-flight ops —
#     the two prior table flaws' root. By default the psync pass scales
#     njobs to njobs×qd so IN-FLIGHT MATCHES the kernel pass
#     (--no-match-inflight restores raw njobs); the per-pass in-flight
#     figure prints in the table either way, so unmatched rows can never
#     read as path deltas.
#   * Cold-read discipline: a read row is COLD only if (a) --allow-remount
#     let the script remount between passes (captured daemon cmdline+env,
#     same binary, restore-verified), or (b) the fileset overflows RAM
#     warmth by construction (set bytes ≥ 2× the R5 `mem_budget_bytes`).
#     Otherwise the pass prints `WARM (label-only)` — never a bare number.
#
# SHIM pass engine law (measured, not assumed: a bs=1M libaio pass through
# the shim measured engagement 0.000 on 2026-08-02 — ops at the slab
# boundary ride the kernel lane): bs=1M rows run psync through the shim,
# bs=4k rows keep libaio (the il libaio interposers). Engine + in-flight
# are labeled per pass; the runner's engagement verdict governs — a
# silent-passthrough shim pass is printed INVALID (passthrough), never
# presented as a shim number.
#
# Integrity screens:
#   * KD-7: the shim must embed the MOUNTED daemon's build commit
#     (refused loud before any row runs).
#   * ipc_bind_refused_budget growth across a shim pass prints the loud
#     hint: pre-eb94f0c binaries cap session arenas at a fixed 2 GiB
#     (raise SQUEEZEFS_IPC_MEM_MAX on the daemon, or upgrade); binaries
#     with the derived cap should never refuse on budget at this scale.
#
# Only ever targets a MOUNTED filesystem path — never raw devices.
#
# usage:
#   tests/fio/exa_client_perf.sh --mount <mountpoint> --shim <libsqueezefs_il.so>
#       [--dir <dir>]                (default <mountpoint>/exa_perf)
#       [--rows write_bw,read_bw,randwrite,randread]   (default: all four)
#       [--sustain]                  (60 s rows instead of 30 s)
#       [--allow-remount]            (cold reads via remount between passes;
#                                     captures + restores the daemon cmdline/env)
#       [--no-match-inflight]        (psync shim passes keep raw njobs)
#       [--njobs N] [--size 1g]      (runner defaults: nproc / 1g)
#       [--data-devs nvme4n1:..]     (diskstats amp columns per row)
#       [--substrate S] [--fill S]   (venue labels, printed + persisted)
#       [--results DIR] [--journal FILE]
#       [--emit-only]                (print the plan + commands, run nothing)
set -u

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)
RUNNER="$SCRIPT_DIR/run_fio_row.sh"

usage() { sed -n '56,71p' "$0" | sed 's/^# \{0,1\}//'; exit 1; }
fail() { echo "FAIL: $*" >&2; exit 1; }

MOUNT="" SHIM="" DIR="" ROWS="write_bw,read_bw,randwrite,randread"
SUSTAIN=0 NJOBS="" SIZE="1g" DATA_DEVS="" SUBSTRATE="unlabeled" FILL="unlabeled"
RESULTS="" JOURNAL="" EMIT_ONLY=0 ALLOW_REMOUNT=0 MATCH_INFLIGHT=1

while [ $# -gt 0 ]; do
    case "$1" in
        --mount) MOUNT="$2"; shift 2 ;;
        --shim) SHIM="$2"; shift 2 ;;
        --dir) DIR="$2"; shift 2 ;;
        --rows) ROWS="$2"; shift 2 ;;
        --sustain) SUSTAIN=1; shift ;;
        --allow-remount) ALLOW_REMOUNT=1; shift ;;
        --no-match-inflight) MATCH_INFLIGHT=0; shift ;;
        --njobs) NJOBS="$2"; shift 2 ;;
        --size) SIZE="$2"; shift 2 ;;
        --data-devs) DATA_DEVS="$2"; shift 2 ;;
        --substrate) SUBSTRATE="$2"; shift 2 ;;
        --fill) FILL="$2"; shift 2 ;;
        --results) RESULTS="$2"; shift 2 ;;
        --journal) JOURNAL="$2"; shift 2 ;;
        --emit-only) EMIT_ONLY=1; shift ;;
        -h|--help) usage ;;
        *) fail "unknown arg: $1" ;;
    esac
done

[ -n "$MOUNT" ] || usage
[ -n "$SHIM" ] || usage
[ -r "$RUNNER" ] || fail "runner not found: $RUNNER"
command -v fio >/dev/null || fail "fio not found"
command -v python3 >/dev/null || fail "python3 not found"
mountpoint -q "$MOUNT" || fail "$MOUNT is not a mountpoint"
[ -r "$MOUNT/.stats" ] || fail "$MOUNT/.stats not readable (not a SqueezeFS mount?)"
[ -f "$SHIM" ] || fail "shim not found: $SHIM"

DIR="${DIR:-$MOUNT/exa_perf}"
RUNTIME=30; MODE="standard (30 s + 10 s ramp)"
[ "$SUSTAIN" -eq 1 ] && { RUNTIME=60; MODE="sustained (60 s + 10 s ramp — the house sustain law)"; }
RAMP=10
# Smoke/plumbing seam (never a quotable posture): SQZ_EXA_RUNTIME=<s>
# shortens rows for harness validation — the mode label says so loudly.
if [ -n "${SQZ_EXA_RUNTIME:-}" ]; then
    RUNTIME="$SQZ_EXA_RUNTIME"
    MODE="SMOKE override (${RUNTIME} s + ${RAMP} s ramp — plumbing proof, not a quotable row)"
fi
NJOBS="${NJOBS:-$(nproc)}"
TS=$(date +%Y%m%d_%H%M%S)
RESULTS="${RESULTS:-/tmp/fio_rows/${TS}_exa_client_perf}"
mkdir -p "$RESULTS"
REPORT="$RESULTS/exa_client_perf_${TS}.txt"

stat_get() { # <key> — one metric off the stats inode (0 when absent)
    python3 -c "import json,sys
m = json.load(open('$MOUNT/.stats')).get('metrics', {})
print(m.get(sys.argv[1], 0))" "$1" 2>/dev/null || echo 0
}

# ---- KD-7 identity screen (before any row runs) ----------------------------
mount_commit() {
    python3 -c "import json
d = json.load(open('$MOUNT/.stats'))
print(d.get('metrics', {}).get('build_commit') or d.get('build_commit', ''))" 2>/dev/null || true
}
MOUNT_COMMIT=$(mount_commit)
[ -n "$MOUNT_COMMIT" ] || fail "could not read build_commit from $MOUNT/.stats"
if ! LC_ALL=C grep -aq "$MOUNT_COMMIT" "$SHIM"; then
    fail "KD-7 identity mismatch: shim $SHIM does not embed the mounted \
daemon's build commit $MOUNT_COMMIT — pair the same-commit daemon+shim \
(dist/<target>/ is the pairing unit) and re-run"
fi

FIO_VERSION=$(fio --version)
NNODES=$(find /sys/devices/system/node -maxdepth 1 -name 'node[0-9]*' 2>/dev/null | wc -l)

# ---- cold-read discipline ----------------------------------------------------
# The daemon's identity for the remount arm: PID, argv, SQUEEZEFS_* env.
daemon_pid() { pgrep -f 'squeezefs moun[t]' | head -1; }

# cold_guarantee <label> — makes the NEXT read pass cold if it can; writes
# the verdict (cold-remount | cold-overflow | WARM) to $RESULTS/<label>.cold
cold_guarantee() {
    local label=$1 verdict="WARM (label-only)"
    if [ "$ALLOW_REMOUNT" -eq 1 ]; then
        local pid
        pid=$(daemon_pid)
        [ -n "$pid" ] || fail "cold remount: no daemon pid found"
        mapfile -d '' -t ARGV < "/proc/$pid/cmdline"
        # SQUEEZEFS_* env travels with the restart (posture-preserving).
        local envs=()
        while IFS= read -r -d '' kv; do
            case "$kv" in SQUEEZEFS_*) envs+=("$kv") ;; esac
        done < "/proc/$pid/environ"
        umount "$MOUNT" 2>/dev/null || { sleep 2; umount "$MOUNT"; } \
            || fail "cold remount: umount failed"
        local i=0
        while kill -0 "$pid" 2>/dev/null && [ $i -lt 40 ]; do
            kill "$pid" 2>/dev/null || true; sleep 0.5; i=$((i+1))
        done
        kill -0 "$pid" 2>/dev/null && { kill -9 "$pid"; sleep 1; }
        env "${envs[@]}" "${ARGV[@]}" || fail "cold remount: daemon relaunch failed"
        for _ in $(seq 1 60); do mountpoint -q "$MOUNT" && break; sleep 0.5; done
        mountpoint -q "$MOUNT" || fail "cold remount: mount did not return"
        # restore verification: same binary identity, transport armed
        local c2="" q=0
        for _ in $(seq 1 20); do
            c2=$(mount_commit); q=$(stat_get transport_queues)
            [ -n "$c2" ] && [ "${q:-0}" -gt 0 ] && break
            sleep 0.5
        done
        [ "$c2" = "$MOUNT_COMMIT" ] || fail "cold remount: build_commit changed ($c2 != $MOUNT_COMMIT)"
        [ "${q:-0}" -gt 0 ] || fail "cold remount: transport never armed"
        verdict="cold (remount, restore-verified: commit unchanged, queues=$q)"
    else
        # cold-by-overflow: fileset bytes >= 2x the R5 budget
        local set_bytes budget
        set_bytes=$(du -sb "$DIR" 2>/dev/null | cut -f1 || echo 0)
        budget=$(stat_get mem_budget_bytes)
        if [ "${budget:-0}" -gt 0 ] && [ "${set_bytes:-0}" -ge $((budget * 2)) ]; then
            verdict="cold (overflow: set $((set_bytes / 1073741824)) GiB >= 2x budget $((budget / 1073741824)) GiB)"
        fi
    fi
    echo "$verdict" > "$RESULTS/${label}.cold"
    echo "  cold-discipline [$label]: $verdict"
}

# ---- the battery ------------------------------------------------------------
# row spec: name | job | class(read/write) | bs | qd | kernel engine | shim engine
battery() {
    cat <<EOF
write_bw|exa_write_bw.job|write|1M|8|libaio|psync
read_bw|exa_read_bw.job|read|1M|8|libaio|psync
randwrite|exa_randwrite_iops.job|write|4k|8|libaio|libaio
randread|exa_randread_iops.job|read|4k|8|libaio|libaio
EOF
}

want_row() { case ",$ROWS," in *",$1,"*) return 0 ;; *) return 1 ;; esac; }

journal() {
    [ -n "$JOURNAL" ] || return 0
    echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) exa-client-perf: $*" >> "$JOURNAL" 2>/dev/null || true
}

# run_pass <row> <job> <class> <bs> <qd> <engine> <pass: kernel|shim>
run_pass() {
    local row=$1 job=$2 class=$3 bs=$4 qd=$5 engine=$6 pass=$7
    local label="${row}-${pass}"
    # Matched offered concurrency: psync is qd=1 structurally — scale its
    # njobs so in-flight matches the kernel pass (in-flight = njobs x qd
    # for libaio, njobs x 1 for psync). Printed per pass; never silent.
    local pass_njobs=$NJOBS inflight
    if [ "$engine" = "psync" ]; then
        if [ "$MATCH_INFLIGHT" -eq 1 ]; then
            pass_njobs=$((NJOBS * qd))
        fi
        inflight=$pass_njobs
    else
        inflight=$((pass_njobs * qd))
    fi
    echo "$inflight $pass_njobs" > "$RESULTS/${label}.inflight"

    # Read rows: establish (or honestly refuse) coldness before the pass.
    [ "$class" = "read" ] && [ "$EMIT_ONLY" -eq 0 ] && cold_guarantee "$label"

    local args=(--job "$SCRIPT_DIR/$job" --dir "$DIR" --mount "$MOUNT"
        --engine "$engine" --bs "$bs" --iodepth "$qd" --size "$SIZE"
        --njobs "$pass_njobs" --runtime "$RUNTIME" --ramp "$RAMP"
        --label "$label" --results "$RESULTS/$label"
        --substrate "$SUBSTRATE" --fill "$FILL" --order "battery-$MODE")
    [ "$pass" = "shim" ] && args+=(--shim "$SHIM")
    [ -n "$DATA_DEVS" ] && args+=(--data-devs "$DATA_DEVS")
    [ -n "$JOURNAL" ] && args+=(--journal "$JOURNAL")

    if [ "$EMIT_ONLY" -eq 1 ]; then
        echo "PLAN $label (in-flight $inflight): bash $RUNNER ${args[*]}"
        return 0
    fi

    local b0 b1
    b0=$(stat_get ipc_bind_refused_budget)
    echo "== $label ($engine bs=$bs qd=$qd njobs=$pass_njobs in-flight=$inflight ${RUNTIME}s) =="
    bash "$RUNNER" "${args[@]}"
    local rc=$?
    echo "$rc" > "$RESULTS/$label/pass.rc" 2>/dev/null || mkdir -p "$RESULTS/$label" && echo "$rc" > "$RESULTS/$label/pass.rc"
    if [ "$pass" = "shim" ]; then
        b1=$(stat_get ipc_bind_refused_budget)
        if [ "$b1" -gt "$b0" ] 2>/dev/null; then
            echo "$((b1 - b0))" > "$RESULTS/$label/bind_refused_budget.delta"
            cat >&2 <<HINT
HINT: ipc_bind_refused_budget grew by $((b1 - b0)) during the $label pass —
      the daemon refused shim sessions on the arena budget. Pre-eb94f0c
      binaries cap session arenas at a fixed 2 GiB: raise the daemon's
      SQUEEZEFS_IPC_MEM_MAX (e.g. 8192) at mount time, or upgrade to a
      binary with the derived cap (>= eb94f0c). Engagement on this row is
      suspect until resolved.
HINT
        fi
    fi
    return "$rc"
}

# ---- header (printed once, persisted) ---------------------------------------
header() {
    cat <<EOF
================================================================================
 SqueezeFS EXA client perf — kernel FUSE path vs LD_PRELOAD interception shim
================================================================================
 instrument : $FIO_VERSION (via tests/fio/run_fio_row.sh: NUMA fan-out across
              $NNODES node(s), engagement off the stats inode, per-row artifacts)
 mode       : $MODE
 shape      : size=$SIZE/job, base njobs=$NJOBS (NUMA-split), direct=1
              kernel pass: libaio qd=8 (EXA canon dims)
              shim pass  : psync for bs=1M rows / libaio for bs=4k rows
              (measured: bs=1M libaio through the shim rides the kernel
              lane — engagement 0.000; engines + IN-FLIGHT labeled per
              pass, matched by default: psync njobs scale to njobs x qd)
 cold reads : $([ "$ALLOW_REMOUNT" -eq 1 ] && echo "remount between passes (--allow-remount)" || echo "overflow check only (un-guaranteed rows print WARM)")
 mount      : $MOUNT (daemon build $MOUNT_COMMIT)
 shim       : $SHIM (KD-7 same-commit verified)
 substrate  : $SUBSTRATE
 fill       : $FILL
 dir        : $DIR
 artifacts  : $RESULTS
================================================================================
EOF
}

# ---- the side-by-side table ----------------------------------------------------
render_table() {
    python3 - "$RESULTS" "$ROWS" <<'EOF'
import json, os, sys

results, rows = sys.argv[1], [r for r in sys.argv[2].split(",") if r]
SPEC = {
    "write_bw":  ("write", "bw",   "1M", ("libaio", "psync")),
    "read_bw":   ("read",  "bw",   "1M", ("libaio", "psync")),
    "randwrite": ("write", "iops", "4k", ("libaio", "libaio")),
    "randread":  ("read",  "iops", "4k", ("libaio", "libaio")),
}

def sidecar(label, suffix):
    try:
        return open(os.path.join(results, f"{label}.{suffix}")).read().strip()
    except OSError:
        return None

def load_pass(row, p):
    label = f"{row}-{p}"
    d = os.path.join(results, label)
    out = {"rc": None, "num": None, "clat": None, "engage": "n/a", "amp": "",
           "inflight": sidecar(label, "inflight"), "cold": sidecar(label, "cold")}
    try:
        out["rc"] = int(open(os.path.join(d, "pass.rc")).read().strip())
    except (OSError, ValueError):
        return None  # pass never ran
    try:
        fio = json.load(open(os.path.join(d, f"{label}.json")))
        direction = SPEC[row][0]
        bw = sum(j.get(direction, {}).get("bw_bytes", 0) for j in fio["jobs"])
        iops = sum(j.get(direction, {}).get("iops", 0) for j in fio["jobs"])
        ios = sum(j.get(direction, {}).get("total_ios", 0) for j in fio["jobs"])
        clw = sum(j.get(direction, {}).get("clat_ns", {}).get("mean", 0.0)
                  * j.get(direction, {}).get("total_ios", 0) for j in fio["jobs"])
        out["num"] = bw if SPEC[row][1] == "bw" else iops
        out["clat"] = (clw / ios / 1e6) if ios else None
        meta = json.load(open(os.path.join(d, f"{label}.meta.json")))
        out["engage"] = meta.get("engagement", "n/a")
        out["amp"] = meta.get("amplification", "")
    except (OSError, ValueError, KeyError, ZeroDivisionError):
        pass
    return out

def fmt(row, r):
    if r is None or r["num"] is None:
        return "  (no data)"
    if SPEC[row][1] == "bw":
        return f"{r['num'] / 1e9:7.2f} GB/s"
    return f"{r['num'] / 1e3:7.1f} kIOPS"

def verdict(row, p):
    if p is None:
        return "NOT RUN"
    if p["rc"] == 4:
        return "INVALID (passthrough)"
    if p["rc"] not in (0, None):
        return f"FAILED (rc={p['rc']})"
    if SPEC[row][0] == "read" and p["cold"] and p["cold"].startswith("WARM"):
        return "WARM (label-only)"
    return "OK"

hdr = (f"{'row':<10} {'bs':<3} {'kernel (engine)':<24} {'shim (engine)':<24} "
       f"{'delta':>7}  {'shim engagement':<22} verdict")
print(hdr)
print("-" * len(hdr))
for row in rows:
    if row not in SPEC:
        print(f"{row:<10} ?   unknown row (valid: {', '.join(SPEC)})")
        continue
    bs, engines = SPEC[row][2], SPEC[row][3]
    k, s = load_pass(row, "kernel"), load_pass(row, "shim")
    kv = f"{fmt(row, k)} ({engines[0]})" if k else "  NOT RUN"
    sv = f"{fmt(row, s)} ({engines[1]})" if s else "  NOT RUN"
    delta = "n/a"
    s_ok = s and s["rc"] == 0 and s["num"]
    k_ok = k and k["rc"] == 0 and k["num"]
    if s_ok and k_ok:
        delta = f"{(s['num'] / k['num'] - 1) * 100:+.1f}%"
    engage = s["engage"] if s else "n/a"
    verd = []
    kver = verdict(row, k)
    if kver != "OK":
        verd.append(f"kernel {kver}")
    verd.append(f"shim {verdict(row, s)}")
    print(f"{row:<10} {bs:<3} {kv:<24} {sv:<24} {delta:>7}  {str(engage):<22} {'; '.join(verd)}")
    for tag, p in (("kernel", k), ("shim", s)):
        if p is None:
            continue
        bits = []
        if p["inflight"]:
            inflight, njobs = (p["inflight"].split() + ["?"])[:2]
            bits.append(f"in-flight {inflight} (njobs {njobs})")
        if p["clat"] is not None:
            bits.append(f"clat_mean {p['clat']:.3f} ms")
        if p["cold"]:
            bits.append(p["cold"])
        if p["amp"]:
            bits.append(p["amp"])
        if bits:
            print(f"{'':>14} {tag}: " + " | ".join(bits))
print()
print("verdict law: a shim row is quotable ONLY when its verdict is OK and its")
print("engagement passed (>= SQZ_FIO_ENGAGE_MIN, default 0.90); INVALID")
print("(passthrough) = the ops rode the kernel lane. A read pass without a")
print("cold guarantee prints WARM (label-only) — never a bare number. In-flight")
print("figures make offered-concurrency mismatches visible by construction.")
EOF
}

# ---- run ---------------------------------------------------------------------
header | tee "$REPORT"
journal "START battery rows=$ROWS mode=\"$MODE\" njobs=$NJOBS match_inflight=$MATCH_INFLIGHT allow_remount=$ALLOW_REMOUNT mount=$MOUNT commit=$MOUNT_COMMIT"

FAILED_HARD=0
while IFS='|' read -r row job class bs qd keng seng; do
    want_row "$row" || continue
    run_pass "$row" "$job" "$class" "$bs" "$qd" "$keng" kernel || {
        rc=$?
        [ "$rc" -ne 4 ] && FAILED_HARD=1
    }
    run_pass "$row" "$job" "$class" "$bs" "$qd" "$seng" shim || {
        rc=$?
        [ "$rc" -ne 4 ] && FAILED_HARD=1
    }
done < <(battery)

if [ "$EMIT_ONLY" -eq 1 ]; then
    echo "EMIT-ONLY: no rows ran (plan above)."
    exit 0
fi

echo
render_table | tee -a "$REPORT"
echo "report: $REPORT"
journal "DONE report=$REPORT failed_hard=$FAILED_HARD"
exit "$FAILED_HARD"
