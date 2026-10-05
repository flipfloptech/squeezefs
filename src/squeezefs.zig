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
    _ = mem_budget;
    _ = align_probe;
    if (builtin.os.tag == .linux) _ = uring_fs;
    _ = nvme_derive;
    if (builtin.os.tag == .linux) _ = nvme_dev;
}
