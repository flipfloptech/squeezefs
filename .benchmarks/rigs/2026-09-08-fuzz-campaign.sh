#!/usr/bin/env bash
# The release fuzz campaign (spec §11 TEST-4, the "Nightly (fuzzing)" tier;
# MANDATORY before a release tag and after any on-disk/wire format change):
# every target under fuzz/fuzz_targets/ for TIME seconds x JOBS libFuzzer
# jobs, LANES targets at a time, under the fenix nightly + cargo-fuzz the
# 1.2 campaign used (.benchmarks/2026-09-03-release-1.2-fuzz.md — that run
# was hand-driven; this is the same recipe as a script). Exits nonzero on
# any crash artifact or any target that failed to run.
#
# NixOS-specific by design (the dev box): `rustup` is not the path here —
# fenix ships patched toolchain binaries; the libFuzzer runtime links
# libstdc++.so.6 dynamically, so LD_LIBRARY_PATH names the gcc lib output;
# everything runs inside `nix-shell` (shell.nix: libfuse3, libclang, the
# jemalloc CFLAGS). Corpora grow under fuzz/corpus/<target>/ (gitignored).
#
# Usage (REPO = the tree under test, default this checkout; box otherwise quiet):
#   OUT=/tmp/release-1.2.2/fuzz TIME=90 JOBS=2 LANES=2 \
#     bash .benchmarks/rigs/2026-09-08-fuzz-campaign.sh [target...]
# Budget: LANES x JOBS workers (2 x 2 = 4 cores ≈ 12.5 % of a 32-thread
# box — the <= 25 % ceiling that keeps the suites' timing undisturbed);
# wall ≈ ceil(targets / LANES) x TIME + the one-time ASan lib build.
#
# The script runs in two phases: the OUTER phase resolves the toolchain and
# re-executes itself inside nix-shell; the INNER phase (FUZZ_INNER=1) runs
# the campaign. One file, no nested quoting.
set -euo pipefail
REPO="${REPO:-$(cd "$(dirname "$0")/../.." && pwd)}"
SELF="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
OUT="${OUT:-$REPO/target/fuzz-campaign}"
TIME="${TIME:-90}"
JOBS="${JOBS:-2}"
LANES="${LANES:-2}"

log() { echo "$*" | tee -a "$OUT/campaign.log"; }

# libFuzzer's final line per job: `#<execs><TAB>DONE   cov: N ft: N corp: ... exec/s: N rss: ...`
# (a TAB between the count and DONE).
verdict() {
    local rc=0 total=0
    log "$(printf '%-20s %6s %13s %10s %6s %8s' target exit execs exec/s cov crashes)"
    for t in $TARGETS; do
        local d="$OUT/$t" ex execs=0 rate=0 cov=0 l done_line n r c crashes
        ex=$(cat "$d/exit" 2>/dev/null || echo 99)
        for l in "$d"/fuzz-*.log; do
            [ -f "$l" ] || continue
            done_line=$(grep -E '^#[0-9]+[[:space:]]*DONE' "$l" | tail -n 1 || true)
            n=$(echo "$done_line" | sed -nE 's/^#([0-9]+)[[:space:]]*DONE.*/\1/p'); execs=$((execs + ${n:-0}))
            r=$(echo "$done_line" | sed -nE 's/.*exec\/s: ([0-9]+).*/\1/p'); rate=$((rate + ${r:-0}))
            c=$(echo "$done_line" | sed -nE 's/.*cov: ([0-9]+).*/\1/p'); [ "${c:-0}" -gt "$cov" ] && cov=$c
        done
        crashes=$(ls "$REPO/fuzz/artifacts/$t" 2>/dev/null | wc -l)
        total=$((total + execs))
        log "$(printf '%-20s %6s %13s %10s %6s %8s' "$t" "$ex" "$execs" "$rate" "$cov" "$crashes")"
        if [ "$ex" != 0 ] || [ "$crashes" != 0 ]; then rc=1; fi
    done
    if [ $rc = 0 ]; then
        log "== fuzz campaign GREEN: $total execs, 0 crashes $(date -Is)"
    else
        log "== fuzz campaign RED (see artifacts/ + <target>/run.log) $(date -Is)"
    fi
    return $rc
}

