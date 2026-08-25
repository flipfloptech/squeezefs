#!/usr/bin/env bash
set -eu
mounts=(/mnt/sqz-mwfleet/m50 /mnt/sqz-mwfleet/m51 /mnt/sqz-mwfleet/m52 /mnt/sqz-mwfleet/m53 /mnt/sqz-mwfleet/m54 /mnt/sqz-mwfleet/m55 /mnt/sqz-mwfleet/m56 /mnt/sqz-mwfleet/m57)
r="${OMPI_COMM_WORLD_RANK:-${PMIX_RANK:-${PMI_RANK:-0}}}"
exec "$@" -o "${mounts[$((r / 4))]}/s11-mpiio.dat"
