#!/usr/bin/env bash
# run_bench_baseline.sh — the microbench program's TEETH (2026-08-04,
# .benchmarks/2026-08-04-microbench-program.md).
#
# Runs the FULL Criterion bench set (root workspace + the fuse3 fork's own
# workspace) and compares every bench's median ns/op against the committed
# reference under .benchmarks/criterion-baselines/, exiting NONZERO past a
# per-group regression threshold — so hot-path regressions trip locally
# instead of at field windows.
#
# ── Honesty box ─────────────────────────────────────────────────────────
# Baselines are SAME-BOX RELATIVE tripwires, not absolute truth: the
# thermally-capped dev box gives relative deltas only. The script therefore
#   * pins the run to the quiet cores (taskset ${SQZ_BENCH_CPUS:-8-15},
#     nice 10, CARGO_BUILD_JOBS=8 — the house thermal law),
#   * REFUSES to measure when the CPU is hot (Tctl/Tdie ≥ 80 °C) or when
#     any foreign cargo/rustc work is running,
#   * stamps the reference with hostname+commit and refuses cross-box
#     compares unless SQZ_BENCH_ALLOW_FOREIGN_BASELINE=1.
#
# Tier placement (AGENTS.md test tiering): per-commit stays the bench
# SMOKE (`cargo bench --benches -- --test`, unchanged); THIS script is the
# NIGHTLY tier and the pre-merge gate for perf-relevant PRs.
#
# Usage:
#   tests/run_bench_baseline.sh check          # (default) compare vs reference
#   tests/run_bench_baseline.sh save           # (re)record the reference
#   SQZ_BENCH_FILTER=<substr>  — criterion filter (partial compare: only
#                                intersecting benches judged; missing-bench
#                                detection is skipped)
#   SQZ_BENCH_THRESHOLD_PCT=N  — override the DEFAULT threshold (10)
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT_DIR=$(pwd)
BASELINE_DIR="$ROOT_DIR/.benchmarks/criterion-baselines"
REFERENCE="$BASELINE_DIR/reference.json"
MODE="${1:-check}"

CPUS="${SQZ_BENCH_CPUS:-8-15}"
JOBS="${CARGO_BUILD_JOBS:-8}"
DEFAULT_THRESHOLD="${SQZ_BENCH_THRESHOLD_PCT:-10}"
FILTER="${SQZ_BENCH_FILTER:-}"

say()  { printf '\033[1;36m[bench-baseline]\033[0m %s\n' "$*"; }
fail() { printf '\033[1;31m[bench-baseline] FAIL:\033[0m %s\n' "$*" >&2; exit 1; }

command -v python3 >/dev/null || fail "python3 required (median extraction)"

# ── Preflight: quiet box or refuse ──────────────────────────────────────
preflight() {
    # Foreign cargo/rustc work: measurement demands a quiet box. Anything
    # already running when we start is foreign by construction.
    local busy
    busy=$(pgrep -x cargo || true; pgrep -x rustc || true)
    if [[ -n "$busy" ]]; then
        fail "foreign cargo/rustc running (pids: $(echo "$busy" | tr '\n' ' ')) — poll, never contend"
    fi
    # Thermal law: refuse ≥ 80 °C (Tctl/Tdie via hwmon: zenpower/k10temp/coretemp).
    local hw name t max_mc=0
    for hw in /sys/class/hwmon/hwmon*; do
        [[ -r "$hw/name" ]] || continue
        name=$(<"$hw/name")
        case "$name" in
        zenpower|k10temp|coretemp)
            for t in "$hw"/temp[12]_input; do
                [[ -r "$t" ]] || continue
                local v; v=$(<"$t")
                (( v > max_mc )) && max_mc=$v
            done
            ;;
        esac
    done
    if (( max_mc == 0 )); then
        say "WARN: no CPU temperature sensor found — thermal gate skipped"
    elif (( max_mc >= 80000 )); then
        fail "CPU at $((max_mc / 1000)) °C (≥ 80 °C) — let the box cool before measuring"
    else
        say "thermal gate OK: $((max_mc / 1000)) °C"
    fi
    say "pinning to cpus $CPUS, nice 10, $JOBS build jobs (house thermal law)"
}

# Per-group thresholds live in the python comparator below (one table,
# one owner): default $DEFAULT_THRESHOLD %, looser where the venue is
# structurally noisier (thread contention, async fixture-heavy groups).

run_benches() {
    say "running root bench set (this is the NIGHTLY tier — expect tens of minutes)"
    taskset -c "$CPUS" nice -n 10 env CARGO_BUILD_JOBS="$JOBS" \
        cargo bench --benches -- ${FILTER:+"$FILTER"}
    say "running fuse3 bench set (own workspace; --benches: bench targets only)"
    (cd crates/fuse3 && taskset -c "$CPUS" nice -n 10 env CARGO_BUILD_JOBS="$JOBS" \
        cargo bench --benches --features "tokio-runtime,unprivileged" -- ${FILTER:+"$FILTER"})
}

