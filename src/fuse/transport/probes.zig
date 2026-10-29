//! The transport's kernel PROBES (charter §3 "Capability discipline: every
//! kernel feature is PROBE-detected at runtime … never by kernel version.
//! Post-probe refusals fail the mount loud; the probe is the only degrade
//! point"; `docs/porting/fuse_mainline_pools.md` §0, §2).
//!
//! ONE gate since the owner's 2026-10-29 ruling (Linux ≥ 7.3 only): the INIT
//! offer's `FUSE_HAS_IO_URING_BUFPOOL` (bit 43) — the buffer-pool UAPI the
//! transport speaks, offered beside `FUSE_OVER_IO_URING` whenever
//! `fuse.enable_uring` reads Y when the kernel SENDS the INIT (the daemon
//! writes Y before `mount(2)`). A kernel that does not offer it is refused
//! by name (`bufpoolVerdict`); nothing degrades. The zero-copy arm of the
//! pool is the second verdict: `CapEff` bit 21 (the kernel's EPERM gate on
//! a zero-copy ADD_QUEUE) ∧ a sparse fixed-buffer table on a scratch ring;
//! without it the pool's queues are created copying. `FUSE_TIME_LIMITS` is
//! read off the same offer.
//!
//! What differs from the reference (0002 ledger rows): the zc capability is
//! `CapEff` bit 21 from `/proc/self/status`, not `euid == 0` (a root daemon
//! with a dropped bounding set would arm zero-copy and die at ADD_QUEUE).

const std = @import("std");
const testkit = @import("../../core/testkit.zig");
const builtin = @import("builtin");
const Io = std.Io;
const abi = @import("../abi.zig");
const counters = @import("../../core/counters.zig");
const knobs = @import("../../core/knobs.zig");

const is_linux = builtin.os.tag == .linux;
const uring128 = if (is_linux) @import("../../io/uring128.zig") else struct {};

// ---------------------------------------------------------------------------
// Constants with their reasons.
// ---------------------------------------------------------------------------

/// The scratch ring's SQ: the sparse-table probe submits nothing.
pub const SCRATCH_RING_ENTRIES: u16 = 8;
/// `CAP_SYS_ADMIN` in the `CapEff` word.
pub const CAP_SYS_ADMIN_BIT: u6 = 21;

// ---------------------------------------------------------------------------
// Gauges: the published verdicts (0 = absent / not probed).
// ---------------------------------------------------------------------------

/// `transport_probe_zc`: 1 when the pool's queues are created zero-copy
/// (bufpool ∧ cap ∧ sparse).
pub var zc_gauge: counters.Gauge = .{};
/// `transport_probe_time_limits`: 1 when the kernel offered bit 62.
pub var time_limits_gauge: counters.Gauge = .{};
/// `transport_probe_cap_sys_admin`: 1 when CapEff bit 21 is set.
pub var cap_gauge: counters.Gauge = .{};

pub const Verdict = enum(u8) {
    absent = 0,
    present = 1,

    pub fn gauge(self: Verdict) u64 {
        return @backingInt(self);
    }
};

// ---------------------------------------------------------------------------
// 1. The buffer-pool UAPI — the INIT bit IS the probe, and the one gate
// ---------------------------------------------------------------------------

/// `FUSE_HAS_IO_URING_BUFPOOL` (bit 43) in the INIT offer: the kernel speaks
/// the buffer-pool UAPI (Linux ≥ 7.3, uapi 7.46). The kernel sets it beside
/// FUSE_OVER_IO_URING iff `fuse.enable_uring` read Y when it SENT the INIT;
/// the session writes Y before `mount(2)`, so the offer is complete on every
/// mount (the first after boot included). Absent ⇒ the session refuses the
/// mount naming it.
pub fn bufpoolVerdict(offered_flags: u64) Verdict {
    return if (offered_flags & abi.FUSE_HAS_IO_URING_BUFPOOL != 0) .present else .absent;
}

// ---------------------------------------------------------------------------
// 2. The pool's zero-copy arm
// ---------------------------------------------------------------------------

/// The zero-copy arm is a conjunction: the kernel's gate on a zero-copy
/// ADD_QUEUE (`capable(CAP_SYS_ADMIN)` — read from CapEff, never `euid`) ∧
/// a sparse fixed-buffer table registers (5.19+: the per-entry slot shape
/// the kernel fills at delivery).
pub fn zcVerdict(cap_sys_admin: bool, sparse_ok: bool) Verdict {
    return if (cap_sys_admin and sparse_ok) .present else .absent;
}

/// `CapEff` from a `/proc/self/status` text: bit 21.
pub fn capSysAdminFromStatus(text: []const u8) ?bool {
    var it = std.mem.splitScalar(u8, text, '\n');
    while (it.next()) |line| {
        if (!std.mem.startsWith(u8, line, "CapEff:")) continue;
        const hex = std.mem.trim(u8, line["CapEff:".len..], " \t\r");
        const caps = std.fmt.parseInt(u64, hex, 16) catch return null;
        return caps & (@as(u64, 1) << CAP_SYS_ADMIN_BIT) != 0;
    }
    return null;
}

// ---------------------------------------------------------------------------
// 3. FUSE_TIME_LIMITS — the init-flag bit IS the probe (porting spec §2.4)
// ---------------------------------------------------------------------------

