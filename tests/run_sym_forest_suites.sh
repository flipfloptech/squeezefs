#!/usr/bin/env bash
# The slot-tree forest's parametrized gate (docs/design-symmetric-metadata.md
# PR 1: "every existing KV contract green on the forest — suites
# parametrized over the stamp").
#
# The forest (incompat bit 17) is stamped ONLY through the test seam
# `SQUEEZEFS_TEST_STAMP_SYMMETRIC=1`, which the image builder reads at
# format time — so the same test binary formats FLAT volumes without it
# and FOREST volumes with it. This script runs the pre-forest KV suites
# BOTH ways (`flat`, then `stamped`) and fails on the first red leg: a
# contract that holds on the shipped layout and not on the forest is
# exactly what review round 1 of PR 1 found by hand (Issues 1-3).
#
# The suites listed drive the KV backend directly (format → mount →
# commit → checkpoint → replay), the tree and node layers, the durable
# block-reference ledger, the block-map tree, the leaf merge, fsck's
# detection AND repair classes, the crash matrix, the coherent reader,
# the per-volume coordinator and the scale/liveness rows — every
# KV-contract suite the harness sweep touched (review round 3, Issue 20:
# "every existing KV contract" means the list). `sym_forest_tests` itself
# sets the seam per test and runs once; `sym_appender_tests` (PR 2 — the
# appender region) formats its region fixtures under the seam itself and
# rides the list so its flat-layout contracts (no appender set, the
# untouched sector 0 and ring) run on both legs too; `sym_manager_tests`
# and `sym_fence_tests` (PR 3 — the manager lease) ride it the same way:
# their stamped fixtures set the seam themselves, and their flat pins (no
# manager, no grant, the shipped rtype-1 reservation, the REGCTL-sized
# report read on every layout) run on both legs.
# `kv_smo_crash_completeness_tests` (the SMO replay-currency crash matrix)
# joined the list in PR 3's round 2: three of its pins assumed the flat
# layout — a per-kind INODES tree armed by id, racing inos filtered by the
# legacy key, an exact parked count from a depth-1 INODES pair — and read
# red under the seam on the base with nobody running them stamped. They
# now resolve their tree through the ONE locator, arm a slot tree by
# SLOT, and state the §2-A law layout-blind (the live root's replayed
# free dropped, every other in-window free parked, every mounted root
# keeping its bit; a forest writer's `open` joins its appender regions,
# and that join's barriered cycle IS the first post-mount durable
# checkpoint, so the parked window is read off the counters there).
# `sym_slot_transfer_tests` (PR 4 — slot leases) rides the list the same
# way as PR 2/3's suites: its armed fixtures stamp under the seam
# themselves (`SQUEEZEFS_SYMMETRIC_META=1` on a bit-17 volume), and its
# flat pins (the `=1` refusal on a flat volume, the `=0` dark forest with
# no plane and `dlm_mode = solo`) run on both legs.
# `rename_lock_set_tests` (PR 4 review round 2, Issue 1 — the shipped
# `rename` lock hole, layout-independent) rides both legs too: the pin
# forces a rename and a layout publish of one inode into ONE conveyor
# batch, which the fixed lock set must make impossible on either layout.
#
# WALL TIME IS A ROW (review round 4, Issue 26): each suite runs under
# its own timer per leg, and the summary prints `flat`, `stamped` and the
# `stamped/flat` ratio per suite. A ratio at or above `SQZ_SYM_RATIO_NOTE`
# (default 2.0) is printed as a NOTE — never a failure: no bound is
# derived for it, and a mixed-kind tree legitimately costs more on some
# shapes (one leaf per file's records instead of three per-kind leaves);
# a suite under 1 s flat is not rated (its ratio is scheduler noise).
# What the row catches is a suite that PASSES both ways while idling on
# one layout — Issue 26's `kv_scale_tests` read 90 s stamped against 25 s
# flat (3.5×) for weeks with every assertion green: a shutdown signalled
# while the checkpoint task was inside its maintenance pass slept to the
# cadence deadline. A green leg says the contracts hold; the ratio says
# whether the forest pays for them in TIME, and that is the reviewer's
# question, not the assertion's.
#
# Usage:
#   tests/run_sym_forest_suites.sh                 # both legs, the default list
#   tests/run_sym_forest_suites.sh stamped         # one leg
#   SQZ_SYM_SUITES="kv_backend_tests" tests/run_sym_forest_suites.sh
#   SQZ_SYM_RATIO_NOTE=1.5 tests/run_sym_forest_suites.sh
set -euo pipefail
cd "$(dirname "$0")/.."

