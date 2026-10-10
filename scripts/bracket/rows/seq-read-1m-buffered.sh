# Sequential 1 MiB buffered reads, kernel FUSE (the page cache + readahead in front of the daemon).
ROW_KIND=data-read
ROW_NEEDS_SHIM=0
FIO_JOB=seq-1m.fio
FIO_RW=read
FIO_DIRECT=0
FIO_BS=1m
FIO_PREFIX=seq
FIO_HEADLINE=mib_per_s
# shellcheck source=_fio.sh
source "$(dirname "${BASH_SOURCE[0]}")/_fio.sh"
