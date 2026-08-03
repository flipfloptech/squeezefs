#!/bin/bash
set -euo pipefail

# Squeezefs integration runner for the pjdfstest POSIX conformance suite
# (github.com/pjd/pjdfstest). MUST be run as root.
#
# Release-gate tier (AGENTS.md "Test tiering", user ruling 2026-07-20):
# pjdfstests + full LTP + full fstests must ALL pass before any release tag.
# Every failure found here rides the repro-port mandate — the fix lands with
# a cargo test reproducing it, or (kernel-interface-only scenarios) a
# documented exception.
#
# Usage:
#   sudo tests/run_pjdfstests.sh                 # full suite (prove -r)
#   sudo tests/run_pjdfstests.sh chmod/12.t      # single test (fix loop)
#   sudo tests/run_pjdfstests.sh chmod           # one directory
#
# Conventions follow tests/run_fstests.sh: self-contained, idempotent,
# cleanup scoped to THIS runner's devices only (never `killall squeezefs` —
# other suites may be running against their own volumes on the same box).

if [ "$(id -u)" -ne 0 ]; then
    echo "ERROR: This script must run as root (sudo $0)." >&2
    exit 1
fi

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
RUNUSER="${SUDO_USER:-root}"

MOUNT_DIR="${MOUNT_DIR:-/mnt/squeezefs_pjdfstest}"
STAGING_DIR="${STAGING_DIR:-/tmp/squeezefs_pjdfstest_staging}"
META_DEV="${META_DEV:-/dev/shm/squeezefs_pjdfstest_meta}"
DATA_DEV="${DATA_DEV:-/dev/shm/squeezefs_pjdfstest_data}"
META_SIZE="${SQUEEZEFS_PJDFSTEST_META_SIZE:-1G}"
DATA_SIZE="${SQUEEZEFS_PJDFSTEST_DATA_SIZE:-4G}"
LOG_FILE="/tmp/squeezefs_pjdfstest.log"
PROVE_OUT="/tmp/squeezefs_pjdfstest_prove.log"

PJDFSTEST_DIR="${PJDFSTEST_DIR:-/tmp/pjdfstest}"
PJDFSTEST_REPO="https://github.com/pjd/pjdfstest.git"

# ---------------------------------------------------------------------------
# EXPECTED-FAIL TABLE — documented BY-DESIGN semantics ONLY (same class as the
# fstests generic/003 (noatime by design) / generic/213 (thin provisioning)
# adjudications; see the expected-result table in tests/run_fstests.sh).
#
# Format: "<test path relative to tests/>:<space-separated failed subtest
# numbers>". A run is GREEN only when the observed failure set matches this
# table EXACTLY:
#   - any failure NOT in the table            => unexpected  => exit 1
#   - any entry here that does NOT fail       => stale table => exit 1
#     (a fixed test must be REMOVED from this table with its fix)
# Entries require a documented adjudication (runner comment + the program
# closing report). An empty table means the full suite is expected green.
# ---------------------------------------------------------------------------
declare -A EXPECTED_FAIL=()

echo "=== Squeezefs pjdfstest Integration ==="
echo "Repo: $REPO_DIR  Mount: $MOUNT_DIR"

# 0. Clean state from any previous runs — scoped to THIS runner's device.
echo "Cleaning up state from previous runs..."
umount -l "$MOUNT_DIR" &>/dev/null || true
pkill -f "squeezefs mount sqmeta://${META_DEV}" &>/dev/null || true
for _ in $(seq 1 100); do
    pgrep -f "squeezefs mount sqmeta://${META_DEV}" >/dev/null || break
    sleep 0.1
done
pkill -9 -f "squeezefs mount sqmeta://${META_DEV}" &>/dev/null || true
rm -f "$META_DEV" "$DATA_DEV" "$LOG_FILE" "$PROVE_OUT"
rm -rf "$STAGING_DIR" "$MOUNT_DIR"
mkdir -p "$MOUNT_DIR" "$STAGING_DIR"

# 1. Install prerequisites (pjdfstest: autotools + a C compiler; prove ships
#    with the system perl).
echo "Installing prerequisites..."
if command -v apt-get &>/dev/null; then
    apt-get install -y autoconf automake libtool make gcc perl >/dev/null
