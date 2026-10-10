# 4 KiB buffered random writes with one fsync per write (charter §6.4): the
# fsync phase ledger `fsync_phase_ns` (exact-sum) is the row's number beside
# fsync/s. psync; depth from the job count.
ROW_KIND=fsync
ROW_NEEDS_SHIM=0
FIO_JOB=fsync-4k.fio
FIO_RW=randwrite
FIO_DIRECT=0
FIO_BS=4k
FIO_PREFIX=fsync
FIO_HEADLINE=iops
FIO_IOENGINE=psync
FIO_SIZE=${FIO_SIZE:-256m}
# shellcheck source=_fio.sh
source "$(dirname "${BASH_SOURCE[0]}")/_fio.sh"
