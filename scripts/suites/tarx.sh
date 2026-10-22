#!/usr/bin/env bash
# The parallel `tar -x` the reference's acceptance ensured passed — a
# release row beside LTP and fstests (owner, 2026-10-22; class C: the
# release window, the venue exclusive, the installed binary). Two phases
# per run, each on a FRESH set:
#
# PHASE 1 — the reference's `sym-tarx` gate 2 (`~/Source/squeezefs/tests/
# run_mw_matrix.sh` `leg_sym_tarx`, `tests/cloud_sym_rows.sh` `row_tarx`;
# our contract: `docs/porting/sym_tokens_intents_striping.md` D1 — "`tar
# -x` of 2,384 entries into a directory the node created: 0 wire
# verbs/entry") in this venue: TWO mounts of one symmetric set on this
# host, A the manager and B joined (`suite_mount_pair`; the smoke's TWO
# MOUNTS OF ONE SET ON ONE HOST row), the corpus = a pristine linux `fs/`
# tree (the reference's 2,384–2,468 entries; `tarx_corpus` below) tarred
# once into the results dir. A-B-B-A arms: S0 = the manager's own extract
# on A into a directory A created (B mounted and idle), then the joined
# writer B extracts into a directory IT created, twice, then A again. THE
# LAW (the reference's, one venue note): B's mean wall ≤ 1.10 × S0 (the
# reference's venue was a 250 µs netem wire; both mounts share this host's
# loopback, so what the row prices here is the join posture's own cost —
# the gate stays 1.10); on each B arm the wire census — Σ of the joined
# writer's shipped and cross-owner verbs (`joined_wire_verbs`,
# `xv_cross_owner_steps_shipped`, `xv_cross_owner_guard_rpcs`,
# `slot_resolve_rpcs`, `slot_ships`, `dir_stripe_ships`, `dlm_token_grants`,
# `free_shipped_blocks`: `scripts/bracket/brk.py`'s solo-mount set, the
# reference's `dlm_rpcs` with no single twin here, plus the stripe ships) —
# per entry < 0.05 (the destination's mkdir under `/` is the ONE shipped
# step; the extent-grant refills a handful per thousand leaves; the
# membership renewals ride `cluster_wire_frames_sent`, recorded beside,
# never judged: they are the lease's clock, not the extraction's);
# `slot_handovers` 0 on BOTH daemons (a handover inside a local
# extraction); every extracted tree byte-exact against the corpus (a sha256
# manifest + the type/mode/path/link-target listing) read through BOTH
# mounts — the acked-writes-present law: a tree the writer acked ≡ the tree
# the other member reads; `invariant_tripwires` 0 and the FUSE op watchdog
# never overdue on both; `fsck --offline --scrub` clean after both unmount
# (B first, then the manager).
#
# PHASE 2 — the parallel single-mount form (the owner's words: "parallel
# tar"): N = nproc/4 concurrent extractions of the same tarball into N
# fresh directories on ONE mount (the release posture, single writer), all
# byte-exact, the mount healthy, fsck clean.
#
# The multi-run discipline (SUITE_RUNS, default 3; the count aborts at the
# first non-pass). FAIL EARLY (owner ruling 2026-10-17): the first failing
# check that is no ledger row (`docs/gates/tarx-exceptions.md`, ids
# `abba/<check>`, `parallel/<check>`) ends the run — phase 2 is not run
# behind a failed phase 1. The walls and the census are recorded (rows.tsv)
# as evidence; the law is PASS.
#
#   scripts/venue scripts/suites/tarx.sh                 # the 3-run gate
#   SUITE_RUNS=1 scripts/venue scripts/suites/tarx.sh
#   TARX_SRC=/path/to/linux/fs TARX_REPS=1 …             # the corpus by hand; one extraction per arm
#   TARX_PHASES=parallel SUITE_RUNS=1 …                  # one phase (triage)
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

