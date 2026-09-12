#!/usr/bin/env bash
set -eu
mounts=(/scratch/mnt-cw1 /scratch/mnt-cw2)
r="${OMPI_COMM_WORLD_RANK:-${PMIX_RANK:-${PMI_RANK:-0}}}"
exec "$@" -o "${mounts[$((r / 4))]}/s11-mpiio.dat"
