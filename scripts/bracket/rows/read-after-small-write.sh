# Read-after-small-write (decision 0003 §K row 7): the warm-up writes 512 B at
# random 512 B-grain offsets (W2 extent records over mapped blocks); the
# measured span reads 4 KiB back at random. A read composing extent records
# over its base must read the base RANGED (one 4 KiB window per gap), never
# the whole seed block: bound 2.0 (a 4 KiB read at a 512 B grain spans ≤ 2
# LBAs) × (1 + f). The fold's own seed images (`write_side_read_dma_bytes`,
# the write-amplification agent's arm) are excluded from the bounded ratio
# and reported beside.
ROW_KIND=data-read
ROW_NEEDS_SHIM=0
FIO_JOB=rand-bs.fio
FIO_RW=randread
FIO_DIRECT=1
FIO_BS=4k
FIO_BLOCKALIGN=512
FIO_PREFIX=rasw
FIO_HEADLINE=iops
FIO_WARMUP_JOB=rand-bs.fio
FIO_WARMUP_RW=randwrite
FIO_WARMUP_BS=512
FIO_READ_AMP_BOUND=2.1
FIO_READ_AMP_WHY="a 4 KiB read at a 512 B grain spans ≤ 2 LBAs whatever composes it (extent runs over a RANGED base), × (1 + f)"
FIO_READ_AMP_EXCLUDE="write_side_read_dma_bytes"
# shellcheck source=_fio.sh
source "$(dirname "${BASH_SOURCE[0]}")/_fio.sh"
