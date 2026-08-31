#!/usr/bin/env bash
set -eu
mounts=(/scratch/mnt-cw1 /scratch/mnt-cw2 /scratch/mnt-cw3 /scratch/mnt-cw4 /scratch/mnt-cw5 /scratch/mnt-cw6 /scratch/mnt-cw7 /scratch/mnt-cw8)
r="${OMPI_COMM_WORLD_RANK:-${PMIX_RANK:-${PMI_RANK:-0}}}"
exec "$@" -o "${mounts[$((r / 4))]}/s11-mpiio-fpp.dat"
