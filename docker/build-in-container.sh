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

# The two-profile LTO law (2026-09-02): `release` (thin LTO) is the dev /
# field-A-B / gate build; SQZ_DIST=1 selects `dist` + `preload-dist`
# (fat LTO, one codegen unit) — tagged releases ONLY (`task dist:*`).
if [ "${SQZ_DIST:-0}" = "1" ]; then
  daemon_profile=dist; shim_profile=preload-dist
else
  daemon_profile=release; shim_profile=preload-release
fi
cargo build --profile "$daemon_profile" --locked
# The only sanctioned shim builds (§5.4 Issue-4 panic-profile guard):
# preload-release, or its fat-LTO twin preload-dist.
cargo build -p squeezefs-preload --profile "$shim_profile" --features interposers --locked

install -m 0755 "$CARGO_TARGET_DIR/$daemon_profile/squeezefs" "$out/squeezefs"
install -m 0755 "$CARGO_TARGET_DIR/$shim_profile/libsqueezefs_il.so" "$out/libsqueezefs_il.so"

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
