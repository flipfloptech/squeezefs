//! Derivation primitives and system facts (charter §1.5 "Every constant is
//! DERIVED").
//!
//! Sizing defaults are functions of the memory budget, core count, block/ring
//! geometry, or measured rates — never free-floating constants. The helpers
//! here are the vocabulary every derivation is written in, so the tie tests
//! (`tests/derivations.zig` and the per-module tests) can restate each formula
//! in one line and go red when the formula drifts.
//!
//! System facts are read once at startup through `Facts.probe()` and passed
//! explicitly; a derivation that takes `Facts` is pure and testable with any
//! synthetic machine shape.

const std = @import("std");
const fuzzkit = @import("fuzzkit.zig");
const builtin = @import("builtin");
const linux = std.os.linux;

pub const KiB: u64 = 1 << 10;
pub const MiB: u64 = 1 << 20;
pub const GiB: u64 = 1 << 30;
pub const TiB: u64 = 1 << 40;

/// `clamp(v, lo, hi)` with the floor winning when `lo > hi` (a floor is a
/// physical minimum or the shipped posture; it is never undercut by a cap
/// derived from a small machine).
pub fn clamp(v: u64, lo: u64, hi: u64) u64 {
    if (lo > hi) return lo;
    return @min(@max(v, lo), hi);
}

pub fn ceilDiv(n: u64, d: u64) u64 {
    std.debug.assert(d != 0);
    return (n + d - 1) / d;
}

pub fn alignUp(v: u64, a: u64) u64 {
    std.debug.assert(std.math.isPowerOfTwo(a));
    return (v + a - 1) & ~(a - 1);
}

pub fn alignDown(v: u64, a: u64) u64 {
    std.debug.assert(std.math.isPowerOfTwo(a));
    return v & ~(a - 1);
}

pub fn isAligned(v: u64, a: u64) bool {
    std.debug.assert(std.math.isPowerOfTwo(a));
    return (v & (a - 1)) == 0;
}

/// Smallest power of two ≥ v (v = 0 ⇒ 1).
pub fn nextPow2(v: u64) u64 {
    if (v <= 1) return 1;
    return @as(u64, 1) << @intCast(64 - @clz(v - 1));
}

/// `v / 10 * n`: the reference's integer form for tenths (`c / 10 * 8` is
/// 80 % of the cgroup cap, `ram / 10 * 7` is 70 % of RAM). Truncates `v` to a
/// multiple of 10 first, exactly as the reference does, so budgets agree to
/// the byte.
pub fn tenths(v: u64, n: u64) u64 {
    std.debug.assert(n <= 10);
    return v / 10 * n;
}

/// `v / 100 * percent`: the integer percentage form for percentage knobs.
pub fn pct(v: u64, percent: u64) u64 {
    std.debug.assert(percent <= 100);
    return v / 100 * percent;
}

/// Facts about the machine every derivation keys on. Probed once through
/// `facts()` (the process-wide in-force copy) or `Facts.probe()`; a test
/// constructs any shape it needs.
pub const Facts = struct {
    /// CPUs the PROCESS may run on: the main thread's affinity mask (by pid,
    /// never the calling thread's — whichever thread first sizes a pool must
    /// not poison it with its own pin: the reference's Hang-1
    /// "pinned-first-toucher sizing poison"), divided by the fleet share
    /// through `withFleetShare`.
    cpus: u32,
    /// Kernel possible CPUs (`/sys/devices/system/cpu/possible`); the FUSE
    /// queue count keys on this, not on the online count, because qid == CPU
    /// id and the kernel routes to the ring only once every possible queue
    /// holds an entry (charter §4 Geometry). Never divided by the fleet
    /// share (kernel geometry, not a budget). Equal to `cpus` off Linux.
    possible_cpus: u32,
    /// The kernel page size: the unit of RSS accounting and of every
    /// page-granular mapping.
    page_size: u32,
    /// Total usable RAM in bytes (`sysinfo.totalram × mem_unit`).
    total_ram: u64,

    pub fn probe() Facts {
        const cpus: u32 = if (builtin.os.tag == .linux) processCpusLinux() else @intCast(@max(1, std.Thread.getCpuCount() catch 1));
        const page: u32 = @intCast(std.heap.pageSize());
        return .{
            .cpus = cpus,
            .possible_cpus = if (builtin.os.tag == .linux) possibleCpusLinux() orelse cpus else cpus,
            .page_size = page,
            .total_ram = if (builtin.os.tag == .linux) totalRamLinux() else 0,
        };
    }

    /// A synthetic machine for tests and tie tests.
    pub fn synthetic(cpus: u32, total_ram: u64) Facts {
        return .{ .cpus = cpus, .possible_cpus = cpus, .page_size = 4096, .total_ram = total_ram };
    }

    /// The fleet-shared view: N co-located daemons divide the CPU root
    /// exactly as the memory budget divides RAM (`effectiveParallelism`);
    /// the kernel's possible-CPU geometry and the page size are facts, not
    /// budgets, and stay.
    pub fn withFleetShare(self: Facts, share: u32) Facts {
        var f = self;
        f.cpus = effectiveParallelism(self.cpus, share);
        return f;
    }
};

