# Random 4 KiB reads through the shim's shared-memory ring (charter §13: the 942 k read IOPS bar); engagement: Δipc_ops_read accounts for every op or the row is INVALID.
ROW_KIND=data-read
ROW_NEEDS_SHIM=1
FIO_JOB=rand-4k.fio
FIO_RW=randread
FIO_DIRECT=1
FIO_BS=4k
FIO_PREFIX=rand
# libaio (the box's instrument shape): fio's engine creates its context through
# libaio's io_queue_init, which the shim interposes (F3 in
# docs/evidence/rung-09-brackets.md); ipc_client_aio_unknown_ctx_ops names any
# iocb that still bypassed the ring. BRK_IOENGINE=psync is the QD1 × N control.
FIO_HEADLINE=iops
# Decision 0003 §K row 9: the IPC path's reads ride the same ladder — the same
# bound as the kernel row (`ipc_bytes_out` beside as the user side).
FIO_READ_AMP_BOUND=1.05
FIO_READ_AMP_WHY="W(off,4096)/4096 = 1 (4 KiB-aligned) through the IPC ring, × (1 + f)"
# shellcheck source=_fio.sh
source "$(dirname "${BASH_SOURCE[0]}")/_fio.sh"
