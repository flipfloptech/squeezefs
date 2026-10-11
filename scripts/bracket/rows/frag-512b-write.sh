# 512 B random writes, buffered through the write-through mount (every write(2)
# a FUSE_WRITE of 512 B: fstests generic/074's `fstest -F -b 512` shape), psync,
# over the pre-written dataset — the W2 extent overlay's in-place page fold
# (decision 0003 §J.1). The row's law: `device bytes ÷ user bytes ≤ 8×` (the
# `write_amplification_bound` verdict at bs 512, sector-aligned); c36b531
# measured 128× on this shape.
ROW_KIND=data-write
ROW_NEEDS_SHIM=0
FIO_JOB=frag-512b.fio
FIO_RW=randwrite
FIO_DIRECT=0
FIO_BS=512
FIO_PREFIX=frag
FIO_HEADLINE=iops
FIO_IOENGINE=psync
FIO_SIZE=${FIO_SIZE:-256m}
# shellcheck source=_fio.sh
source "$(dirname "${BASH_SOURCE[0]}")/_fio.sh"
