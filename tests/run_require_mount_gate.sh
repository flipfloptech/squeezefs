#!/bin/bash
# The require-mount gate (spec §11 TEST-2) — the release-gate leg that
# makes "all green" mean "the live-mount surface actually ran".
#
# Twelve test binaries drive a real FUSE-over-io_uring mount. Before this
# gate they self-skipped and reported PASS on a box without /dev/fuse,
# `fuse.enable_uring`, or fusermount3 — so `cargo test --all-features`
# could be green with the product's core mechanism entirely unexecuted,
# and libtest swallowed the skip notices without --nocapture.
#
# This script runs those binaries with SQUEEZEFS_TEST_REQUIRE_MOUNT=1,
# which turns every mount-class skip into a hard FAILURE, and collects a
# machine-readable skip ledger (one JSON object per line) the harness can
# diff. Non-mount classes (hardware, sudo, capability, toolchain, opt-in)
# stay skips here — pass SQUEEZEFS_TEST_REQUIRE_ALL=1 to promote them too.
#
# Usage:
#   tests/run_require_mount_gate.sh            # the twelve mount suites
#   tests/run_require_mount_gate.sh --ledger-only   # just print the ledger
#   SQZ_REQUIRE_GATE_TESTS="statfs_tests" tests/run_require_mount_gate.sh
set -euo pipefail
cd "$(dirname "$0")/.."

LEDGER="${SQUEEZEFS_TEST_SKIP_LEDGER:-$PWD/target/skip-ledger.jsonl}"
mkdir -p "$(dirname "$LEDGER")"

# The mount-gated surface. Keep in sync with the MOUNT_GATED list in
# tests/skip_ledger_tests.rs (a cargo test pins that list against the
# tree, so a new mount suite cannot quietly stay out of this gate).
DEFAULT_TESTS=(
  cache_path_policy_tests
  cli_clients_df_tests
  format_guard_tests
  mount_owner_override_tests
  multi_queue_tests
  phantom_backend0_tests
  posix_mount_semantics_tests
  statfs_tests
  transport_concurrency_tests
  transport_geometry_tests
  transport_ingress_tests
  transport_lease_overlong_tests
)
read -r -a TESTS <<<"${SQZ_REQUIRE_GATE_TESTS:-${DEFAULT_TESTS[*]}}"

summarize() {
  if [[ ! -s "$LEDGER" ]]; then
    echo "skip ledger: EMPTY (no test declined to run)"
    return 0
  fi
  echo "skip ledger: $LEDGER"
  echo "--- by class ---"
  sed -n 's/.*"class":"\([^"]*\)".*/\1/p' "$LEDGER" | sort | uniq -c | sort -rn
  echo "--- records ---"
  cat "$LEDGER"
}

if [[ "${1:-}" == "--ledger-only" ]]; then
  summarize
  exit 0
fi

: >"$LEDGER"
export SQUEEZEFS_TEST_SKIP_LEDGER="$LEDGER"
export SQUEEZEFS_TEST_REQUIRE_MOUNT=1

echo "== require-mount gate: ${#TESTS[@]} live-mount suites =="
echo "   /dev/fuse:        $([[ -e /dev/fuse ]] && echo present || echo MISSING)"
echo "   enable_uring:     $(cat /sys/module/fuse/parameters/enable_uring 2>/dev/null || echo MISSING)"
echo "   fusermount3:      $(command -v fusermount3 || echo MISSING)"
echo "   fusectl:          $([[ -d /sys/fs/fuse/connections ]] && echo mounted || echo MISSING)"

rc=0
for t in "${TESTS[@]}"; do
  echo "-- cargo test --test $t"
  if ! cargo test --all-features --test "$t" -- --test-threads=1; then
    echo "!! FAILED: $t" >&2
    rc=1
  fi
done

echo
summarize

# A mount-class record can only exist here if a suite declined to mount —
# which SQUEEZEFS_TEST_REQUIRE_MOUNT already turned into a test failure.
# Assert it anyway: a future gate that forgets to export the var would
# otherwise silently regress to the phantom-green posture.
if grep -q '"class":"mount"' "$LEDGER" 2>/dev/null; then
  echo "!! mount-class skips present under SQUEEZEFS_TEST_REQUIRE_MOUNT=1" >&2
  rc=1
fi

if [[ $rc -eq 0 ]]; then
  echo "== require-mount gate: PASS (the live-mount surface executed) =="
else
  echo "== require-mount gate: FAIL ==" >&2
fi
exit $rc
