# Sequential 1 MiB O_DIRECT reads, QD32 × N jobs, kernel FUSE (charter §13: the read GB/s bar).
ROW_KIND=data-read
ROW_NEEDS_SHIM=0
FIO_JOB=seq-1m.fio
FIO_RW=read
FIO_DIRECT=1
FIO_BS=1m
FIO_PREFIX=seq
FIO_HEADLINE=mib_per_s
# Decision 0003 §K row 6: a full sequential pass is 1.0 — whole blocks each
# fetched once, the prefetch stops at the file's last block; × (1 + f) for the
# time-based wrap (a lane re-claims after the 2 s idle law).
FIO_READ_AMP_BOUND=1.05
FIO_READ_AMP_WHY="a classified stream fetches each whole block once (1.0), the plan stops at total_blocks, × (1 + f)"
# shellcheck source=_fio.sh
source "$(dirname "${BASH_SOURCE[0]}")/_fio.sh"