/// `max(1, ceil(cpus / max(share, 1)))`: the CPU root a daemon may size
/// pools for when `share` daemons share the machine (the reference's
/// `process_parallelism() = affinity ÷ fleet share`). Floor 1: a lane
/// count of 0 parks every caller forever.
pub fn effectiveParallelism(cpus: u32, share: u32) u32 {
    return @intCast(@max(1, ceilDiv(cpus, @max(share, 1))));
}

var facts_in_force: Facts = undefined;
/// 0 = unprobed, 1 = probing, 2 = published.
var facts_state: std.atomic.Value(u8) = .init(0);

/// The process facts in force — the ONE source of `cpus`/`page_size` for
/// every module that sizes something process-wide (counter stripes, the
/// uring_fs pool shape before a pool published its config). Probed once on
/// first use; a concurrent first use waits for the probe (microseconds).
pub fn facts() Facts {
    // acquire pairs with the release store of state 2 below.
    if (facts_state.load(.acquire) == 2) return facts_in_force;
    if (facts_state.cmpxchgStrong(0, 1, .acquire, .acquire) == null) {
        facts_in_force = Facts.probe();
        facts_state.store(2, .release);
        return facts_in_force;
    }
    while (facts_state.load(.acquire) != 2) std.atomic.spinLoopHint();
    return facts_in_force;
}

/// The main thread's affinity mask (pid, not tid), popcounted; 1 on failure.
fn processCpusLinux() u32 {
    var set: linux.cpu_set_t = undefined;
    if (linux.errno(linux.sched_getaffinity(linux.getpid(), @sizeOf(linux.cpu_set_t), &set)) != .SUCCESS) return 1;
    return @max(1, @as(u32, linux.CPU_COUNT(set)));
}

fn totalRamLinux() u64 {
    var info: linux.Sysinfo = undefined;
    if (linux.errno(linux.sysinfo(&info)) != .SUCCESS) return 0;
    return @as(u64, info.totalram) * @as(u64, info.mem_unit);
}

