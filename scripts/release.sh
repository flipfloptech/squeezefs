#!/usr/bin/env bash
# The release act (charter §11 "Versioning": a release train, semantic, bumped
# only as a release act across all first-party components; the git commit is
# the fine-grained identity; the daemon and shim refuse to pair across builds).
#
#   scripts/release.sh <train> [--dry-run] [--wire-abi-bump] [--sdk-abi-bump]
#
# ONE commit on `dev`, tagged `v<train>`, that moves every train-bearing word:
#   - build.zig           `const train_version = "<train>";`
#   - build.zig.zon       `.version = "<train>"`            (build.zig refuses to configure when they disagree)
#   - --wire-abi-bump:    src/ipc/wire.zig `SDK_ABI_CURRENT` += 1 and the accepted window becomes {cur, cur-1};
#                         include/squeezefs_sdk.h `SQZ_ABI_WIRE` follows
#                         (ONLY when the frozen subset changed — il_sdk §4; the daemon refuses an app outside the window)
#   - --sdk-abi-bump:     src/sdk/version.zig `SDK_ABI` += 1; include/squeezefs_sdk.h `SQZ_ABI_SDK` follows;
#                         build.zig's libsqueezefs_sdk.so `.version` minor += 1 (the SONAME stays .so.1: additive C surface)
#   The shim's SONAME has no version: it pairs on the build commit, never on a number (charter §11, §12).
#
# Then it VERIFIES on the Linux venue (scripts/venue): `squeezefs --version`
# names the train; `libsqueezefs_il.so` carries `squeezefs-il train <train>`;
# `libsqueezefs_sdk.so`'s `sqz_version()` text carries `squeezefs-sdk <train>`;
# the daemon↔shim pairing refusal across builds is the existing tests
# `src/ipc/client.zig` "e2e: every HELLO refusal — … a wrong commit, a -dirty
# commit …" and `src/ipc/wire.zig` "degenerate identities: unknown, empty and
# -dirty refuse the same-commit law"; `zig build -Dprofile=dist dist` produces
# `squeezefs` + `squeezefs.debug` with a `.gnu_debuglink` naming the debug
# file and a binary that names the dist profile.
#
# --dry-run performs the edits in a throwaway worktree under
# `.release-dry-run/<train>` (inside the repo, so the venue sees it), prints
# the diff, runs the whole verification there, and removes the worktree:
# nothing is committed or tagged. Without it the act requires a clean `dev`.
set -euo pipefail

usage() { sed -n '2,32p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }

TRAIN=${1:-}; shift || true
[ -n "$TRAIN" ] || usage
DRY=0; WIRE=0; SDK=0
for a in "$@"; do
  case "$a" in
    --dry-run) DRY=1 ;;
    --wire-abi-bump) WIRE=1 ;;
    --sdk-abi-bump) SDK=1 ;;
    *) echo "release: unknown flag $a" >&2; usage ;;
  esac
