# Random 4 KiB O_DIRECT writes, QD32 × N jobs, kernel FUSE (the W1 sole-owner patch shape, charter §6.2).
ROW_KIND=data-write
ROW_NEEDS_SHIM=0
FIO_JOB=rand-4k.fio
FIO_RW=randwrite
FIO_DIRECT=1
FIO_BS=4k
FIO_PREFIX=rand
FIO_HEADLINE=iops
# shellcheck source=_fio.sh
source "$(dirname "${BASH_SOURCE[0]}")/_fio.sh"