/// Parse `/sys/devices/system/cpu/possible` ("0-63" or "0" or "0-3,8-11")
/// into a count. Returns null when unreadable.
fn possibleCpusLinux() ?u32 {
    var buf: [256]u8 = undefined;
    const fd_raw = linux.open("/sys/devices/system/cpu/possible", .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
    if (linux.errno(fd_raw) != .SUCCESS) return null;
    const fd: linux.fd_t = @intCast(fd_raw);
    defer _ = linux.close(fd);
    const n_raw = linux.read(fd, &buf, buf.len);
    if (linux.errno(n_raw) != .SUCCESS) return null;
    return parseCpuList(buf[0..n_raw]);
}

/// Count CPUs in a kernel cpulist ("0-63", "0,2-3", trailing newline allowed).
/// Total: malformed input yields null rather than a panic.
pub fn parseCpuList(text: []const u8) ?u32 {
    var total: u32 = 0;
    var it = std.mem.splitScalar(u8, std.mem.trim(u8, text, " \n\r\t"), ',');
    var any = false;
    while (it.next()) |range| {
        if (range.len == 0) return null;
        any = true;
        if (std.mem.indexOfScalar(u8, range, '-')) |dash| {
            const lo = std.fmt.parseInt(u32, range[0..dash], 10) catch return null;
            const hi = std.fmt.parseInt(u32, range[dash + 1 ..], 10) catch return null;
            if (hi < lo) return null;
            total += hi - lo + 1;
        } else {
            _ = std.fmt.parseInt(u32, range, 10) catch return null;
            total += 1;
        }
    }
    return if (any) total else null;
}

test "clamp: floor wins over an inverted cap" {
    try std.testing.expectEqual(@as(u64, 4), clamp(2, 4, 32));
    try std.testing.expectEqual(@as(u64, 32), clamp(100, 4, 32));
    try std.testing.expectEqual(@as(u64, 7), clamp(7, 4, 32));
    // lo > hi: the floor is the physical minimum; it wins.
    try std.testing.expectEqual(@as(u64, 64), clamp(10, 64, 8));
}

test "nextPow2 and alignment" {
    try std.testing.expectEqual(@as(u64, 1), nextPow2(0));
    try std.testing.expectEqual(@as(u64, 1), nextPow2(1));
    try std.testing.expectEqual(@as(u64, 4096), nextPow2(4096));
    try std.testing.expectEqual(@as(u64, 8192), nextPow2(4097));
    try std.testing.expectEqual(@as(u64, 8192), alignUp(4097, 4096));
    try std.testing.expectEqual(@as(u64, 4096), alignDown(8191, 4096));
    try std.testing.expect(isAligned(8192, 4096));
    try std.testing.expect(!isAligned(8193, 4096));
    try std.testing.expectEqual(@as(u64, 3), ceilDiv(9, 4));
}

test "tenths and pct reproduce the reference's integer forms" {
    // `c / 10 * 8` (80 %) and `ram / 10 * 7` (70 %) in the Rust reference.
    const c: u64 = 25_769_803_777;
    try std.testing.expectEqual(c / 10 * 8, tenths(c, 8));
    try std.testing.expectEqual(c / 10 * 7, tenths(c, 7));
    try std.testing.expectEqual(@as(u64, 0), tenths(0, 8));
    try std.testing.expectEqual(c / 100 * 25, pct(c, 25));
    try std.testing.expectEqual(@as(u64, 100), pct(100, 100));
}

test "parseCpuList" {
    try std.testing.expectEqual(@as(?u32, 64), parseCpuList("0-63\n"));
    try std.testing.expectEqual(@as(?u32, 1), parseCpuList("0"));
    try std.testing.expectEqual(@as(?u32, 8), parseCpuList("0-3,8-11"));
    try std.testing.expectEqual(@as(?u32, null), parseCpuList(""));
    try std.testing.expectEqual(@as(?u32, null), parseCpuList("3-1"));
    try std.testing.expectEqual(@as(?u32, null), parseCpuList("a-b"));
}

test "Facts.probe yields a sane machine; facts() is the same machine, probed once" {
    const f = Facts.probe();
    try std.testing.expect(f.cpus >= 1);
    try std.testing.expect(f.possible_cpus >= f.cpus);
    try std.testing.expect(std.math.isPowerOfTwo(f.page_size));
    if (builtin.os.tag == .linux) try std.testing.expect(f.total_ram > 0);
    const a = facts();
    const b = facts();
    try std.testing.expectEqual(a, b);
    try std.testing.expectEqual(f.possible_cpus, a.possible_cpus);
    try std.testing.expectEqual(f.page_size, a.page_size);
    try std.testing.expectEqual(f.total_ram, a.total_ram);
}

test "fleet share divides the CPU root: ceil, floor 1; possible CPUs and the page size are never divided" {
    try std.testing.expectEqual(@as(u32, 8), effectiveParallelism(32, 4));
    try std.testing.expectEqual(@as(u32, 7), effectiveParallelism(32, 5));
    try std.testing.expectEqual(@as(u32, 32), effectiveParallelism(32, 1));
    try std.testing.expectEqual(@as(u32, 32), effectiveParallelism(32, 0));
    try std.testing.expectEqual(@as(u32, 1), effectiveParallelism(4, 4096));
    try std.testing.expectEqual(@as(u32, 1), effectiveParallelism(0, 1));
    const f = Facts.synthetic(32, 320 * GiB).withFleetShare(4);
    try std.testing.expectEqual(@as(u32, 8), f.cpus);
    try std.testing.expectEqual(@as(u32, 32), f.possible_cpus);
    try std.testing.expectEqual(@as(u32, 4096), f.page_size);
    try std.testing.expectEqual(320 * GiB, f.total_ram);
    // The 4-way fleet share of 32 CPUs is a quarter of the lane count (the derivation-sweep tie).
    try std.testing.expectEqual(effectiveParallelism(32, 1) / 4, effectiveParallelism(32, 4));
}

fn cpuListFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [256]u8 = undefined;
    const len = smith.slice(&buf);
    _ = parseCpuList(buf[0..len]);
}

test "fuzz: the cpu-list parser is total on arbitrary sysfs text" {
    try fuzzkit.fuzz({}, cpuListFuzz, .{ .corpus = &.{ fuzzkit.slice("0-7\n"), fuzzkit.slice("0,2-3,9999999999\n"), fuzzkit.slice("-") } });
}