# The corpus: a PRISTINE linux `fs/` source tree (no build outputs), the
# reference's 2,384–2,468 entries (design §5.10). Named by TARX_SRC, else
# the first such tree among the venue's kernel sources (`/usr/src/linux*`
# is the installed headers tree on Arch — 143 entries, not the corpus;
# `scripts/kernel/build-host-kernel` leaves the full sources under
# `$SQZ_BUILD_ROOT/kernel/`).
TARX_SRC=${TARX_SRC:-}
TARX_ENTRIES_MIN=2000
TARX_ENTRIES_MAX=3000
# Extractions per arm, each into a fresh subdirectory (the reference's
# `--tarx-reps`). The wall is per extraction, the law a MEAN over the arm's
# extractions: one extraction per arm read 0.86×, 1.02×, 1.09× and 1.10×
# of S0 on one box in one day (2026-10-22) — a single 1.3 s extraction is
# noise at the gate's width, and B's first arm also pays its ring growths'
# drains (`rung-10-fuse-mainline-uapi.md` §10 / `ring_growth.zig`), which
# three extractions amortize. Three is the smallest count whose spread
# measured inside the gate.
TARX_REPS=${TARX_REPS:-3}
# Phase 2's concurrency.
TARX_PAR=${TARX_PAR:-$(( $(nproc) / 4 ))}
[ "$TARX_PAR" -ge 2 ] || TARX_PAR=2
# The phases a run performs, in this order (a subset is triage; the gate runs both).
TARX_PHASES=${TARX_PHASES:-"abba parallel"}
# The gate-2 law's two bounds (the reference's; `sym_rows_lib.sh`).
TARX_WALL_RATIO=1.10
TARX_VERBS_PER_ENTRY=0.05
# Wall bound per extraction (`timeout -s KILL`): a 2,400-entry extraction
# is seconds; past this it is a wedge to read.
TARX_TIMEOUT_S=${TARX_TIMEOUT_S:-900}
TARX_WIRE_KEYS=(joined_wire_verbs xv_cross_owner_steps_shipped xv_cross_owner_guard_rpcs slot_resolve_rpcs slot_ships dir_stripe_ships dlm_token_grants free_shipped_blocks)

tarx_corpus() {
  local cands=() c n built
  if [ -n "$TARX_SRC" ]; then cands=("$TARX_SRC")
  else for c in /usr/src/linux*/fs "${SQZ_BUILD_ROOT:-/build}"/kernel/linux-mainline*/fs "${SQZ_BUILD_ROOT:-/build}"/kernel/linux-*/fs; do [ -d "$c" ] && cands+=("$c"); done; fi
  local src=""
  for c in "${cands[@]}"; do
    n=$(find "$c" | wc -l)
    built=$(find "$c" \( -name '*.o' -o -name '*.cmd' -o -name '*.mod' \) -print -quit)
    if [ "$n" -ge "$TARX_ENTRIES_MIN" ] && [ "$n" -le "$TARX_ENTRIES_MAX" ] && [ -z "$built" ]; then src=$c; break; fi
    suite_log "corpus candidate $c: $n entries${built:+, build outputs ($built)} — not the row's corpus"
  done
  [ -n "$src" ] || suite_die "no corpus: a pristine linux fs/ tree with $TARX_ENTRIES_MIN–$TARX_ENTRIES_MAX entries (TARX_SRC=<linux>/fs names one)"
  TARX_SRC=$src
  TARBALL="$RESULTS/corpus.tar"
  tar -cf "$TARBALL" -C "$(dirname "$src")" "$(basename "$src")"
  TARX_ENTRIES=$(tar -tf "$TARBALL" | wc -l)
  # The oracle: one extraction on the venue's ext4 → the sha256 manifest of
  # every file + the listing of every entry's type, mode, path and link
  # target. An extracted tree is byte-exact when both agree.
  local ref="$RESULTS/corpus-ref"
  rm -rf "$ref"; mkdir -p "$ref"
  tar -xf "$TARBALL" -C "$ref"
  (cd "$ref" && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum) > "$RESULTS/manifest.sha"
  (cd "$ref" && find . -printf '%y %m %p %l\n' | LC_ALL=C sort) > "$RESULTS/manifest.tree"
  rm -rf "$ref"
  suite_log "corpus: $src — $TARX_ENTRIES entries, $(du -h "$TARBALL" | cut -f1) tarball, $(grep -c . "$RESULTS/manifest.sha") files in the manifest"
}

