# Random 256 KiB O_DIRECT reads, aligned (decision 0003 §K row 3): the request
# sits at the ranged ceiling — one window of exactly the request; bound 1.0 ×
# (1 + f) for the governed ghost escalations.
ROW_KIND=data-read
ROW_NEEDS_SHIM=0
FIO_JOB=rand-bs.fio
FIO_RW=randread
FIO_DIRECT=1
FIO_BS=256k
FIO_PREFIX=rand
FIO_HEADLINE=iops
FIO_READ_AMP_BOUND=1.05
FIO_READ_AMP_WHY="W(off,256 KiB)/256 KiB = 1 (aligned), × (1 + f), f = 5 %"
# shellcheck source=_fio.sh
source "$(dirname "${BASH_SOURCE[0]}")/_fio.sh"
