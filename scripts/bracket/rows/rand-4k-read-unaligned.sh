# Random 4 KiB O_DIRECT reads at a 512 B offset grain (decision 0003 §K row 2):
# a 4 KiB window at a non-4 KiB offset spans two LBAs — bound 2.0 (7 of 8
# offsets straddle; the mean is 1.875).
ROW_KIND=data-read
ROW_NEEDS_SHIM=0
FIO_JOB=rand-bs.fio
FIO_RW=randread
FIO_DIRECT=1
FIO_BS=4k
FIO_BLOCKALIGN=512
FIO_PREFIX=rand
FIO_HEADLINE=iops
FIO_READ_AMP_BOUND=2.1
FIO_READ_AMP_WHY="max over offsets of W(off,4096)/4096 = 8192/4096 = 2 at a 512 B grain, × (1 + f), f = 5 %"
# shellcheck source=_fio.sh
source "$(dirname "${BASH_SOURCE[0]}")/_fio.sh"
