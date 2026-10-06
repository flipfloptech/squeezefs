//! SqueezeFS — a high-performance distributed POSIX filesystem for Linux.
//!
//! `docs/charter.md` is the specification. This file is the library root:
//! every module hangs off it, and `test` here pulls every module's tests into
//! `zig build test`. Linux-only modules compile only on Linux; the portable
//! core (codecs, derivations, knobs, counters) runs on every host so the fast
//! inner loop needs no container.

const std = @import("std");
const builtin = @import("builtin");

/// Build identity (charter §11 "Versioning"): the strings are the exe's
/// (`build_options`, imported by `main` alone), so the library never changes
/// when HEAD moves.
pub const Version = struct {
    train: []const u8,
    git_commit: []const u8,
    profile: []const u8,

    pub fn write(self: Version, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.print("squeezefs {s} ({s}, {s} profile, zig {s})\n", .{ self.train, self.git_commit, self.profile, builtin.zig_version_string });
    }
};

// ---- core ----
pub const checksum = @import("core/checksum.zig");
pub const clock = @import("core/clock.zig");
pub const derive = @import("core/derive.zig");
pub const knobs = @import("core/knobs.zig");
pub const log = @import("core/log.zig");
pub const errors = @import("core/errors.zig");
pub const counters = @import("core/counters.zig");
pub const thread_name = @import("core/thread_name.zig");

// ---- model checking: the exhaustive interleaving explorer (portable) ----
pub const model = struct {
    pub const explorer = @import("model/explorer.zig");
};

// ---- the metadata plane: MetaLV (portable codecs + cores; device I/O via io/) ----
pub const meta = struct {
    pub const conveyor = @import("meta/conveyor.zig");
    pub const device_loader = if (builtin.os.tag == .linux) @import("meta/device_loader.zig") else struct {};
    pub const backend = if (builtin.os.tag == .linux) @import("meta/backend.zig") else struct {};
    pub const tree = @import("meta/tree.zig");
    pub const smo = @import("meta/smo.zig");
    pub const leaf_merge = @import("meta/leaf_merge.zig");
    pub const checkpoint = @import("meta/checkpoint.zig");
    pub const node_snapshot = @import("meta/node_snapshot.zig");
    pub const node_cache = @import("meta/node_cache.zig");
    pub const economy = @import("meta/economy.zig");
    pub const journal_lane = if (builtin.os.tag == .linux) @import("meta/journal_lane.zig") else struct {};
    pub const tx = @import("meta/tx.zig");
    pub const heap_law = @import("meta/heap_law.zig");
    pub const inos = @import("meta/inos.zig");
    pub const dlm = @import("meta/dlm.zig");
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
    pub const node_seq = @import("meta/node_seq.zig");
    pub const node = @import("meta/node.zig");
    pub const builder = @import("meta/builder.zig");
    pub const format = @import("meta/format.zig");
    pub const node_state_core = @import("meta/node_state_core.zig");
    pub const epoch_core = @import("meta/epoch_core.zig");
    pub const conveyor_core = @import("meta/conveyor_core.zig");
};

// ---- sync primitives (charter §9), written over std.Io's futex ops ----
pub const sync = @import("sync/sync.zig");

// ---- memory (portable logic; the cgroup/proc readers are Linux-only inside) ----
pub const mem_budget = @import("mem/budget.zig");

// ---- I/O: the ring-park primitive, the io_uring file layer + metadata device posture (Linux only) ----
pub const align_probe = @import("io/align_probe.zig");
pub const ring_park = if (builtin.os.tag == .linux) @import("io/ring_park.zig") else struct {};
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
    _ = errors;
    _ = counters;
    _ = thread_name;
    _ = model.explorer;
    _ = meta.conveyor;
    if (builtin.os.tag == .linux) _ = meta.device_loader;
    if (builtin.os.tag == .linux) _ = meta.backend;
    _ = meta.tree;
    _ = meta.smo;
    _ = meta.leaf_merge;
    _ = meta.checkpoint;
    _ = meta.node_snapshot;
    _ = meta.node_cache;
    _ = meta.economy;
    _ = sync;
    if (builtin.os.tag == .linux) _ = meta.journal_lane;
    _ = meta.tx;
    _ = meta.heap_law;
    _ = meta.inos;
    _ = meta.dlm;
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
    _ = meta.node_seq;
    _ = meta.node;
    _ = meta.builder;
    _ = meta.format;
    _ = meta.node_state_core;
    _ = meta.epoch_core;
    _ = meta.conveyor_core;
    _ = mem_budget;
    _ = align_probe;
    if (builtin.os.tag == .linux) _ = ring_park;
    if (builtin.os.tag == .linux) _ = uring_fs;
    _ = nvme_derive;
    if (builtin.os.tag == .linux) _ = nvme_dev;
}
