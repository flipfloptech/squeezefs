#!/usr/bin/env bash
# The zc-capability gate (TEST-2's capability half) — the root-run leg
# that makes "all green" mean "the zero-copy kernel surface actually ran".
#
# The fuse zero-copy suites arm FUSE_URING_ZERO_COPY, which the kernel
# gates on capable(CAP_SYS_ADMIN) — so an unprivileged `cargo test`
# self-skips them (ledgered, class=capability) and a box that HAS the
# sqz kernel can sit permanently green with the zc write path entirely
# unexecuted. The mount gate (`tests/run_require_mount_gate.sh`)
# deliberately leaves non-mount classes as skips; this script is the
# packaged consumer of `SQUEEZEFS_TEST_REQUIRE_CAPABILITY=1`: run it as
# root on an sqz-kernel box and every capability-class decline is a
# hard FAILURE instead of a phantom-green skip.
#
# Venue: the sqz custom kernel series (docker/kernel-sqz/) + root. The
# kernel-name check below is informational only — the ARMING probe is
# authoritative, and under REQUIRE_CAPABILITY a venue that cannot arm
# fails loudly, which is exactly what invoking this gate asks for.
#
# Usage:
#   sudo tests/run_zc_capability_gate.sh                # the capability suites
#   sudo tests/run_zc_capability_gate.sh --ledger-only  # just print the ledger
#
# Keep the SUITES list in sync with
# `the_capability_gated_suites_all_ride_the_zc_capability_gate` in
# tests/skip_ledger_tests.rs — the pin auto-discovers every suite that
# declares SkipClass::Capability and fails the cargo gate until this
# script names it.
set -euo pipefail
cd "$(dirname "$0")/.."
REPO_DIR="$PWD"
RUNUSER="${SUDO_USER:-$(id -un)}"

# The capability-gated integration suites (whole binaries, the
# require-mount-gate pattern). bench_tests carries the zc/kmbuf bench
# rows; the two zc suites are the FUSE_URING_ZERO_COPY contract sets.
SUITES=(
  fuse_zc_write_fusion_tests
  zc_bridge_cqe_wedge_tests
  # The zc direct-leg bridge decomposition (R-3): the per-op
  # bridge_sent → … → block_fetched chain + the exact-sum family on
  # the real READ_FIXED(device → slot) bridge.
  zc_bridge_phase_tests
  bench_tests
  # Live WERO-semantics leg (fix/wero-rtype): root + kernel nvmet +
  # nvme-cli — the sqz box has all three, so REQUIRE_CAPABILITY turns
  # its decline into a failure here.
  wero_rtype_tests
  # PR 13i's direct-posture contracts (the O_DIRECT metadata path, the
  # ring's sector-pad law): they decline where the scratch filesystem
  # refuses O_DIRECT — this kernel's tmpfs serves it, so the decline is a
  # failure here (review round 1, Issue 7a).
  meta_io_direct_tests
)

LEDGER="${SQUEEZEFS_TEST_SKIP_LEDGER:-$PWD/target/skip-ledger-capability.jsonl}"
mkdir -p "$(dirname "$LEDGER")"

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

if [[ "$(id -u)" -ne 0 ]]; then
  echo "must run as root: the kernel arms FUSE_URING_ZERO_COPY only for" >&2
  echo "capable(CAP_SYS_ADMIN) — unprivileged runs are exactly the" >&2
  echo "phantom-green posture this gate exists to kill." >&2
  exit 1
fi

echo "== zc-capability gate: ${#SUITES[@]} suites + the zcrx lib probes =="
echo "   kernel:           $(uname -r)"
echo "   /dev/fuse:        $([[ -e /dev/fuse ]] && echo present || echo MISSING)"
echo "   enable_uring:     $(cat /sys/module/fuse/parameters/enable_uring 2>/dev/null || echo MISSING)"
case "$(uname -r)" in
*sqz*) : ;;
*)
  # Advisory only: the arming probe decides. A kernel built with the sqz
  # patch series under another name (a cachyos 7.1.x with the 7.1 track,
  # for one) arms and passes this gate; a stock kernel refuses the zc arm
  # and, under REQUIRE_CAPABILITY, fails loudly.
  echo "   NOTE: kernel name does not say 'sqz' — the zc arm decides:" >&2
  echo "   a stock kernel refuses it and this gate then fails loudly." >&2
  ;;