# An extracted tree against the oracle: 0 when byte-exact (files) and
# structurally identical (every entry's type, mode, path, link target).
tarx_verify() {   # tree label
  local t=$1 label=$2 rc=0
  (cd "$t" && sha256sum --quiet -c "$RESULTS/manifest.sha") > "$RESULTS/verify-$label.out" 2>&1 || rc=1
  (cd "$t" && find . -printf '%y %m %p %l\n' | LC_ALL=C sort) | diff - "$RESULTS/manifest.tree" > "$RESULTS/verify-$label.tree.diff" 2>&1 || rc=1
  if [ "$rc" != 0 ]; then
    suite_log "tree $label at $t differs from the corpus:"
    { grep -v ': OK$' "$RESULTS/verify-$label.out" | head -5; head -5 "$RESULTS/verify-$label.tree.diff"; } | cut -c1-200 | sed 's/^/    /' >&2
  fi
  return $rc
}

TARX_STOP=0
tarx_fail() {   # id message
  echo "$1" >> "$FAILURES"
  if grep -qxF "$1" "$RESULTS/ledger-ids.txt"; then suite_log "FAIL (adjudicated ledger row) $1: $2"
  else suite_log "FAIL $1: $2"; TARX_STOP=1; fi
}

# The background extractions of the current step (a stop kills them).
TARX_PIDS=()
tarx_cleanup() {
  local p
  for p in "${TARX_PIDS[@]}"; do kill -KILL "$p" 2>/dev/null || true; done
  TARX_PIDS=()
}

# ---- phase 1: the A-B-B-A row -------------------------------------------------

# One arm on mount `tag`: snapshots of both daemons, `mkdir` of the
# destination (the directory this mount created — under `/`, the ONE
# shipped step for B), TARX_REPS extractions each into a fresh
# subdirectory, snapshots again. The wall per extraction (s) in
# TARX_ARM_WALL (no subshell: a failure must reach TARX_STOP).
TARX_ARM_WALL=""
tarx_arm() {   # label tag
  local label=$1 tag=$2 mnt=${SUITE_MNT[$2]}
  local dest="$mnt/tarx-$label" r rc=0
  suite_stats_snapshot "$TAG_A" "$label-0" >/dev/null; suite_stats_snapshot "$TAG_B" "$label-0" >/dev/null
  local t0 t1
  t0=$(date +%s.%N)
  mkdir "$dest"
  for r in $(seq 0 $((TARX_REPS - 1))); do
    mkdir "$dest/r$r"
    timeout -s KILL "$TARX_TIMEOUT_S" tar -xf "$TARBALL" -C "$dest/r$r" > "$RESULTS/tar-$label-r$r.out" 2>&1 || { rc=$?; break; }
  done
  t1=$(date +%s.%N)
  suite_stats_snapshot "$TAG_A" "$label-1" >/dev/null; suite_stats_snapshot "$TAG_B" "$label-1" >/dev/null
  [ "$rc" = 0 ] || { head -5 "$RESULTS/tar-$label-r$r.out" | sed 's/^/    /' >&2; tarx_fail "abba/extract:$label" "tar -x on $tag exit $rc (137 = the $TARX_TIMEOUT_S s wall bound)"; }
  TARX_ARM_WALL=$(python3 -c "print(f'{($t1 - $t0) / $TARX_REPS:.3f}')")
}

