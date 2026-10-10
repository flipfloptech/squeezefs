//! The ONE test kit for environment skips (charter §11 "Static rails as
//! tests": every environment skip routed through one testkit — a skip
//! ledger; a release-gate mode turns skips into failures).
//!
//! A test that cannot run HERE (not Linux, not root, a capability or a
//! device or a NIC or a kernel feature missing, the reference box absent)
//! says so through `skip`: the class names WHAT is missing, the detail says
//! which thing, and `@src()` names the site. Every skip is a ledger line on
//! stderr — `skip[<class>] <file>:<line>: <detail>` — and, when the harness
//! sets `SQUEEZEFS_TEST_SKIP_LEDGER`, an appended row in that file (one row
//! per skip, tab-separated, so a gate run's skips can be counted by class).
//!
//! `SQUEEZEFS_TEST_RELEASE_GATE=1` (set by `zig build gate -Drelease-gate=true`
//! on every test run step) is the release-gate mode: a skip FAILS the test
//! (`error.SkipRefusedByReleaseGate`, the ledger line still printed) except
//! for the classes a release gate on a venue cannot honestly satisfy —
//! `needs_reference_hardware` (the box brackets) and `other_step` (a test the
//! build graph drives from another step whose cwd stages its inputs; in the
//! unit run it has nothing to test). The rail in `tests/rails.zig` refuses a
//! bare `error.SkipZigTest` anywhere outside this file, so the ledger is the
//! complete census of skips.

const std = @import("std");
const builtin = @import("builtin");
const knobs = @import("knobs.zig");

comptime {
    std.debug.assert(builtin.is_test);
}

/// What the venue lacks. The release gate fails every class but the two
/// documented in the module doc.
pub const Class = enum {
    /// The test needs Linux (io_uring, /proc, cgroups, FUSE) and this host is not.
    no_linux,
    /// The test needs root (mount(2), nvmet configfs, loop devices).
    no_root,
    /// A capability is missing from the effective set (CAP_SYS_ADMIN for mount(2), CAP_IPC_LOCK for fixed buffers).
    no_cap,
    /// A device node is missing (/dev/fuse, /dev/loop-control, /dev/ng*, /dev/nvme-fabrics, a kmbuf-capable ring).
    no_device,
    /// A NIC fact is missing (a multi-queue NIC, a route, a channels control).
    no_nic,
    /// The kernel lacks a feature the test exercises (a register opcode, the nvmet target, CONFIG_ETHTOOL_NETLINK).
    no_kernel_feature,
    /// The executor refused a unit of concurrency (`error.ConcurrencyUnavailable`) the body needs.
    no_concurrency,
    /// A harness seam is not armed (a child-process marker, a looped-stage count): the test is a rig another test drives.
    no_seam,
    /// The rail under test is compiled out of this build mode (`lock_order.enabled` is Debug-only).
    debug_only,
    /// A concurrent runner swept the venue (`pkill`/`umount -l`) mid-row; the row is void, not a verdict.
    external_interference,
    /// `bench-compare`: no committed baseline for this venue id — a laptop baseline never judges another box.
    no_baseline,
    /// The row needs the quiet reference box (charter §11 measurement law); a dev venue is scoping only.
    needs_reference_hardware,
    /// The test is driven by another build step that stages its inputs (the binary, the .so) in its cwd.
    other_step,

    /// Honest on a venue: a release gate cannot conjure the reference box or
    /// another step's cwd.
    pub fn exemptFromReleaseGate(self: Class) bool {
        return self == .needs_reference_hardware or self == .other_step;
    }
};

pub const Error = error{ SkipZigTest, SkipRefusedByReleaseGate };

/// A venue probe's answer: what is missing and which thing. A rig's
/// `venueMissing()` returns `?Why` so one probe feeds one honest skip.
pub const Why = struct { class: Class, detail: []const u8 };

pub fn skipWhy(src: std.builtin.SourceLocation, why: Why) Error {
    return skip(src, why.class, why.detail);
}

const ledger_knob = "SQUEEZEFS_TEST_SKIP_LEDGER";
const release_gate_knob = "SQUEEZEFS_TEST_RELEASE_GATE";

/// THE skip. Returns the error the test must return: `error.SkipZigTest`
/// (the runner counts a skip) or, in release-gate mode, the failure.
pub fn skip(src: std.builtin.SourceLocation, class: Class, detail: []const u8) Error {
    const release_gate = switch (knobs.parseBool(release_gate_knob, std.testing.environ.getPosix(release_gate_knob))) {
        .value => |v| v,
        .absent, .invalid => false,
    };
    const verdict: Error = if (release_gate and !class.exemptFromReleaseGate()) error.SkipRefusedByReleaseGate else error.SkipZigTest;
    std.debug.print("skip[{t}] {s}:{d}: {s}{s}\n", .{ class, src.file, src.line, detail, if (verdict == error.SkipRefusedByReleaseGate) " — REFUSED: release-gate mode turns skips into failures" else "" });
    if (knobs.present(std.testing.environ.getPosix(ledger_knob))) |path| appendLedger(path, class, src, detail, verdict);
    return verdict;
}

