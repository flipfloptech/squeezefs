# Random 4 KiB O_DIRECT reads, QD32 × N jobs, kernel FUSE (charter §13: rand-4k ≥ 555 k IOPS on the sqz kernel; --mount-opts direct_device_true is the ruler row).
ROW_KIND=data-read
ROW_NEEDS_SHIM=0
FIO_JOB=rand-4k.fio
FIO_RW=randread
FIO_DIRECT=1
FIO_BS=4k
FIO_PREFIX=rand
FIO_HEADLINE=iops
# Decision 0003 §K row 2 (aligned): W(off,4096)/4096 = 1, × (1 + f).
FIO_READ_AMP_BOUND=1.05
FIO_READ_AMP_WHY="W(off,4096)/4096 = 1 (4 KiB-aligned), × (1 + f), f = SQUEEZEFS_READ_ADMISSION_FILL_PCT 5 % for the governed ghost escalations"
# shellcheck source=_fio.sh
source "$(dirname "${BASH_SOURCE[0]}")/_fio.sh"