# The arm's census off its snapshots: `<Σ wire verbs on B> <handovers A>
# <handovers B> <manager verbs A> <frames sent B> <renewals B>`.
tarx_census() {   # label
  local l=$1
  local ba="$RESULTS/stats-$TAG_A-$l-0.json" aa="$RESULTS/stats-$TAG_A-$l-1.json" bb="$RESULTS/stats-$TAG_B-$l-0.json" ab="$RESULTS/stats-$TAG_B-$l-1.json"
  echo "$(suite_stats_delta "$bb" "$ab" "${TARX_WIRE_KEYS[@]}") $(suite_stats_delta "$ba" "$aa" slot_handovers) $(suite_stats_delta "$bb" "$ab" slot_handovers) $(suite_stats_delta "$ba" "$aa" manager_verbs) $(suite_stats_delta "$bb" "$ab" cluster_wire_frames_sent) $(suite_stats_delta "$bb" "$ab" member_renewals 2>/dev/null)"
}

TAG_A=""; TAG_B=""
tarx_abba() {   # n
  local n=$1
  TAG_A="run$n-a"; TAG_B="run$n-b"
  suite_mount_pair "$TAG_A" "$TAG_B"
  local mnt_a=${SUITE_MNT[$TAG_A]} mnt_b=${SUITE_MNT[$TAG_B]}
  suite_log "run $n phase 1: A (manager) at $mnt_a, B (joined) at $mnt_b; A-B-B-A over $TARX_ENTRIES entries × $TARX_REPS extraction(s) per arm"
  local rows="$RESULTS/rows-run$n.tsv"
  printf 'arm\tmount\twall_s_per_rep\tentries_per_s\twire_verbs_B\tverbs_per_entry\thandovers_A\thandovers_B\tmanager_verbs_A\tframes_sent_B\trenewals_B\n' > "$rows"
  declare -A wall
  local label side tag w wire ha hb mv fr rn vpe eps
  for label in a1 b1 b2 a2; do
    side=${label:0:1}
    tag=$([ "$side" = a ] && echo "$TAG_A" || echo "$TAG_B")
    tarx_arm "$label" "$tag"; w=$TARX_ARM_WALL; wall[$label]=$w
    eps=$(python3 -c "print(f'{$TARX_ENTRIES / max(float(\"$w\"), 0.001):.0f}')")
    read -r wire ha hb mv fr rn <<< "$(tarx_census "$label")"
    vpe=$(python3 -c "print(f'{$wire / ($TARX_ENTRIES * $TARX_REPS):.4f}')")
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$label" "$side" "$w" "$eps" "$wire" "$vpe" "$ha" "$hb" "$mv" "$fr" "${rn:-0}" >> "$rows"
    suite_log "arm $label on $side ($tag): $w s per extraction ($eps entries/s); B's wire verbs $wire ($vpe per entry), handovers A $ha B $hb, manager verbs A +$mv, frames sent B +$fr, renewals B +${rn:-0}"
    # THE ENGAGEMENT LAW on B's arms: verbs/entry < 0.05, no handover on
    # either side. (A's arms: recorded.)
    if [ "$side" = b ]; then
      python3 -c "import sys; sys.exit(0 if $wire / ($TARX_ENTRIES * $TARX_REPS) < $TARX_VERBS_PER_ENTRY else 1)" \
        || tarx_fail "abba/verbs:$label" "$vpe wire verbs per entry on B (bound $TARX_VERBS_PER_ENTRY; keys: ${TARX_WIRE_KEYS[*]}) — the joiner did not extract into its OWN slot tree"
    fi
    [ "$ha" = 0 ] && [ "$hb" = 0 ] || tarx_fail "abba/handovers:$label" "slot_handovers moved (A $ha, B $hb) — a handover inside a local extraction"
    # ACKED WRITES PRESENT: the tree through BOTH mounts, after the
    # snapshots (the other member's read-back takes tokens and promotes the
    # writer's residents — the untimed, unjudged part of the arm).
    local r exact=0
    for r in $(seq 0 $((TARX_REPS - 1))); do
      tarx_verify "$mnt_a/tarx-$label/r$r" "$label-r$r@a" && exact=$((exact + 1)) || tarx_fail "abba/tree:$label@a" "arm $label's tree r$r read through A differs from the corpus"
      tarx_verify "$mnt_b/tarx-$label/r$r" "$label-r$r@b" && exact=$((exact + 1)) || tarx_fail "abba/tree:$label@b" "arm $label's tree r$r read through B differs from the corpus"
    done
    suite_log "arm $label: $exact of $((2 * TARX_REPS)) tree reads byte-exact against the corpus ($(grep -c . "$RESULTS/manifest.sha") files, $TARX_ENTRIES entries each; through A and through B)"
    [ "$TARX_STOP" = 0 ] || break
  done
  if [ "$TARX_STOP" = 0 ]; then
    # THE WALL LAW: B's mean ≤ 1.10 × S0 (A's mean); both orders beside.
    local verdict
    verdict=$(python3 - "${wall[b1]}" "${wall[b2]}" "${wall[a1]}" "${wall[a2]}" "$TARX_WALL_RATIO" <<'PY'
import sys
b = (float(sys.argv[1]) + float(sys.argv[2])) / 2
a = (float(sys.argv[3]) + float(sys.argv[4])) / 2
r = b / a
print(f"gate 2: joined writer B {b:.3f} s vs manager-local S0 {a:.3f} s -> {r:.3f}x of S0 (gate <= {sys.argv[5]}x): {'MET' if r <= float(sys.argv[5]) else 'MISS'}")
print(f"both orders: a1 {float(sys.argv[3]):.3f} b1 {float(sys.argv[1]):.3f} | b2 {float(sys.argv[2]):.3f} a2 {float(sys.argv[4]):.3f}")
PY
)
    printf '%s\n' "$verdict" | tee "$RESULTS/verdict-run$n.txt" | sed 's/^/    /' >&2
    printf '%s\n' "$verdict" | grep -q 'MET$' || tarx_fail "abba/wall" "$(printf '%s\n' "$verdict" | head -1)"
  fi
  # The mounts healthy after, then B leaves first, the manager last.
  suite_judge_mount "$TAG_A" "abba-after" || tarx_fail "abba/mount:a" "A is not healthy after the row"
  suite_judge_mount "$TAG_B" "abba-after" || tarx_fail "abba/mount:b" "B is not healthy after the row"
  suite_umount "$TAG_B"
  suite_umount "$TAG_A"
  suite_daemon_alerts "$TAG_A" || tarx_fail "abba/tripwire:a" "a tripwire fired in A (or A was OOM-killed)"
  suite_daemon_alerts "$TAG_B" || tarx_fail "abba/tripwire:b" "a tripwire fired in B (or B was OOM-killed)"
  suite_fsck "$TAG_A" || tarx_fail "abba/fsck" "fsck --offline --scrub found something after the A-B-B-A row"
  unset SQUEEZEFS_MW_BIND
}

