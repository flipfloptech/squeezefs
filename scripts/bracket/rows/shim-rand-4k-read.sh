# Random 4 KiB reads through the shim's shared-memory ring (charter §13: the 942 k read IOPS bar); engagement: Δipc_ops_read accounts for every op or the row is INVALID.
ROW_KIND=data-read
ROW_NEEDS_SHIM=1
FIO_JOB=rand-4k.fio
FIO_RW=randread
FIO_DIRECT=1
FIO_BS=4k
FIO_PREFIX=rand
# psync: fio's libaio engine creates its context through libaio's own
# io_queue_init, which never crosses the shim's PLT, so its io_submit passes
# through to the kernel (ipc_ops_* stay 0: INVALID by the engagement law —
# see docs/evidence/rung-09-brackets.md).
FIO_IOENGINE=${FIO_IOENGINE:-psync}
FIO_HEADLINE=iops
# shellcheck source=_fio.sh
source "$(dirname "${BASH_SOURCE[0]}")/_fio.sh"
