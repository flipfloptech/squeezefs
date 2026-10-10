# Sequential 1 MiB O_DIRECT writes through LD_PRELOAD=libsqueezefs_il.so on an -o interception mount; 1 MiB is above the hybrid lane gate, so the ops route to kernel FUSE by design (charter §7) and the engagement law judges Δfuse_ops.
ROW_KIND=data-write
ROW_NEEDS_SHIM=1
FIO_JOB=seq-1m.fio
FIO_RW=write
FIO_DIRECT=1
FIO_BS=1m
FIO_PREFIX=seq
# libaio (the box's instrument shape): fio's engine creates its context through
# libaio's io_queue_init, which the shim interposes (F3 in
# docs/evidence/rung-09-brackets.md); ipc_client_aio_unknown_ctx_ops names any
# iocb that still bypassed the ring. BRK_IOENGINE=psync is the QD1 × N control.
FIO_HEADLINE=mib_per_s
# shellcheck source=_fio.sh
source "$(dirname "${BASH_SOURCE[0]}")/_fio.sh"
