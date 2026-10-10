# Random 4 KiB O_DIRECT reads, QD32 × N jobs, kernel FUSE (charter §13: rand-4k ≥ 555 k IOPS on the sqz kernel; --mount-opts direct_device_true is the ruler row).
ROW_KIND=data-read
ROW_NEEDS_SHIM=0
FIO_JOB=rand-4k.fio
FIO_RW=randread
FIO_DIRECT=1
FIO_BS=4k
FIO_PREFIX=rand
FIO_HEADLINE=iops
# shellcheck source=_fio.sh
source "$(dirname "${BASH_SOURCE[0]}")/_fio.sh"