DEFAULT_SUITES=(
  kv_tree_tests
  kv_node_tests
  kv_backend_tests
  kv_journal_tests
  kv_partitioned_append_tests
  kv_leaf_merge_tests
  kv_node_cache_coherence_tests
  kvmap_tree_tests
  kv_scale_tests
  durable_block_refs_tests
  fsck_tests
  fsck_c9_tests
  fsck_c10_tests
  fsck_c12_tests
  fsck_repair_tests
  crash_contract_tests
  crash_kill_tests
  writer_scoped_staging_tests
  readonly_mount_tests
  meta_slot_migration_tests
  pv_coordinator_tests
  kv_smo_crash_completeness_tests
  sym_appender_tests
  sym_manager_tests
  sym_fence_tests
  sym_slot_transfer_tests
  rename_lock_set_tests
)
read -r -a SUITES <<<"${SQZ_SYM_SUITES:-${DEFAULT_SUITES[*]}}"
RATIO_NOTE="${SQZ_SYM_RATIO_NOTE:-2.0}"

LEGS=(flat stamped)
if [[ $# -ge 1 ]]; then
  LEGS=("$1")
fi

args=()
for s in "${SUITES[@]}"; do
  args+=(--test "$s")
done

# One build for both legs.
cargo test --all-features --no-run "${args[@]}" >/dev/null

# Per-suite wall seconds per leg: WALL[leg/suite].
declare -A WALL
now_ms() { date +%s%3N; }

for leg in "${LEGS[@]}"; do
  case "$leg" in
    flat) unset SQUEEZEFS_TEST_STAMP_SYMMETRIC ;;
    stamped) export SQUEEZEFS_TEST_STAMP_SYMMETRIC=1 ;;
    *) echo "unknown leg '$leg' (flat|stamped)" >&2; exit 2 ;;
  esac
  echo "=== sym-forest suites: $leg leg (${#SUITES[@]} suites) ==="
  leg_rc=0
  for s in "${SUITES[@]}"; do
    t0=$(now_ms)
    if cargo test --all-features --test "$s" -- --test-threads=1; then
      :
    else
      leg_rc=1
    fi
    t1=$(now_ms)
    WALL["$leg/$s"]=$(( t1 - t0 ))
    printf '=== %s %-32s wall %6.1f s\n' "$leg" "$s" "$(awk "BEGIN{print (${WALL["$leg/$s"]})/1000}")"
    if [[ $leg_rc -ne 0 ]]; then
      echo "=== $leg leg: FAILED at $s ===" >&2
      exit 1
    fi
  done
  echo "=== $leg leg: PASS ==="
done

if [[ ${#LEGS[@]} -eq 2 ]]; then
  echo "=== sym-forest suites: wall time per suite (s) — stamped/flat ratio ≥ ${RATIO_NOTE} is a NOTE ==="
  printf '%-32s %9s %9s %7s\n' suite flat stamped ratio
  for s in "${SUITES[@]}"; do
    f=${WALL["flat/$s"]}
    st=${WALL["stamped/$s"]}
    awk -v s="$s" -v f="$f" -v st="$st" -v note="$RATIO_NOTE" 'BEGIN{
      r = (f > 0) ? st / f : 0;
      tag = (r >= note && f >= 1000) ? "  NOTE: stamped/flat ratio" : "";
      printf "%-32s %9.1f %9.1f %7.2f%s\n", s, f/1000, st/1000, r, tag;
    }'
  done
fi
echo "sym-forest suites: PASS (${LEGS[*]})"