elif command -v pacman &>/dev/null; then
    echo "Arch Linux detected, assuming prerequisites are installed."
else
    echo "Warning: package manager not supported, assuming prerequisites are installed."
fi
# Arch keeps perl's core utilities (incl. prove) out of the default PATH.
if [ -d /usr/bin/core_perl ]; then
    export PATH="$PATH:/usr/bin/core_perl"
fi
if ! command -v prove &>/dev/null; then
    echo "ERROR: 'prove' (perl Test::Harness) not found." >&2
    exit 1
fi

# 2. Build Squeezefs release binary
cd "$REPO_DIR"
if [ "$RUNUSER" != "root" ] && id "$RUNUSER" &>/dev/null; then
    su -s /bin/bash "$RUNUSER" -c "cd '$REPO_DIR' && cargo build --release"
else
    cargo build --release
fi
SQUEEZEFS_BIN="$REPO_DIR/target/release/squeezefs"

# 3. Clone and build pjdfstest if not already cached
if [ ! -d "$PJDFSTEST_DIR" ]; then
    echo "Cloning pjdfstest..."
    git clone --depth 1 "$PJDFSTEST_REPO" "$PJDFSTEST_DIR"
fi
if [ ! -x "$PJDFSTEST_DIR/pjdfstest" ]; then
    echo "Building pjdfstest..."
    (cd "$PJDFSTEST_DIR" && autoreconf -ifs && ./configure && make pjdfstest) \
        >/tmp/squeezefs_pjdfstest_build.log 2>&1 || {
        echo "ERROR: pjdfstest build failed; see /tmp/squeezefs_pjdfstest_build.log" >&2
        exit 1
    }
fi

# 4. Format and mount a fresh squeezefs volume (file-backed on tmpfs, the
#    LTP-runner pattern; pjdfstest is a semantics suite, not a perf rig).
truncate -s "$META_SIZE" "$META_DEV"
truncate -s "$DATA_SIZE" "$DATA_DEV"

echo "Formatting squeezefs volume..."
"$SQUEEZEFS_BIN" format \
    "sqmeta://$META_DEV" \
    "sqdata://$DATA_DEV" \
    --disk-cache-paths "$STAGING_DIR" \
    --force >/dev/null

# Cache-path policy: staging dirs were declared at format above and are read
# from the format config — mount rejects the flag.
echo "Mounting squeezefs..."
"$SQUEEZEFS_BIN" mount \
    "sqmeta://$META_DEV" \
    "$MOUNT_DIR" \
    --daemon \
    --disk-cache-size 500MB \
    --log-file "$LOG_FILE" \
    --allow-other

for _ in $(seq 1 100); do
    mountpoint -q "$MOUNT_DIR" && break
    sleep 0.1
done
if ! mountpoint -q "$MOUNT_DIR"; then
    echo "ERROR: Failed to mount Squeezefs!" >&2
    cat "$LOG_FILE" 2>/dev/null || true
    exit 1
fi
chmod 1777 "$MOUNT_DIR"

cleanup() {
    cd /
    umount "$MOUNT_DIR" &>/dev/null || umount -l "$MOUNT_DIR" &>/dev/null || true
    for _ in $(seq 1 300); do
        pgrep -f "squeezefs mount sqmeta://${META_DEV}" >/dev/null || break
        sleep 0.1
    done
    pkill -9 -f "squeezefs mount sqmeta://${META_DEV}" &>/dev/null || true
    rm -rf "$MOUNT_DIR" "$STAGING_DIR"
    rm -f "$META_DEV" "$DATA_DEV"
}
trap cleanup EXIT

