# Sequential 1 MiB buffered writes, kernel FUSE (hybrid I/O, charter §6.3: O_DIRECT is not an admission class — the same mechanism under the page cache).
ROW_KIND=data-write
ROW_NEEDS_SHIM=0
FIO_JOB=seq-1m.fio
FIO_RW=write
FIO_DIRECT=0
FIO_BS=1m
FIO_PREFIX=seq
FIO_HEADLINE=mib_per_s
# shellcheck source=_fio.sh
source "$(dirname "${BASH_SOURCE[0]}")/_fio.sh"
