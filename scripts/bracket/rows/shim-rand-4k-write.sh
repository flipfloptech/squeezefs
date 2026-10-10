# Random 4 KiB writes through the shim's direct-drive lane (charter §13: the 727 k write IOPS bar); engagement: Δipc_ops_write accounts for every op or the row is INVALID.
ROW_KIND=data-write
ROW_NEEDS_SHIM=1
FIO_JOB=rand-4k.fio
FIO_RW=randwrite
FIO_DIRECT=1
FIO_BS=4k
FIO_PREFIX=rand
# libaio (the box's instrument shape): fio's engine creates its context through
# libaio's io_queue_init, which the shim interposes (F3 in
# docs/evidence/rung-09-brackets.md); ipc_client_aio_unknown_ctx_ops names any
# iocb that still bypassed the ring. BRK_IOENGINE=psync is the QD1 × N control.
FIO_HEADLINE=iops
# shellcheck source=_fio.sh
source "$(dirname "${BASH_SOURCE[0]}")/_fio.sh"
