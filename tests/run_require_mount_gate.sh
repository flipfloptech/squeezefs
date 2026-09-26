#!/usr/bin/env bash
# The require-mount gate (spec §11 TEST-2) — the release-gate leg that
# makes "all green" mean "the live-mount surface actually ran".
#
# Seventeen test binaries drive a real FUSE-over-io_uring mount. Before this
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
#   tests/run_require_mount_gate.sh            # the seventeen mount suites
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
  commit_wake_loss_tests
  corpse_sweep_tests
  dismount_staged_residue_tests
  format_guard_tests
  fsync_promote_staged_tests
  inline_raise_tests
  mount_owner_override_tests
  multi_queue_tests
  overlay_growth_merge_tests
  pack_compaction_tests
  pack_tenant_ops_tests
  packed_mapping_wire_tests
  phantom_backend0_tests
  posix_mount_semantics_tests
  small_file_packing_tests
  statfs_tests
  sym_convert_fuse_tests
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

# The memlock PREFLIGHT (PR 14 — the fresh-install finding): a mounted
# daemon pins one kmbuf ring per FUSE queue (one queue per possible CPU) of
# `q_depth × buf_size` — 32 × 1 MiB at the shipped geometry — and an
# unprivileged daemon's pins are bounded by RLIMIT_MEMLOCK (8 MiB on most
# distributions). Every suite here would run into that wall and fail at
# its first mount; refuse to START instead, naming the derived need and
# the fix. Root (and CAP_IPC_LOCK) is exempt.
memlock_preflight() {
  if [[ "$(id -u)" == "0" ]]; then
    echo "   memlock:          root (RLIMIT_MEMLOCK does not bind)"
    return 0
  fi
  local soft; soft="$(ulimit -l)"
  if [[ "$soft" == "unlimited" ]]; then
    echo "   memlock:          unlimited"
    return 0
  fi
  # ulimit -l reports KiB. The need is DERIVED: queues (possible CPUs) ×
  # entries (the shipped q_depth 32) × buf_size (1 MiB — the transport's
  # max_write payload) — the same arithmetic the daemon pins.
  local cpus depth=32 buf_mib=1 need_kib
  cpus="$(getconf _NPROCESSORS_CONF 2>/dev/null || nproc)"
  need_kib=$(( cpus * depth * buf_mib * 1024 ))
  if (( soft < need_kib )); then
    cat >&2 <<EOF_MEMLOCK
!! require-mount gate refuses to start: RLIMIT_MEMLOCK soft limit is ${soft} KiB and a
   mounted daemon on this box pins ${need_kib} KiB of kmbuf rings (${cpus} queues × ${depth}
   entries × ${buf_mib} MiB) — IORING_REGISTER_KMBUF_RING would refuse ENOMEM at the first
   queue and every live-mount suite would fail at its first mount (the daemon refuses loud,
   naming this limit). Raise it in the shell that runs the gate:
     sudo -n prlimit --pid \$\$ --memlock=unlimited:unlimited      # this shell, now
     ulimit -l unlimited                                           # if the hard limit allows
   or persistently (NEW sessions): '<user> - memlock unlimited' in /etc/security/limits.d/,
   DefaultLimitMEMLOCK=infinity in /etc/systemd/{system,user}.conf.d/ — docs/operations.md
   §Prerequisites.
EOF_MEMLOCK
    return 1
  fi
  echo "   memlock:          ${soft} KiB (need ${need_kib} KiB for ${cpus} queues × ${depth} × ${buf_mib} MiB)"
}
memlock_preflight || exit 2

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
