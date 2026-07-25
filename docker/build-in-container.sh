#!/usr/bin/env bash
# In-container half of `task build:<distro>` (see Taskfile.yml).
#
# Contract with the Taskfile: the checkout is bind-mounted READ-ONLY at
# its host absolute path (cwd; a linked worktree's git common dir rides
# along at its own host path) so build.rs captures the real git commit
# identity; /out is dist/<target>/ on the host; /build and the cargo
# registry are per-target named volumes (incremental rebuilds); env:
# GLIBC_CEILING (version or "auto"), optional HOST_UID/HOST_GID.
set -euo pipefail

src=$(pwd)
out=/out
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-/build/target}

# The bind mount is owned by the invoking host user, not container root —
# blanket-trust it (throwaway build container, nothing else runs here).
git config --global --add safe.directory '*'

cargo build --release --locked
# The only sanctioned shim build (§5.4 Issue-4 panic-profile guard).
cargo build -p squeezefs-preload --profile preload-release --features interposers --locked

install -m 0755 "$CARGO_TARGET_DIR/release/squeezefs" "$out/squeezefs"
install -m 0755 "$CARGO_TARGET_DIR/preload-release/libsqueezefs_il.so" "$out/libsqueezefs_il.so"

# Foreign-glibc artifacts: identity + ceiling assertions run HERE, inside
# the container that can execute them.
"$src/docker/check-artifacts.sh" "$out" \
  "${GLIBC_CEILING:?GLIBC_CEILING must be set (a version, or auto)}"

# Hand artifacts back to the invoking host user — real-docker case only.
# Rootless podman already maps container root onto the host user (there a
# chown to HOST_UID would REMAP files to a subuid); the mapping is
# detectable from the source mount's apparent owner: uid 0 = rootless
# podman (leave alone), HOST_UID = real docker (chown back).
if [ -n "${HOST_UID:-}" ] && [ -n "${HOST_GID:-}" ] \
  && [ "$(stat -c %u "$src")" = "$HOST_UID" ]; then
  chown "$HOST_UID:$HOST_GID" "$out/squeezefs" "$out/libsqueezefs_il.so" 2>/dev/null || true
fi
