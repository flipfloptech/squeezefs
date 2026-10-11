# Sequential 1 MiB O_DIRECT reads through the shim (above the lane gate: kernel FUSE by design).
ROW_KIND=data-read
ROW_NEEDS_SHIM=1
FIO_JOB=seq-1m.fio
FIO_RW=read
FIO_DIRECT=1
FIO_BS=1m
FIO_PREFIX=seq
# libaio (the box's instrument shape): fio's engine creates its context through
# libaio's io_queue_init, which the shim interposes (F3 in
# docs/evidence/rung-09-brackets.md); ipc_client_aio_unknown_ctx_ops names any
# iocb that still bypassed the ring. BRK_IOENGINE=psync is the QD1 × N control.
FIO_HEADLINE=mib_per_s
FIO_READ_AMP_BOUND=1.05
FIO_READ_AMP_WHY="a classified stream fetches every whole block once through the IPC path's same ladder, × (1 + f)"
# shellcheck source=_fio.sh
source "$(dirname "${BASH_SOURCE[0]}")/_fio.sh"
