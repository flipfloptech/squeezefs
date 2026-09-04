#!/usr/bin/env bash
# Artifact identity + ABI verification — shared by the host `task build`
# and the in-container distro builds (docker/build-in-container.sh).
#
#   check-artifacts.sh <dir> <glibc-ceiling|auto|none> [expected-profile]
#
# Identity (KD-7 daemon/shim same-commit equality; the "unidentifiable
# binary" cluster-incident rule): `<dir>/squeezefs --version` must NOT
# contain `unknown` and must carry the checkout's full commit hash. The
# shim is a cdylib with no --version entry point, so its embedded
# SQUEEZEFS_IL_BUILD_COMMIT identity is asserted the strong way instead:
# the checkout's full hash must appear in the .so bytes.
#
# Profile (the two-profile LTO law): when the caller names the profile it
# built, the version line must end in `profile <that>` — the 1.2 `dist`
# artifacts said `profile release` (a build.rs OUT_DIR-parsing bug under
# the container's /build target dir) and only a human reading the log
# noticed.
#
# ABI: the max GLIBC_* symbol version referenced by each artifact must be
# ≤ the ceiling. "auto" resolves the ceiling from the RUNNING glibc (for
# images whose ceiling we deliberately do not hardcode, e.g. ubuntu2604);
# "none" skips the ceiling check (host dev builds).
set -euo pipefail

usage="usage: check-artifacts.sh <dir> <glibc-ceiling|auto|none> [expected-profile]"
dir=${1:?$usage}
ceiling=${2:?$usage}
expected_profile=${3:-}

commit=$(git rev-parse HEAD)

ver=$("$dir/squeezefs" --version)
echo "  squeezefs --version: $ver"
case "$ver" in
  *unknown*)
    echo "FAIL: --version contains 'unknown' — git identity did not embed" >&2
    exit 1
    ;;
esac
case "$ver" in
  *"$commit"*) ;;
  *)
    echo "FAIL: --version does not carry the checkout commit $commit" >&2
    exit 1
    ;;
esac
if [ -n "$expected_profile" ]; then
  case "$ver" in
    *" profile $expected_profile") echo "  profile: $expected_profile" ;;
    *)
      echo "FAIL: --version does not end in 'profile $expected_profile' — the binary" \
           "does not name the profile it was built with (two-profile LTO law)" >&2
      exit 1
      ;;
  esac
fi

if LC_ALL=C grep -aq "$commit" "$dir/libsqueezefs_il.so"; then
  echo "  libsqueezefs_il.so: embeds build commit $commit"
else
  echo "FAIL: shim does not embed the checkout commit $commit (KD-7 identity)" >&2
  exit 1
fi

if [ "$ceiling" != "none" ]; then
  if [ "$ceiling" = "auto" ]; then
    ceiling=$(ldd --version | head -n1 | grep -oE '[0-9]+\.[0-9]+$')
    echo "  glibc ceiling (auto, running glibc): $ceiling"
  fi
  for f in "$dir/squeezefs" "$dir/libsqueezefs_il.so"; do
    max=$(objdump -T "$f" | grep -oE 'GLIBC_[0-9]+\.[0-9]+(\.[0-9]+)?' \
      | sed 's/GLIBC_//' | sort -uV | tail -n1 || true)
    if [ -z "$max" ]; then
      echo "  $(basename "$f"): no versioned glibc symbols"
      continue
    fi
    highest=$(printf '%s\n%s\n' "$max" "$ceiling" | sort -V | tail -n1)
    if [ "$highest" != "$ceiling" ]; then
      echo "FAIL: $(basename "$f") requires GLIBC_$max > ceiling $ceiling" >&2
      exit 1
    fi
    echo "  $(basename "$f"): max GLIBC_$max <= ceiling $ceiling"
  done
fi

echo "artifact checks passed: $dir"
