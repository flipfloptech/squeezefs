//! NVMe-oF target management, the fabrics initiator, the real PR legs and
//! the fidelity tier (charter §8 "NVMe-oF targets", §3, §5.5, §11; `docs/
//! porting/ops_nvmet_keys.md`; decision 0003 §G). Linux only: the kernel
//! `nvmet` target through configfs (`configfs.zig`), the write-ahead-
//! intent share ledger (`ledger.zig`), the verbs `share / unshare / list /
//! restore / adopt / target *` (`verbs.zig`), the initiator over
//! `/dev/nvme-fabrics` with identity, discovery and the `fabric_*` sampler
//! (`initiator.zig`), the kernel's native multipath under those connects
//! — the head's serving set judged exactly ours, the per-path nodes, the
//! iopolicy (`multipath.zig`) —, the daemon-owned data-plane connect at
//! mount and its release at umount (`mount_fabric.zig`), the `storage` census (`storage.zig`), the real
//! reservation matrix on nvmet-loop and nvmet-tcp (`pr_legs.zig`) and the
//! fidelity tier that stands the fabric up through the product's own
//! verbs with zero mocks (`fidelity.zig`, `zig build nvmet-smoke`).

const std = @import("std");
const builtin = @import("builtin");

comptime {
    if (builtin.os.tag != .linux) @compileError("ops/nvmet is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const stats = @import("nvmet/stats.zig");
pub const configfs = @import("nvmet/configfs.zig");
pub const ledger = @import("nvmet/ledger.zig");
pub const initiator = @import("nvmet/initiator.zig");
pub const multipath = @import("nvmet/multipath.zig");
pub const mount_fabric = @import("nvmet/mount_fabric.zig");
pub const verbs = @import("nvmet/verbs.zig");
pub const storage = @import("nvmet/storage.zig");
pub const pr_legs = @import("nvmet/pr_legs.zig");
pub const run = @import("nvmet/run.zig");
pub const fidelity = @import("nvmet/fidelity.zig");

test {
    _ = stats;
    _ = configfs;
    _ = ledger;
    _ = initiator;
    _ = multipath;
    _ = mount_fabric;
    _ = verbs;
    _ = storage;
    _ = pr_legs;
    _ = run;
    _ = fidelity;
}
