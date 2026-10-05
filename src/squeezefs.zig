//! SqueezeFS — a high-performance distributed POSIX filesystem for Linux.
//!
//! `docs/charter.md` is the specification. This file is the library root:
//! every module hangs off it, and `test` here pulls every module's tests into
//! `zig build test`. Linux-only modules compile only on Linux; the portable
//! core (codecs, derivations, knobs, counters) runs on every host so the fast
//! inner loop needs no container.

const std = @import("std");
const builtin = @import("builtin");

pub const build_options = @import("build_options");

/// Build identity: train, commit, profile (charter §11 "Versioning").
pub const version = struct {
    pub const train = build_options.train;
    pub const git_commit = build_options.git_commit;
    pub const profile = build_options.profile;

    pub fn write(w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.print("squeezefs {s} ({s}, {s} profile, zig {s})\n", .{ train, git_commit, profile, builtin.zig_version_string });
    }
};

// ---- core ----
pub const checksum = @import("core/checksum.zig");
pub const clock = @import("core/clock.zig");
pub const derive = @import("core/derive.zig");
pub const knobs = @import("core/knobs.zig");
pub const log = @import("core/log.zig");
pub const counters = @import("core/counters.zig");
pub const thread_name = @import("core/thread_name.zig");

// ---- model checking: the exhaustive interleaving explorer (portable) ----
pub const model = struct {
    pub const explorer = @import("model/explorer.zig");
};

// ---- the metadata plane: MetaLV (portable codecs + cores; device I/O via io/) ----
pub const meta = struct {
    pub const key = @import("meta/key.zig");
    pub const record = @import("meta/record.zig");
    pub const bset = @import("meta/bset.zig");
    pub const journal_core = @import("meta/journal_core.zig");
    pub const journal_core_model = @import("meta/journal_core_model.zig");
    pub const journal = @import("meta/journal.zig");
    pub const ledger = @import("meta/ledger.zig");
    pub const alloc_core = @import("meta/alloc_core.zig");
    pub const alloc_ext = @import("meta/alloc_ext.zig");
    pub const superblock = @import("meta/superblock.zig");
    pub const geometry = @import("meta/geometry.zig");
    // @anchor:meta_k2_import
    // @anchor:meta_k6_import
    pub const node_state_core = @import("meta/node_state_core.zig");
    pub const epoch_core = @import("meta/epoch_core.zig");
    pub const conveyor_core = @import("meta/conveyor_core.zig");
};

// ---- memory (portable logic; the cgroup/proc readers are Linux-only inside) ----
pub const mem_budget = @import("mem/budget.zig");

// ---- I/O: the io_uring file layer + metadata device posture (Linux only) ----
pub const align_probe = @import("io/align_probe.zig");
pub const uring_fs = if (builtin.os.tag == .linux) @import("io/uring_fs.zig") else struct {};

// ---- I/O: NVMe block device workers (Linux only) ----
pub const nvme_derive = @import("io/nvme_derive.zig");
pub const nvme_dev = if (builtin.os.tag == .linux) @import("io/nvme_dev.zig") else struct {};

test {
    std.testing.refAllDecls(@This());
    _ = checksum;
    _ = clock;
    _ = derive;
    _ = knobs;
    _ = log;
    _ = counters;
    _ = thread_name;
    _ = model.explorer;
    _ = meta.key;
    _ = meta.record;
    _ = meta.bset;
    _ = meta.journal_core;
    _ = meta.journal_core_model;
    _ = meta.journal;
    _ = meta.ledger;
    _ = meta.alloc_core;
    _ = meta.alloc_ext;
    _ = meta.superblock;
    _ = meta.geometry;
    // @anchor:meta_k2_test
    // @anchor:meta_k6_test
    _ = meta.node_state_core;
    _ = meta.epoch_core;
    _ = meta.conveyor_core;
    _ = mem_budget;
    _ = align_probe;
    if (builtin.os.tag == .linux) _ = uring_fs;
    _ = nvme_derive;
    if (builtin.os.tag == .linux) _ = nvme_dev;
}
