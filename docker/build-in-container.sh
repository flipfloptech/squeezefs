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
# field-A-B / gate build; BUILD_DIST=1 selects `dist` + `preload-dist`
# (fat LTO, one codegen unit) — tagged releases ONLY (`task dist:*`). The
# selector deliberately sits outside the SQUEEZEFS_*/SQZ_* knob namespace:
# the artifact check runs `squeezefs --version` in this environment, and
# the knob registry announces every unregistered name there as a typo.
if [ "${BUILD_DIST:-0}" = "1" ]; then
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

# Split debug info on TAGGED releases (2026-09-12; the 1.2.4 dist daemon
# was 329 MiB, 305 of them DWARF): the shipped artifact is stripped of
# .debug_* and its DWARF lands beside it as `<name>.debug`, joined by a
# .gnu_debuglink (name + CRC32) — perf/gdb resolve it from the same
# directory or from /usr/lib/debug, so every profiling row stays
# symbolicated. `release` (dev / A-B / gate) keeps its symbols in-binary.
# The three objcopy steps are the documented sequence: the debug copy is
# taken from the UNSTRIPPED file, then the file is stripped, then linked.
if [ "$daemon_profile" = "dist" ]; then
  for name in squeezefs libsqueezefs_il.so; do
    objcopy --only-keep-debug "$out/$name" "$out/$name.debug"
    objcopy --strip-debug "$out/$name"
    objcopy --add-gnu-debuglink="$out/$name.debug" "$out/$name"
    chmod 0644 "$out/$name.debug"
  done
  # The release act's checksum file, written where the artifacts are
  # (the 1.2.x acts wrote it by hand on the host).
  (cd "$out" && sha256sum squeezefs libsqueezefs_il.so squeezefs.debug libsqueezefs_il.so.debug > SHA256SUMS)
fi

# Foreign-glibc artifacts: identity + ceiling assertions run HERE, inside
# the container that can execute them.
"$src/docker/check-artifacts.sh" "$out" \
  "${GLIBC_CEILING:?GLIBC_CEILING must be set (a version, or auto)}" \
  "$daemon_profile"

# Hand artifacts back to the invoking host user — real-docker case only.
# Rootless podman already maps container root onto the host user (there a
# chown to HOST_UID would REMAP files to a subuid); the mapping is
# detectable from the source mount's apparent owner: uid 0 = rootless
# podman (leave alone), HOST_UID = real docker (chown back).
if [ -n "${HOST_UID:-}" ] && [ -n "${HOST_GID:-}" ] \
  && [ "$(stat -c %u "$src")" = "$HOST_UID" ]; then
  chown "$HOST_UID:$HOST_GID" "$out"/squeezefs "$out"/libsqueezefs_il.so "$out"/*.debug "$out"/SHA256SUMS 2>/dev/null || true
fi
