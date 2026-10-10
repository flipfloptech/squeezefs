# Sequential 1 MiB O_DIRECT reads through the shim (above the lane gate: kernel FUSE by design).
ROW_KIND=data-read
ROW_NEEDS_SHIM=1
FIO_JOB=seq-1m.fio
FIO_RW=read
FIO_DIRECT=1
FIO_BS=1m
FIO_PREFIX=seq
# psync: fio's libaio engine creates its context through libaio's own
# io_queue_init, which never crosses the shim's PLT, so its io_submit passes
# through to the kernel (ipc_ops_* stay 0: INVALID by the engagement law —
# see docs/evidence/rung-09-brackets.md).
FIO_IOENGINE=${FIO_IOENGINE:-psync}
FIO_HEADLINE=mib_per_s
# shellcheck source=_fio.sh
source "$(dirname "${BASH_SOURCE[0]}")/_fio.sh"
