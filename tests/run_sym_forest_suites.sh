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
# detection AND repair classes, the crash matrix, the coherent reader and
# the per-volume coordinator — every KV-contract suite the harness sweep
# touched (review round 3, Issue 20: "every existing KV contract" means
# the list). `sym_forest_tests` itself sets the seam per test and runs
# once.
#
# Usage:
#   tests/run_sym_forest_suites.sh                 # both legs, the default list
#   tests/run_sym_forest_suites.sh stamped         # one leg
#   SQZ_SYM_SUITES="kv_backend_tests" tests/run_sym_forest_suites.sh
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
)
read -r -a SUITES <<<"${SQZ_SYM_SUITES:-${DEFAULT_SUITES[*]}}"

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

for leg in "${LEGS[@]}"; do
  case "$leg" in
    flat) unset SQUEEZEFS_TEST_STAMP_SYMMETRIC ;;
    stamped) export SQUEEZEFS_TEST_STAMP_SYMMETRIC=1 ;;
    *) echo "unknown leg '$leg' (flat|stamped)" >&2; exit 2 ;;
  esac
  echo "=== sym-forest suites: $leg leg (${#SUITES[@]} suites) ==="
  cargo test --all-features --no-fail-fast "${args[@]}" -- --test-threads=1
  echo "=== $leg leg: PASS ==="
done
echo "sym-forest suites: PASS (${LEGS[*]})"