# ---- phase 2: the parallel single-mount form ----------------------------------

tarx_parallel() {   # n
  local n=$1 tag="par$n"
  suite_mount "$tag"
  local mnt=${SUITE_MNT[$tag]} base k rc t0 t1
  base="$mnt/par"; mkdir "$base"
  suite_stats_snapshot "$tag" before >/dev/null
  suite_log "run $n phase 2: $TARX_PAR concurrent extractions of $TARX_ENTRIES entries into $base/x1..x$TARX_PAR on one mount"
  t0=$(date +%s.%N)
  TARX_PIDS=()
  for k in $(seq 1 "$TARX_PAR"); do
    mkdir "$base/x$k"
    timeout -s KILL "$TARX_TIMEOUT_S" tar -xf "$TARBALL" -C "$base/x$k" > "$RESULTS/tar-par$n-x$k.out" 2>&1 &
    TARX_PIDS+=($!)
  done
  local fails=0
  for k in $(seq 1 "$TARX_PAR"); do
    rc=0; wait "${TARX_PIDS[$((k - 1))]}" || rc=$?
    [ "$rc" = 0 ] || { fails=$((fails + 1)); head -3 "$RESULTS/tar-par$n-x$k.out" | sed 's/^/    /' >&2; tarx_fail "parallel/extract:$k" "tar -x #$k exit $rc"; }
  done
  TARX_PIDS=()
  t1=$(date +%s.%N)
  local w; w=$(python3 -c "print(f'{$t1 - $t0:.3f}')")
  suite_log "run $n phase 2: $TARX_PAR extractions in $w s wall ($(python3 -c "print(f'{$TARX_PAR * $TARX_ENTRIES / ($t1 - $t0):.0f}')") entries/s aggregate), $fails failed"
  printf 'parallel\t%s\t%s\t%s\n' "$tag" "$w" "$TARX_PAR" >> "$RESULTS/rows-run$n.tsv"
  local exact=0
  for k in $(seq 1 "$TARX_PAR"); do
    tarx_verify "$base/x$k" "par$n-x$k" && exact=$((exact + 1)) || tarx_fail "parallel/tree:$k" "tree #$k differs from the corpus"
  done
  suite_log "run $n phase 2: $exact of $TARX_PAR trees byte-exact against the corpus"
  suite_judge_mount "$tag" after || tarx_fail "parallel/mount" "the mount is not healthy after the parallel extractions"
  suite_umount "$tag"
  suite_daemon_alerts "$tag" || tarx_fail "parallel/tripwire" "a tripwire fired (or the daemon was OOM-killed)"
  suite_fsck "$tag" || tarx_fail "parallel/fsck" "fsck --offline --scrub found something after the parallel extractions"
}

