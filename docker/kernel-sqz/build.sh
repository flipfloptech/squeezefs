#!/usr/bin/env bash
# Host half of the sqz kernel build. Builds the pinned EL8 toolchain
# image, then runs build-kernel.sh in it with dev-box manners (capped
# CPUs, nice). Artifacts land in dist/kernel-sqz/.
#
#   docker/kernel-sqz/build.sh [--cpus N]
#
# podman or docker both work (podman preferred, matching the house
# Taskfile pattern).
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
out="$repo/dist/kernel-sqz"
cpus=16
[ "${1:-}" = "--cpus" ] && cpus=$2

engine=podman
command -v podman >/dev/null || engine=docker

mkdir -p "$out"
$engine build -t squeezefs-kernel-sqz-build:el8 -f "$here/Dockerfile" "$here"
exec nice -n 10 $engine run --rm \
	--cpus="$cpus" \
	-v "$here":/src:ro \
	-v squeezefs-kernel-sqz-work:/work \
	-v "$out":/out \
	-e JOBS="$cpus" \
	squeezefs-kernel-sqz-build:el8 \
	bash /src/build-kernel.sh