# Extract {bench_id: median_ns} from both criterion trees into $1.
extract_medians() {
    python3 - "$1" <<'PY'
import json, os, sys
out = {}
for prefix, tree in (("root", "target/criterion"),
                     ("fuse3", "crates/fuse3/target/criterion")):
    if not os.path.isdir(tree):
        continue
    for dirpath, _dirs, files in os.walk(tree):
        if os.path.basename(dirpath) != "new" or "estimates.json" not in files:
            continue
        with open(os.path.join(dirpath, "estimates.json")) as f:
            est = json.load(f)
        rel = os.path.relpath(os.path.dirname(dirpath), tree)
        out[f"{prefix}/{rel}"] = est["median"]["point_estimate"]
meta = {
    "_meta": {
        "hostname": os.uname().nodename,
        "commit": os.popen("git rev-parse --short HEAD").read().strip(),
        "date": os.popen("date -u +%Y-%m-%dT%H:%M:%SZ").read().strip(),
    }
}
meta.update(dict(sorted(out.items())))
with open(sys.argv[1], "w") as f:
    json.dump(meta, f, indent=1)
print(f"[bench-baseline] extracted {len(out)} bench medians -> {sys.argv[1]}")
PY
}

case "$MODE" in
save)
    preflight
    run_benches
    mkdir -p "$BASELINE_DIR"
    extract_medians "$REFERENCE"
    say "reference saved: $REFERENCE — commit it (.benchmarks/criterion-baselines/)"
    ;;
check)
    [[ -f "$REFERENCE" ]] || fail "no committed reference at $REFERENCE — run 'save' first"
    preflight
    run_benches
    FRESH=$(mktemp /tmp/bench-baseline-fresh.XXXXXX.json)
    extract_medians "$FRESH"
    export FILTER DEFAULT_THRESHOLD
    export ALLOW_FOREIGN="${SQZ_BENCH_ALLOW_FOREIGN_BASELINE:-0}"
    python3 - <<'PY' "$FRESH" "$REFERENCE" || exit 1
import json, os, sys

with open(sys.argv[1]) as f: fresh = json.load(f)
with open(sys.argv[2]) as f: ref = json.load(f)
meta = ref.pop("_meta", {})
fresh.pop("_meta", None)

host = os.uname().nodename
if meta.get("hostname") and meta["hostname"] != host:
    msg = (f"reference recorded on '{meta['hostname']}', this box is '{host}' — "
           "baselines are SAME-BOX relative tripwires")
    if os.environ.get("ALLOW_FOREIGN") != "1":
        print(f"[bench-baseline] FAIL: {msg} (SQZ_BENCH_ALLOW_FOREIGN_BASELINE=1 overrides)")
        sys.exit(1)
    print(f"[bench-baseline] WARN: {msg} (override active)")

# Threshold table mirrors threshold_for() in the shell wrapper.
def threshold(bench):
    for pre, pct in (("high_concurrency_contention/", 25), ("cluster_dlm/", 25),
                     ("kv_tree/", 20), ("kv_meta_metadata/", 20),
                     ("crypto_compress_throughput/", 15)):
        # ids are '<prefix>/<group>/<...>'
        if bench.split("/", 1)[-1].startswith(pre):
            return pct
    if "add_ref_release_contended_4t" in bench:
        return 25
    return float(os.environ.get("DEFAULT_THRESHOLD", "10"))

partial = bool(os.environ.get("FILTER"))
regressions, missing, new_benches, improved = [], [], [], []
for bench, ref_ns in sorted(ref.items()):
    if bench not in fresh:
        if not partial:
            missing.append(bench)
        continue
    new_ns = fresh[bench]
    pct = threshold(bench)
    delta = (new_ns - ref_ns) / ref_ns * 100.0
    if delta > pct:
        regressions.append((bench, ref_ns, new_ns, delta, pct))
    elif delta < -25.0:
        improved.append((bench, ref_ns, new_ns, delta))
for bench in sorted(fresh):
    if bench not in ref:
        new_benches.append(bench)

for b, r, n, d, p in regressions:
    print(f"[bench-baseline] REGRESSION {b}: {r:,.0f} -> {n:,.0f} ns (+{d:.1f}% > {p:.0f}%)")
for b in missing:
    print(f"[bench-baseline] MISSING {b}: present in reference, absent from this run "
          "(a bench binary broke or a bench was removed — the meta_lv_bench lesson)")
for b, r, n, d in improved:
    print(f"[bench-baseline] note: {b} improved {d:.1f}% ({r:,.0f} -> {n:,.0f} ns) — "
          "stale baseline? re-'save' after landing")
for b in new_benches:
    print(f"[bench-baseline] note: new bench {b} not in reference — re-'save' to adopt")

judged = sum(1 for b in ref if b in fresh)
print(f"[bench-baseline] judged {judged} benches: "
      f"{len(regressions)} regressions, {len(missing)} missing, "
      f"{len(new_benches)} new, {len(improved)} large improvements")
sys.exit(1 if regressions or missing else 0)
PY
    say "PASS — no bench regressed past its threshold"
    ;;
*)
    fail "unknown mode '$MODE' (use: save | check)"
    ;;
esac