esac

# The unprivileged build shell: `$BASH` (the bash running this script),
# never the FHS literal `/bin/bash`, which does not exist on NixOS (the
# run_fstests.sh discipline); the caller's PATH is carried into the user
# shell so `cargo` resolves the same way it did for the invoking user —
# `su` resets PATH otherwise, and the 1.2.1 local leg died at "Cannot
# execute /bin/bash" before a single test ran.
run_as_user() {
  if [[ "$RUNUSER" != "root" ]] && id "$RUNUSER" &>/dev/null; then
    su -s "$BASH" "$RUNUSER" -c "export PATH='$PATH'; cd '$REPO_DIR' && $*"
  else
    "$BASH" -c "cd '$REPO_DIR' && $*"
  fi
}

# Build UNPRIVILEGED (root-owned target artifacts break later dev
# builds — the run_preload_gate.sh run_as_user discipline), then run
# the produced binaries as root.
echo "-- building test binaries as $RUNUSER"
for t in "${SUITES[@]}"; do
  run_as_user "cargo test --test $t --no-run" >/dev/null
done
run_as_user "cargo test --lib --no-run" >/dev/null

newest_bin() {
  # Newest matching executable in deps/ (exclude dep-info files).
  ls -t "$REPO_DIR"/target/debug/deps/"$1"-* 2>/dev/null | grep -v '\.d$' | head -1
}

: >"$LEDGER"
export SQUEEZEFS_TEST_SKIP_LEDGER="$LEDGER"
export SQUEEZEFS_TEST_REQUIRE_CAPABILITY=1

rc=0
for t in "${SUITES[@]}"; do
  bin="$(newest_bin "$t")"
  if [[ -z "$bin" ]]; then
    echo "!! no built binary for $t" >&2
    rc=1
    continue
  fi
  echo "-- $t ($bin)"
  if ! "$bin" --test-threads=1; then
    echo "!! FAILED: $t" >&2
    rc=1
  fi
done

# The zcrx lane's lib-test capability probes (io_uring ring-flag gate,
# src/zcrx_lane/uring_zcrx.rs). The `squeezefs-*` deps prefix matches
# the lib unittest binary AND the CLI bin target — screen candidates
# with libtest's `--list` (the CLI errors on it), then run the filter
# and require that at least one binary actually executed tests: a
# filter that runs zero tests everywhere is this leg silently
# disarming.
zcrx_ran=0
for bin in $(ls -t "$REPO_DIR"/target/debug/deps/squeezefs-* 2>/dev/null | grep -v '\.d$'); do
  if ! "$bin" --list zcrx_lane >/dev/null 2>&1; then
    continue # not a libtest binary (the CLI bin target)
  fi
  echo "-- zcrx lib probes ($bin)"
  out="$("$bin" zcrx_lane --test-threads=1 2>&1)" || rc=1
  echo "$out" | tail -3
  if echo "$out" | grep -qE '^running [1-9][0-9]* tests?'; then
    zcrx_ran=1
  fi
done
if [[ $zcrx_ran -ne 1 ]]; then
  echo "!! the zcrx lib-probe leg executed zero tests (filter rot?)" >&2
  rc=1
fi

echo
summarize

# A capability-class record can only exist here if a suite declined —
# which REQUIRE_CAPABILITY already turned into a failure. Assert it
# anyway (the mount gate's belt): a future edit that drops the export
# would otherwise regress silently to the phantom-green posture.
if grep -q '"class":"capability"' "$LEDGER" 2>/dev/null; then
  echo "!! capability-class skips present under SQUEEZEFS_TEST_REQUIRE_CAPABILITY=1" >&2
  rc=1
fi

if [[ $rc -eq 0 ]]; then
  echo "== zc-capability gate: PASS (the zero-copy surface executed) =="
else
  echo "== zc-capability gate: FAIL ==" >&2
fi
exit $rc