/// One tab-separated row appended under an exclusive lock (test threads and
/// the gate's test processes share the file).
fn appendLedger(path: []const u8, class: Class, src: std.builtin.SourceLocation, detail: []const u8, verdict: Error) void {
    const io = std.testing.io;
    var line_buf: [1024]u8 = undefined;
    const line = std.fmt.bufPrint(&line_buf, "{t}\t{s}:{d}\t{s}\t{s}\n", .{ class, src.file, src.line, @errorName(verdict), detail }) catch return;
    const f = std.Io.Dir.cwd().createFile(io, path, .{ .truncate = false, .lock = .exclusive }) catch return;
    defer f.close(io);
    const end = f.length(io) catch return;
    f.writePositionalAll(io, line, end) catch return;
}

/// The Linux venue probes the mount rigs share (the smoke, the bench smoke,
/// the shim smoke, the transport e2e, the IL host e2e, the guard smoke):
/// ONE answer per missing thing, in the order that keeps the classes
/// honest — a staged input absent from the cwd is `other_step` (the unit
/// run is not that step) BEFORE root, `/dev/fuse` and CAP_SYS_ADMIN are
/// judged.
pub const venue = if (builtin.os.tag == .linux) struct {
    const linux = std.os.linux;

    /// CAP_SYS_ADMIN's bit in `CapEff` (`linux/capability.h`).
    const CAP_SYS_ADMIN_BIT: u6 = 21;

    pub fn staged(names: []const []const u8) ?Why {
        var st: linux.Statx = undefined;
        for (names) |name| {
            var zbuf: [linux.PATH_MAX:0]u8 = undefined;
            const z = std.fmt.bufPrintSentinel(&zbuf, "{s}", .{name}, 0) catch return .{ .class = .other_step, .detail = "a staged input's name does not fit PATH_MAX" };
            if (linux.errno(linux.statx(linux.AT.FDCWD, z.ptr, 0, .{ .TYPE = true }, &st)) != .SUCCESS) return .{ .class = .other_step, .detail = name };
        }
        return null;
    }

    pub fn root() ?Why {
        if (linux.getuid() != 0) return .{ .class = .no_root, .detail = "mount(2) needs root" };
        return null;
    }

    pub fn devFuse() ?Why {
        const rc = linux.open("/dev/fuse", .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
        if (linux.errno(rc) != .SUCCESS) return .{ .class = .no_device, .detail = "/dev/fuse" };
        _ = linux.close(@intCast(rc));
        return null;
    }

    /// `CapEff` from `/proc/self/status`: a container without CAP_SYS_ADMIN
    /// gets EPERM from mount(2); `scripts/venue up` grants it.
    pub fn capSysAdmin(io: std.Io) ?Why {
        var buf: [4096]u8 = undefined;
        const text = std.Io.Dir.cwd().readFile(io, "/proc/self/status", &buf) catch return .{ .class = .no_cap, .detail = "/proc/self/status unreadable (CapEff unknown)" };
        var it = std.mem.splitScalar(u8, text, '\n');
        while (it.next()) |line| {
            if (!std.mem.startsWith(u8, line, "CapEff:")) continue;
            const caps = std.fmt.parseInt(u64, std.mem.trim(u8, line["CapEff:".len..], " \t"), 16) catch break;
            if (caps & (@as(u64, 1) << CAP_SYS_ADMIN_BIT) != 0) return null;
            break;
        }
        return .{ .class = .no_cap, .detail = "CAP_SYS_ADMIN is not in the effective set — mount(2) would be EPERM; recreate the venue with `scripts/venue up`" };
    }

    /// The composite the mount rigs use: the staged inputs, root,
    /// `/dev/fuse`, CAP_SYS_ADMIN.
    pub fn mount(io: std.Io, staged_names: []const []const u8) ?Why {
        return staged(staged_names) orelse root() orelse devFuse() orelse capSysAdmin(io);
    }
} else struct {};

test "the class table: exactly the two documented classes are exempt from the release gate" {
    inline for (@typeInfo(Class).@"enum".field_values) |v| {
        const c: Class = @fromBackingInt(@intCast(v));
        try std.testing.expectEqual(c == .needs_reference_hardware or c == .other_step, c.exemptFromReleaseGate());
    }
}

test "the ledger knobs are registered harness rows" {
    try std.testing.expect(knobs.lookup(ledger_knob) != null);
    try std.testing.expect(knobs.lookup(release_gate_knob) != null);
    try std.testing.expectEqual(knobs.Class.harness, knobs.lookup(ledger_knob).?.class);
    try std.testing.expectEqual(knobs.Class.harness, knobs.lookup(release_gate_knob).?.class);
}

test "skip returns SkipZigTest outside release-gate mode and prints the ledger line" {
    // The process environment of the test run decides the mode; this test
    // asserts the kit's verdict agrees with it either way.
    const release_gate = switch (knobs.parseBool(release_gate_knob, std.testing.environ.getPosix(release_gate_knob))) {
        .value => |v| v,
        .absent, .invalid => false,
    };
    const v = skip(@src(), .needs_reference_hardware, "the kit's own probe (an exempt class: never refused)");
    try std.testing.expectEqual(Error.SkipZigTest, v);
    const w = skip(@src(), .no_device, "the kit's own probe (a refusable class)");
    try std.testing.expectEqual(if (release_gate) Error.SkipRefusedByReleaseGate else Error.SkipZigTest, w);
}