tarx_run() {   # n
  local n=$1 phase
  : > "$RESULTS/rows-run$n.tsv"
  for phase in $TARX_PHASES; do
    case $phase in
      abba) tarx_abba "$n" ;;
      parallel) tarx_parallel "$n" ;;
      *) suite_die "unknown phase '$phase' (TARX_PHASES: abba parallel)" ;;
    esac
    if [ "$TARX_STOP" = 1 ]; then
      suite_log "run $n: a check failed in phase '$phase' and is no ledger row — stopping the run here (fail early; the later phases are not run)"
      SUITE_JUDGE_NO_STALE=1
      return 1
    fi
  done
  return 0
}

main() {
  SUITE_CLEANUP_HOOK=tarx_cleanup
  suite_prepare tarx
  tarx_corpus
  suite_ledger_rows tarx | cut -f1 > "$RESULTS/ledger-ids.txt"
  local n verdict=0
  for n in $(seq 1 "$SUITE_RUNS"); do
    suite_run_count "$n"
    FAILURES="$RESULTS/run$n.failures"; : > "$FAILURES"
    TARX_STOP=0
    local run_rc=0 jrc=0
    tarx_run "$n" || run_rc=$?
    suite_judge tarx "$FAILURES" || jrc=$?
    if [ "$run_rc" != 0 ] || [ "$jrc" != 0 ]; then
      suite_log "run $n did NOT pass (run rc $run_rc, ledger rc $jrc): the count aborts; fix, then restart from run 1"
      verdict=1; break
    fi
    suite_log "run $n PASSED (phases: $TARX_PHASES — every law met, every extracted tree byte-exact, the mounts healthy, fsck clean, no tripwire)"
  done
  [ "$verdict" = 0 ] && suite_log "tarx ($TARX_ENTRIES entries; phases: $TARX_PHASES; A-B-B-A ×$TARX_REPS, $TARX_PAR parallel): $SUITE_RUNS consecutive clean run(s) on $("$SQZ_BIN" --version)"
  exit $verdict
}

main "$@"