pub fn timeLimitsVerdict(offered_flags: u64) Verdict {
    return if (offered_flags & abi.FUSE_TIME_LIMITS != 0) .present else .absent;
}

/// Publish every verdict (after the gate passed).
pub fn publish(zc: Verdict, time_limits: Verdict, cap: bool) void {
    zc_gauge.set(zc.gauge());
    time_limits_gauge.set(time_limits.gauge());
    cap_gauge.set(@intFromBool(cap));
}

// ---------------------------------------------------------------------------
// The Linux drivers: the real syscalls on a scratch ring.
// ---------------------------------------------------------------------------

pub const driver = if (is_linux) struct {
    /// A sparse table of `n` slots registers on a scratch ring (5.19+).
    pub fn probeSparse(n: u32) bool {
        var ring = uring128.Ring128.init(SCRATCH_RING_ENTRIES, 0) catch return false;
        defer ring.deinit();
        ring.registerBuffersSparse(n) catch return false;
        return true;
    }

    /// `CapEff` bit 21 of this process; the test seam forces it false.
    pub fn capSysAdmin(io: Io, env: *const std.process.Environ.Map) bool {
        if (knobs.boolKnob("SQUEEZEFS_TEST_FUSE_CAP_SYS_ADMIN_ABSENT", env)) return false;
        var buf: [4096]u8 = undefined;
        const text = Io.Dir.cwd().readFile(io, "/proc/self/status", &buf) catch return false;
        return capSysAdminFromStatus(text) orelse false;
    }
} else struct {};

// ---------------------------------------------------------------------------
// Tests: every verdict, the CapEff parse, the live drivers.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the buffer-pool gate: INIT bit 43 is the probe — present beside FUSE_OVER_IO_URING (Linux ≥ 7.3), absent on an offer without it (a pre-7.3 kernel, or enable_uring reading N when the INIT was sent)" {
    try testing.expectEqual(Verdict.present, bufpoolVerdict(abi.FUSE_OVER_IO_URING | abi.FUSE_HAS_IO_URING_BUFPOOL));
    try testing.expectEqual(Verdict.present, bufpoolVerdict(abi.FUSE_HAS_IO_URING_BUFPOOL));
    try testing.expectEqual(Verdict.absent, bufpoolVerdict(abi.FUSE_OVER_IO_URING));
    try testing.expectEqual(Verdict.absent, bufpoolVerdict(abi.FUSE_HAS_SYNCFS | abi.FUSE_TIME_LIMITS));
    try testing.expectEqual(Verdict.absent, bufpoolVerdict(0));
    try testing.expectEqual(@as(u64, 1), Verdict.present.gauge());
    try testing.expectEqual(@as(u64, 0), Verdict.absent.gauge());
}

test "the zero-copy arm is a conjunction of CapEff bit 21 and the sparse table; CapEff parses from /proc/self/status text; the gauges publish" {
    try testing.expectEqual(Verdict.present, zcVerdict(true, true));
    try testing.expectEqual(Verdict.absent, zcVerdict(false, true));
    try testing.expectEqual(Verdict.absent, zcVerdict(true, false));
    try testing.expectEqual(Verdict.absent, zcVerdict(false, false));
    try testing.expectEqual(@as(?bool, true), capSysAdminFromStatus("Name:\tx\nCapInh:\t0000000000000000\nCapEff:\t00000000a82465fb\nCapBnd:\t0\n"));
    try testing.expectEqual(@as(?bool, false), capSysAdminFromStatus("CapEff:\t00000000a80465fb\n"));
    try testing.expectEqual(@as(?bool, true), capSysAdminFromStatus("CapEff: 0000000000200000"));
    try testing.expectEqual(@as(?bool, null), capSysAdminFromStatus("CapPrm:\t00000000a82465fb\n"));
    try testing.expectEqual(@as(?bool, null), capSysAdminFromStatus("CapEff:\tzz\n"));
    publish(.present, .present, true);
    try testing.expectEqual(@as(u64, 1), zc_gauge.load());
    try testing.expectEqual(@as(u64, 1), time_limits_gauge.load());
    try testing.expectEqual(@as(u64, 1), cap_gauge.load());
}

test "FUSE_TIME_LIMITS: bit 62 offered is the probe" {
    try testing.expectEqual(Verdict.present, timeLimitsVerdict(abi.FUSE_TIME_LIMITS | abi.FUSE_OVER_IO_URING));
    try testing.expectEqual(Verdict.absent, timeLimitsVerdict(abi.FUSE_OVER_IO_URING));
}

test "linux: the drivers live on this kernel — the sparse table registers, CapEff reads this process's capability, the seam forces it absent" {
    if (!is_linux) return testkit.skip(@src(), .no_linux, "probes: Linux only");
    try testing.expect(driver.probeSparse(33));
    var env: std.process.Environ.Map = .init(testing.allocator);
    defer env.deinit();
    var buf: [4096]u8 = undefined;
    const text = try Io.Dir.cwd().readFile(testing.io, "/proc/self/status", &buf);
    try testing.expectEqual(capSysAdminFromStatus(text) orelse false, driver.capSysAdmin(testing.io, &env));
    try env.put("SQUEEZEFS_TEST_FUSE_CAP_SYS_ADMIN_ABSENT", "1");
    try testing.expect(!driver.capSysAdmin(testing.io, &env));
}