# REDUCE_ONLY=1: re-tabulate a finished campaign's logs (no toolchain, no runs).
if [ -n "${REDUCE_ONLY:-}" ]; then
    cd "$REPO"
    TARGETS="${TARGETS:-$(ls fuzz/fuzz_targets/*.rs | xargs -n1 basename | sed 's/\.rs$//' | sort | tr '\n' ' ')}"
    verdict
    exit $?
fi

if [ -z "${FUZZ_INNER:-}" ]; then
    cd "$REPO"
    mkdir -p "$OUT"
    if [ "$#" -gt 0 ]; then
        TARGETS="$*"
    else
        TARGETS="$(ls fuzz/fuzz_targets/*.rs | xargs -n1 basename | sed 's/\.rs$//' | sort | tr '\n' ' ')"
    fi
    echo "== fuzz campaign $(date -Is): $(echo "$TARGETS" | wc -w) targets x ${TIME}s x ${JOBS} jobs, ${LANES} lanes; tip $(git log --oneline -1)" | tee "$OUT/campaign.log"
    # --- toolchain: fenix nightly + cargo-fuzz + the gcc lib for libstdc++ ---
    mapfile -t PATHS < <(nix build --no-link --print-out-paths \
        github:nix-community/fenix#minimal.toolchain nixpkgs#cargo-fuzz)
    FENIX=""; CFUZZ=""
    for p in "${PATHS[@]}"; do
        [ -x "$p/bin/cargo-fuzz" ] && CFUZZ="$p"
        [ -x "$p/bin/rustc" ] && FENIX="$p"
    done
    [ -n "$FENIX" ] && [ -n "$CFUZZ" ] || { echo "toolchain resolution failed: ${PATHS[*]}" >&2; exit 2; }
    GCCLIB="$(nix build --no-link --print-out-paths nixpkgs#stdenv.cc.cc.lib)"
    [ -e "$GCCLIB/lib/libstdc++.so.6" ] || { echo "no libstdc++ under $GCCLIB/lib" >&2; exit 2; }
    {
        echo "fenix:      $FENIX ($("$FENIX/bin/rustc" --version))"
        echo "cargo-fuzz: $CFUZZ ($("$CFUZZ/bin/cargo-fuzz" --version 2>/dev/null | head -n 1))"
        echo "gcc lib:    $GCCLIB"
    } | tee -a "$OUT/campaign.log"
    export FUZZ_INNER=1 OUT TIME JOBS LANES TARGETS FENIX CFUZZ GCCLIB
    exec nix-shell "$REPO/shell.nix" --run "$BASH '$SELF'"
fi

# ---------------------------------------------------------------- INNER
cd "$REPO/fuzz"
export PATH="$FENIX/bin:$CFUZZ/bin:$PATH"
export LD_LIBRARY_PATH="$GCCLIB/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
rm -rf artifacts

# One-time ASan build with `cargo fuzz run` semantics (its RUSTFLAGS differ
# from `cargo fuzz build`; a 1 s run of the first target is the warm build
# the 1.2 campaign paid twice).
first="${TARGETS%% *}"
log "-- warm build via $first (1 s) $(date +%T)"
if ! cargo fuzz run "$first" -- -max_total_time=1 -jobs=1 -workers=1 > "$OUT/warm-build.log" 2>&1; then
    tail -n 40 "$OUT/warm-build.log"
    log "warm build FAILED"
    exit 1
fi
rm -f fuzz-*.log

run_one() {
    local t=$1 d="$OUT/$t"
    mkdir -p "$d"
    log "-- $t START $(date +%T)"
    # -jobs writes fuzz-<n>.log per job into the CURRENT dir; a per-target
    # scratch dir keeps two lanes' logs apart.
    ( cd "$d" && cargo fuzz run --fuzz-dir "$REPO/fuzz" "$t" -- \
        -max_total_time="$TIME" -jobs="$JOBS" -workers="$JOBS" ) > "$d/run.log" 2>&1
    echo $? > "$d/exit"
    log "-- $t exit $(cat "$d/exit") $(date +%T)"
}

running=0
for t in $TARGETS; do
    run_one "$t" &
    running=$((running + 1))
    if [ "$running" -ge "$LANES" ]; then
        wait -n
        running=$((running - 1))
    fi
done
wait

# ---------------------------------------------------------------- verdict
verdict
