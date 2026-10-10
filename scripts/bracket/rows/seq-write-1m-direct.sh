# Sequential 1 MiB O_DIRECT writes, QD32 × N jobs, kernel FUSE (charter §13: the write GB/s bar).
ROW_KIND=data-write
ROW_NEEDS_SHIM=0
FIO_JOB=seq-1m.fio
FIO_RW=write
FIO_DIRECT=1
FIO_BS=1m
FIO_PREFIX=seq
FIO_HEADLINE=mib_per_s
# shellcheck source=_fio.sh
source "$(dirname "${BASH_SOURCE[0]}")/_fio.sh"
