# Random 1 MiB O_DIRECT reads, aligned (decision 0003 §K row 4): a random
# sub-block request that will not be re-read is bounded by its own window —
# 1.0 × (1 + f); a whole-block fetch (4×) is legal only when stream
# classification PROVES reuse.
ROW_KIND=data-read
ROW_NEEDS_SHIM=0
FIO_JOB=rand-bs.fio
FIO_RW=randread
FIO_DIRECT=1
FIO_BS=1m
FIO_PREFIX=rand
FIO_HEADLINE=mib_per_s
FIO_READ_AMP_BOUND=1.05
FIO_READ_AMP_WHY="W(off,1 MiB)/1 MiB = 1 (aligned), × (1 + f), f = 5 %; a random read PROVES no reuse so no whole-block fetch is owed"
# shellcheck source=_fio.sh
source "$(dirname "${BASH_SOURCE[0]}")/_fio.sh"