# 5. Select tests: full recursive run, or explicit paths for fix loops.
TEST_PATHS=()
if [ $# -gt 0 ]; then
    for t in "$@"; do
        if [ -e "$PJDFSTEST_DIR/tests/$t" ]; then
            TEST_PATHS+=("$PJDFSTEST_DIR/tests/$t")
        elif [ -e "$t" ]; then
            TEST_PATHS+=("$(realpath "$t")")
        else
            echo "ERROR: test '$t' not found under $PJDFSTEST_DIR/tests" >&2
            exit 1
        fi
    done
else
    TEST_PATHS=("$PJDFSTEST_DIR/tests")
fi

# 6. Run the suite from inside the mount (pjdfstest resolves the target fs
#    from the working directory). prove's exit code alone is not the verdict:
#    the expected-fail table is adjudicated below.
echo "Running pjdfstest (prove -r ${TEST_PATHS[*]})..."
cd "$MOUNT_DIR"
set +e
prove -r "${TEST_PATHS[@]}" 2>&1 | tee "$PROVE_OUT"
PROVE_RC=${PIPESTATUS[0]}
set -e
cd /

# 7. Adjudicate against the expected-fail table.
#    prove's "Test Summary Report" lists each failing .t with its failed
#    subtest numbers; normalize to "relative/path.t:n n n" and diff.
declare -A OBSERVED_FAIL=()
current=""
while IFS= read -r line; do
    wstat_re='^(/[^ ]+\.t)[[:space:]]+\(Wstat:[[:space:]]*([0-9]+).*Failed:[[:space:]]*([0-9]+)\)'
    if [[ "$line" =~ $wstat_re ]]; then
        abs="${BASH_REMATCH[1]}"
        wstat="${BASH_REMATCH[2]}"
        nfailed="${BASH_REMATCH[3]}"
        if [ "$nfailed" -gt 0 ] || [ "$wstat" -ne 0 ]; then
            # A real failure (TODO-passed-only entries carry Failed: 0).
            current="${abs#"$PJDFSTEST_DIR"/tests/}"
            OBSERVED_FAIL["$current"]="${OBSERVED_FAIL[$current]:-}"
        else
            current=""
        fi
    elif [[ -n "$current" && "$line" =~ ^[[:space:]]+Failed[[:space:]]tests?:[[:space:]]*(.*)$ ]]; then
        nums="${BASH_REMATCH[1]}"
        # Expand "1-4, 7" style lists to "1 2 3 4 7".
        expanded=""
        for tok in $(echo "$nums" | tr ',' ' '); do
            if [[ "$tok" =~ ^([0-9]+)-([0-9]+)$ ]]; then
                expanded+=" $(seq -s' ' "${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}")"
            elif [[ "$tok" =~ ^[0-9]+$ ]]; then
                expanded+=" $tok"
            fi
        done
        OBSERVED_FAIL["$current"]="$(echo "${OBSERVED_FAIL[$current]:-}$expanded" | xargs || true)"
    elif [[ ! "$line" =~ ^[[:space:]] ]]; then
        current=""
    fi
done < "$PROVE_OUT"

VERDICT=0

for t in "${!OBSERVED_FAIL[@]}"; do
    obs="${OBSERVED_FAIL[$t]}"
    exp="${EXPECTED_FAIL[$t]:-__ABSENT__}"
    if [ "$exp" = "__ABSENT__" ]; then
        echo "UNEXPECTED FAILURE: $t (failed subtests: ${obs:-<non-zero wstat / plan mismatch>})" >&2
        VERDICT=1
    elif [ "$(echo "$obs" | xargs)" != "$(echo "$exp" | xargs)" ]; then
        echo "UNEXPECTED SHAPE: $t failed subtests [${obs}] != documented expected [${exp}]" >&2
        VERDICT=1
    else
        echo "expected-fail (documented by-design): $t [${exp}]"
    fi
done

# Stale-table check: only meaningful for entries inside the selected scope.
for t in "${!EXPECTED_FAIL[@]}"; do
    in_scope=0
    for p in "${TEST_PATHS[@]}"; do
        case "$PJDFSTEST_DIR/tests/$t" in "$p"|"$p"/*) in_scope=1 ;; esac
    done
    if [ "$in_scope" = "1" ] && [ -z "${OBSERVED_FAIL[$t]:-}" ]; then
        echo "STALE EXPECTED-FAIL ENTRY: $t now passes — remove it from the table" >&2
        VERDICT=1
    fi
done

if [ "$PROVE_RC" -ne 0 ] && [ "${#OBSERVED_FAIL[@]}" -eq 0 ]; then
    # prove failed without a parseable summary (aborted run, dubious wstat).
    echo "prove exited $PROVE_RC with no parseable failure summary — see $PROVE_OUT" >&2
    VERDICT=1
fi

echo "=== pjdfstest Completed (prove=$PROVE_RC verdict=$VERDICT) ==="
exit $VERDICT