done
[[ "$TRAIN" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "release: '$TRAIN' is not a semantic train X.Y.Z" >&2; exit 2; }

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"
CURRENT=$(sed -n 's/^const train_version = "\(.*\)";$/\1/p' build.zig)
[ -n "$CURRENT" ] || { echo "release: build.zig has no train_version" >&2; exit 1; }
say() { printf '\n== %s\n' "$*"; }

say "release act: train $CURRENT -> $TRAIN (dry-run=$DRY wire-abi-bump=$WIRE sdk-abi-bump=$SDK)"
# The train only moves forward.
if [ "$(printf '%s\n%s\n' "$CURRENT" "$TRAIN" | sort -V | tail -1)" != "$TRAIN" ] || [ "$CURRENT" = "$TRAIN" ]; then
  echo "release: $TRAIN does not advance $CURRENT" >&2; exit 1
fi

BRANCH=$(git rev-parse --abbrev-ref HEAD)
if [ "$DRY" = 0 ]; then
  [ "$BRANCH" = dev ] || { echo "release: the act lands on dev (on $BRANCH)" >&2; exit 1; }
  [ -z "$(git status --porcelain --untracked-files=no)" ] || { echo "release: the tree is dirty; the act is ONE commit from a clean dev" >&2; exit 1; }
  WORK="$REPO"
else
  WORK="$REPO/.release-dry-run/$TRAIN"
  rm -rf "$WORK"; mkdir -p "$(dirname "$WORK")"
  git worktree add --detach "$WORK" HEAD >/dev/null
  trap 'cd "$REPO"; git worktree remove --force "$WORK" >/dev/null 2>&1 || true; rmdir .release-dry-run 2>/dev/null || true' EXIT
  say "dry run: editing the throwaway worktree $WORK (HEAD $(git rev-parse --short=12 HEAD))"
fi
cd "$WORK"

# ---- the edits ----
say "edits"
sed -i.bak "s/^const train_version = \"$CURRENT\";$/const train_version = \"$TRAIN\";/" build.zig && rm build.zig.bak
sed -i.bak "s/^    \.version = \"$CURRENT\",$/    .version = \"$TRAIN\",/" build.zig.zon && rm build.zig.zon.bak
grep -q "train_version = \"$TRAIN\"" build.zig || { echo "release: build.zig edit failed" >&2; exit 1; }
grep -q "\.version = \"$TRAIN\"" build.zig.zon || { echo "release: build.zig.zon edit failed" >&2; exit 1; }
if [ "$WIRE" = 1 ]; then
  cur=$(sed -n 's/^pub const SDK_ABI_CURRENT: u32 = \([0-9]*\);$/\1/p' src/ipc/wire.zig)
  new=$((cur + 1))
  sed -i.bak "s/^pub const SDK_ABI_CURRENT: u32 = $cur;$/pub const SDK_ABI_CURRENT: u32 = $new;/" src/ipc/wire.zig && rm src/ipc/wire.zig.bak
  # The window {cur, cur-1}: newest first (sdk/version.zig's windowBounds law).
  sed -i.bak "s/^pub const sdk_abi_accepted = \[_\]u32{.*};$/pub const sdk_abi_accepted = [_]u32{ SDK_ABI_CURRENT, SDK_ABI_CURRENT - 1 };/" src/ipc/wire.zig && rm src/ipc/wire.zig.bak
  sed -i.bak "s/^#define SQZ_ABI_WIRE ${cur}u$/#define SQZ_ABI_WIRE ${new}u/" include/squeezefs_sdk.h && rm include/squeezefs_sdk.h.bak
  grep -q "SQZ_ABI_WIRE ${new}u" include/squeezefs_sdk.h || { echo "release: header SQZ_ABI_WIRE edit failed" >&2; exit 1; }
  echo "wire ABI $cur -> $new (window {$new, $cur})"
fi
if [ "$SDK" = 1 ]; then
  cur=$(sed -n 's/^pub const SDK_ABI: u16 = \([0-9]*\);$/\1/p' src/sdk/version.zig)
  new=$((cur + 1))
  sed -i.bak "s/^pub const SDK_ABI: u16 = $cur;$/pub const SDK_ABI: u16 = $new;/" src/sdk/version.zig && rm src/sdk/version.zig.bak
  sed -i.bak "s/^#define SQZ_ABI_SDK ${cur}u$/#define SQZ_ABI_SDK ${new}u/" include/squeezefs_sdk.h && rm include/squeezefs_sdk.h.bak
  grep -q "SQZ_ABI_SDK ${new}u" include/squeezefs_sdk.h || { echo "release: header SQZ_ABI_SDK edit failed" >&2; exit 1; }
  minor=$(sed -n 's/.*\.version = \.{ \.major = 1, \.minor = \([0-9]*\), \.patch = 0 }.*/\1/p' build.zig | head -1)
  sed -i.bak "s/\.version = \.{ \.major = 1, \.minor = $minor, \.patch = 0 }/.version = .{ .major = 1, .minor = $((minor + 1)), .patch = 0 }/" build.zig && rm build.zig.bak
  echo "sdk ABI $cur -> $new; libsqueezefs_sdk.so minor $minor -> $((minor + 1))"
fi
git --no-pager diff --stat
git --no-pager diff

# ---- the verification, on the Linux venue ----
say "verification (Linux venue)"
VENUE="$REPO/scripts/venue"
ZB="zig build --cache-dir ${SQZ_BUILD_ROOT:-/build}/zig-cache --prefix zig-out-linux"
# Every command below runs inside the venue, in WORK.
v() { "$VENUE" "cd '$WORK' && $*"; }
v "$ZB install shim sdk"
echo "-- squeezefs --version:"; v "zig-out-linux/bin/squeezefs --version" | tee /tmp/release-version.txt
grep -q "^squeezefs $TRAIN (" /tmp/release-version.txt || { echo "release: the binary does not name train $TRAIN" >&2; exit 1; }
echo "-- the shim's identity:"; v "strings zig-out-linux/lib/libsqueezefs_il.so | grep -F 'squeezefs-il train $TRAIN commit' | head -1" | tee /tmp/release-shim.txt
grep -q "squeezefs-il train $TRAIN commit" /tmp/release-shim.txt || { echo "release: the shim does not name train $TRAIN" >&2; exit 1; }
echo "-- the SDK's sqz_version text:"; v "strings zig-out-linux/lib/libsqueezefs_sdk.so | grep -F 'squeezefs-sdk $TRAIN wire-abi' | head -1" | tee /tmp/release-sdk.txt
grep -q "squeezefs-sdk $TRAIN wire-abi" /tmp/release-sdk.txt || { echo "release: the SDK does not name train $TRAIN" >&2; exit 1; }
echo "-- the daemon<->shim pairing refuses across builds (the existing tests) and the SDK window law:"
v "$ZB test -Dtest-filter='every HELLO refusal' -Dtest-filter='degenerate identities' -Dtest-filter='sdk e2e: the ABI window' --summary all 2>&1 | grep -E 'squeezefs-unit|^error' | head -4" | tee /tmp/release-pairing.txt
if grep -q "fail\|^error" /tmp/release-pairing.txt; then
  echo "release: the pairing / ABI-window tests do not pass on the bumped tree — an ABI bump must land WITH the window tests' new values before the act (the act is one commit, not two)" >&2; exit 1
fi
echo "-- the dist profile: split debug info + debuglink:"
v "$ZB -Dprofile=dist dist && readelf -p .gnu_debuglink zig-out-linux/bin/squeezefs | grep -F squeezefs.debug && test -s zig-out-linux/bin/squeezefs.debug && zig-out-linux/bin/squeezefs --version" | tee /tmp/release-dist.txt
grep -q "dist profile" /tmp/release-dist.txt || { echo "release: the dist binary does not name its profile" >&2; exit 1; }

# ---- the commit + tag ----
if [ "$DRY" = 1 ]; then
  say "dry run complete: no commit, no tag; the worktree is removed on exit"
  exit 0
fi
say "commit + tag"
git add build.zig build.zig.zon src/ipc/wire.zig src/sdk/version.zig include/squeezefs_sdk.h
git commit -q -m "release: train v$TRAIN

The release act (charter §11): train $CURRENT -> $TRAIN across build.zig, build.zig.zon$([ "$WIRE" = 1 ] && printf ', the SDK wire ABI word + window + header')$([ "$SDK" = 1 ] && printf ', the SDK ABI revision + header + libsqueezefs_sdk.so minor'). Verified on the venue: the binary, the shim and the SDK name the train; the pairing refusal tests pass; the dist profile splits its debug info with a debuglink."
git tag -a "v$TRAIN" -m "squeezefs train $TRAIN"
say "released: $(git rev-parse --short=12 HEAD) tagged v$TRAIN (push dev and the tag when ready)"
