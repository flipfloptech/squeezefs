//! The zcrx area: its sizing (portable — every term a pure function of the
//! probed facts with its WHY) and its mapping (Linux — PMD-aligned,
//! THP-advised, NUMA-preferred, R5-gauged). Charter §6.3 "area = admitted
//! window + MTU-grain delivery slack + the NIC ring's standing pool
//! demand"; `docs/porting/ops_zcrx.md` §3.1, §3.4, §1.3.
//!
//! `area = W + S + R`, chunk-granular:
//!   W  fill window  = `depth × max_xfer` PMD-rounded — depth commands of
//!      max_xfer IS the CID namespace IS the admitted window (one arithmetic);
//!   S  delivery slack = `W × (burst − payload) / payload` — HDS splits
//!      headers off; an MTU-grain payload starts a fresh net_iov and occupies
//!      `ceil(mtu/chunk)` chunks (≈ 73 % occupancy at 9000/4096; field
//!      64 MiB → 24 MiB); unknown MTU ⇒ W (occupancy ½);
//!   R  ring standing = `max(descs × ceil(mtu/chunk) × chunk, descs ×
//!      stride(mtu))` — the legacy/cyclic RQ vs the striding RQ (mlx5
//!      `roundup_pow_of_two(SKB_FRAG_SZ(headroom + hw_mtu))`); the RQ mode is
//!      not probeable ⇒ max. Field rail 8192 × 16 KiB = 128 MiB vs legacy
//!      96 MiB — the 32 MiB shortfall held every round-2 window starved.
//! The field shape: 64 + 24 + 128 = 216 MiB per queue.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const derive = @import("../../core/derive.zig");
const log = @import("../../core/log.zig");
const stats = @import("stats.zig");

/// The huge-page granule: the area is PMD-aligned so THP maps it whole.
pub const PMD_BYTES: u64 = 2 << 20;
/// One page class; composable with chunk arithmetic.
pub const ADMISSION_UNIT: u64 = 4096;
/// The largest single destination a dest-serve fills in one command = the
/// FUSE transport's payload base (1 MiB); one definition.
pub const LANE_MAX_XFER_CAP_BYTES: u64 = 1 << 20;
/// Pre-steering pinned-footprint rail: 8 × (W + S + R) per device at most
/// before the census pool (the real ceiling) is known.
pub const LANE_IO_QUEUES_MAX: u32 = 8;
/// mlx5's SKB linear overhead ceiled: NET_SKB_PAD 64 + hard_mtu ≤ 22 +
/// skb_shared_info 320 + two SKB_DATA_ALIGN roundings ≤ 128 ⇒ ≤ 498;
/// over-estimating only rounds UP at a pow2 boundary (safe).
pub const MPWQE_SKB_LINEAR_OVERHEAD_BYTES: u64 = 512;
/// `sqsize0 = depth − 1 ≥ 1`.
pub const DEPTH_FLOOR: u32 = 2;

pub fn pmdRound(bytes: u64) u64 {
    return @max(derive.alignUp(bytes, PMD_BYTES), PMD_BYTES);
}

/// W: depth commands of `max_xfer`, PMD-rounded, floor one PMD.
pub fn areaBytesPerQueue(depth: u32, max_xfer: u64) u64 {
    return pmdRound(@as(u64, depth) * max_xfer);
}

/// The FULL window in admission units (the retired `/2` halved engagement:
/// 32 MiB/queue admitted vs ~51 MiB/device offered ⇒ 0.05 % engagement).
pub fn admissionPermits(window: u64, chunk: u64) u64 {
    return @max(@max(window, chunk) / ADMISSION_UNIT, 1);
}

pub const Burst = struct { payload: u64, burst: u64 };

/// An MTU-grain payload occupies `ceil(mtu/chunk)` chunks.
pub fn burstGeometry(mtu: ?u32, chunk: u64) ?Burst {
    const m = mtu orelse return null;
    if (m == 0) return null;
    return .{ .payload = m, .burst = derive.ceilDiv(m, chunk) * chunk };
}

/// S.
pub fn deliverySlackBytes(window: u64, mtu: ?u32, chunk: u64) u64 {
    const g = burstGeometry(mtu, chunk) orelse return window;
    return pmdRound(derive.ceilDiv(window * (g.burst - g.payload), g.payload));
}

/// The striding-RQ stride.
pub fn mpwqeStrideBytes(mtu: u32) u64 {
    return derive.nextPow2(@as(u64, mtu) + MPWQE_SKB_LINEAR_OVERHEAD_BYTES);
}

/// R.
pub fn ringStandingBytes(rx_descs: u64, mtu: ?u32, chunk: u64) u64 {
    const g = burstGeometry(mtu, chunk) orelse return rx_descs * chunk;
    return @max(rx_descs * g.burst, rx_descs * mpwqeStrideBytes(mtu.?));
}

/// W + S + R, chunk-granular.
pub fn areaBytes(depth: u32, max_xfer: u64, mtu: ?u32, chunk: u64, rx_descs: u64) u64 {
    const w = areaBytesPerQueue(depth, max_xfer);
    return derive.alignUp(w + deliverySlackBytes(w, mtu, chunk) + ringStandingBytes(rx_descs, mtu, chunk), chunk);
}

/// The kernel requires a power of two; 1:1 with the chunks so recycling can
/// never stall the ring while refs are outstanding.
pub fn rqEntriesFor(chunks: u64) u32 {
    return @intCast(derive.nextPow2(@max(chunks, 1)));
}

pub const Geometry = struct { io_queues: u32, depth: u32 };

/// queues = the possible-CPUs slope `clamp(cpus/8, 1, 8)`; depth = the
/// OFFERED-CONCURRENCY slope `clamp(cpus × 2, 4, 64)` (BDP depth adjudicated
/// against — falsified twice; 64 × 1 MiB ≥ the ~51 MiB/device offered).
pub fn laneGeometry(cpus: u32) Geometry {
    return .{ .io_queues = @intCast(derive.clamp(cpus / 8, 1, LANE_IO_QUEUES_MAX)), .depth = @intCast(derive.clamp(@as(u64, cpus) * 2, 4, 64)) };
}

/// MQES clamps the derived depth: `max(min(queue_depth, mqes + 1), 2)`.
pub fn depthFor(queue_depth: u32, mqes: u16) u32 {
    return @max(@min(queue_depth, @as(u32, mqes) + 1), DEPTH_FLOOR);
}

/// `clamp(max_hw_sectors_kb × 1024, 1 << lba_shift, 1 MiB)` with a SATURATING
/// multiply: the unlimited-MDTS sentinel `max_hw_sectors_kb = 2147483644` on
/// every field nvme-tcp controller overflowed a checked u32 multiply and
/// silently refused every real arm.
pub fn maxXferBytes(max_hw_sectors_kb: u64, lba_shift: u6) u64 {
    const raw = std.math.mul(u64, max_hw_sectors_kb, 1024) catch std.math.maxInt(u64);
    return derive.clamp(raw, @as(u64, 1) << lba_shift, LANE_MAX_XFER_CAP_BYTES);
}

/// The sim leg (`RECV` into granted chunks): the recv grain. A power of two
/// in [64 B, PMD]; derived 64 KiB — large enough that loopback TCP's
/// segment-sized recvs fill a chunk in a few passes, small enough that a
/// 1 MiB window has ≥ 16 chunks to pipeline over.
pub const SIM_CHUNK_DEFAULT: u64 = 64 << 10;
pub const SIM_CHUNK_MIN: u64 = 64;

pub fn simChunk(explicit: ?u64) u64 {
    const want = explicit orelse SIM_CHUNK_DEFAULT;
    return derive.nextPow2(derive.clamp(want, SIM_CHUNK_MIN, PMD_BYTES));
}

/// The sim area is W only (no NIC ring, unprobed grain); the admitted
/// window is HALF of it so partially filled chunks can never exhaust the
/// pool mid-stream (backpressure lands at admission, never in the parser).
pub fn simAdmittedWindow(area_len: u64) u64 {
    return area_len / 2;
}

// ---------------------------------------------------------------------------
// The mapping (Linux).
// ---------------------------------------------------------------------------

pub const Mapped = if (builtin.os.tag == .linux) struct {
    base: [*]align(std.heap.page_size_min) u8,
    len: usize,
    chunk: u64,

    pub const Error = error{ MapFailed, BadGeometry };

    /// Over-reserve `len + PMD` PROT_NONE, MAP_FIXED the RW anonymous
    /// mapping at the first PMD-aligned address inside it, unmap the head
    /// and tail slack (an unaligned vma maps huge folios 4 KiB-wise),
    /// MADV_HUGEPAGE, `mbind(MPOL_PREFERRED, node)` BEFORE first touch
    /// (refusal-tolerant: placement is an optimisation), charge the gauge.
    pub fn map(len: u64, chunk: u64, numa_node: ?u16) Error!Mapped {
        if (len == 0 or chunk == 0 or len % chunk != 0) return error.BadGeometry;
        const reserve: usize = @intCast(len + PMD_BYTES);
        const rrc = linux.mmap(null, reserve, .{}, .{ .TYPE = .PRIVATE, .ANONYMOUS = true }, -1, 0);
        if (linux.errno(rrc) != .SUCCESS) return error.MapFailed;
        const raw: usize = rrc;
        const aligned = derive.alignUp(raw, PMD_BYTES);
        const frc = linux.mmap(@ptrFromInt(aligned), @intCast(len), .{ .READ = true, .WRITE = true }, .{ .TYPE = .PRIVATE, .ANONYMOUS = true, .FIXED = true }, -1, 0);
        if (linux.errno(frc) != .SUCCESS or frc != aligned) {
            _ = linux.munmap(@ptrFromInt(raw), reserve);
            return error.MapFailed;
        }
        if (aligned > raw) _ = linux.munmap(@ptrFromInt(raw), aligned - raw);
        const tail = raw + reserve - (aligned + @as(usize, @intCast(len)));
        if (tail > 0) _ = linux.munmap(@ptrFromInt(aligned + @as(usize, @intCast(len))), tail);
        const base: [*]align(std.heap.page_size_min) u8 = @ptrFromInt(aligned);
        _ = linux.madvise(base, @intCast(len), linux.MADV.HUGEPAGE);
        if (numa_node) |n| bindPreferred(base, @intCast(len), n);
        stats.zcrx_area_bytes.add(len);
        return .{ .base = base, .len = @intCast(len), .chunk = chunk };
    }

    pub fn unmap(self: *Mapped) void {
        _ = linux.munmap(self.base, self.len);
        stats.zcrx_area_bytes.sub(self.len);
        self.* = undefined;
    }

    pub fn chunks(self: *const Mapped) u32 {
        return @intCast(self.len / self.chunk);
    }

    pub fn bytes(self: *const Mapped) []u8 {
        return self.base[0..self.len];
    }

    /// The byte span of chunk `slot`.
    pub fn chunkBytes(self: *const Mapped, slot: u32) []u8 {
        const off: usize = @intCast(@as(u64, slot) * self.chunk);
        return self.base[off..][0..@intCast(self.chunk)];
    }

    /// `MPOL_PREFERRED` over the area: the NIC's node when known.
    fn bindPreferred(base: [*]u8, len: usize, node: u16) void {
        const MPOL_PREFERRED = 1;
        var mask: [16]usize = @splat(0);
        if (node >= mask.len * @bitSizeOf(usize)) return;
        mask[node / @bitSizeOf(usize)] |= @as(usize, 1) << @intCast(node % @bitSizeOf(usize));
        const rc = linux.syscall6(.mbind, @intFromPtr(base), len, MPOL_PREFERRED, @intFromPtr(&mask), mask.len * @bitSizeOf(usize), 0);
        if (linux.errno(rc) != .SUCCESS) log.warn("zcrx area: mbind(MPOL_PREFERRED, node {d}) refused ({t}); the area stays first-touch placed", .{ node, linux.errno(rc) });
    }
} else struct {};

// ---------------------------------------------------------------------------
// Tests: ties restating every formula on the canonical shapes.
// ---------------------------------------------------------------------------

const testing = std.testing;
const MiB = derive.MiB;

test "W: depth × max_xfer PMD-rounded with a one-PMD floor (64 × 1 MiB = 64 MiB; 3 × 4 KiB → 2 MiB)" {
    try testing.expectEqual(64 * MiB, areaBytesPerQueue(64, 1 * MiB));
    try testing.expectEqual(2 * MiB, areaBytesPerQueue(3, 4096));
    try testing.expectEqual(4 * MiB, areaBytesPerQueue(3, 1 * MiB));
    try testing.expectEqual(2 * MiB, areaBytesPerQueue(0, 1 * MiB));
}

test "admission permits: the FULL window in 4 KiB units, floor 1" {
    try testing.expectEqual(@as(u64, 16384), admissionPermits(64 * MiB, 4096));
    try testing.expectEqual(@as(u64, 1), admissionPermits(0, 4096));
    try testing.expectEqual(@as(u64, 16), admissionPermits(1024, 64 << 10));
}

test "burst geometry and delivery slack: 9000/4096 ⇒ burst 12288, 64 MiB → 24 MiB; unknown MTU ⇒ W" {
    const g = burstGeometry(9000, 4096).?;
    try testing.expectEqual(@as(u64, 9000), g.payload);
    try testing.expectEqual(@as(u64, 12288), g.burst);
    try testing.expectEqual(24 * MiB, deliverySlackBytes(64 * MiB, 9000, 4096));
    try testing.expectEqual(64 * MiB, deliverySlackBytes(64 * MiB, null, 4096));
    try testing.expect(burstGeometry(0, 4096) == null);
    // A page-exact MTU has no slack beyond the PMD floor.
    try testing.expectEqual(PMD_BYTES, deliverySlackBytes(64 * MiB, 4096, 4096));
}

test "MPWQE stride: next_pow2(mtu + 512) — 9000 → 16 KiB, 1500 → 2 KiB, 3584 → 4 KiB (the boundary rounds UP)" {
    try testing.expectEqual(@as(u64, 16384), mpwqeStrideBytes(9000));
    try testing.expectEqual(@as(u64, 2048), mpwqeStrideBytes(1500));
    try testing.expectEqual(@as(u64, 4096), mpwqeStrideBytes(3584));
    try testing.expectEqual(@as(u64, 8192), mpwqeStrideBytes(3585));
}

test "ring standing: max(legacy, striding) — 8192 × {12288, 16384} = 128 MiB (the 32 MiB over legacy that starved round 2); unknown MTU ⇒ descs × chunk" {
    try testing.expectEqual(128 * MiB, ringStandingBytes(8192, 9000, 4096));
    try testing.expectEqual(96 * MiB, 8192 * 12288);
    try testing.expectEqual(32 * MiB, ringStandingBytes(8192, null, 4096));
    try testing.expectEqual(@as(u64, 0), ringStandingBytes(0, 9000, 4096));
}

test "the area: W + S + R on the field shape = 216 MiB; chunk-granular" {
    try testing.expectEqual(216 * MiB, areaBytes(64, 1 * MiB, 9000, 4096, 8192));
    try testing.expectEqual(@as(u64, 0), areaBytes(64, 1 * MiB, 9000, 4096, 8192) % 4096);
    // Sizing flies blind without ring descriptors (probe degraded to 0).
    try testing.expectEqual(88 * MiB, areaBytes(64, 1 * MiB, 9000, 4096, 0));
}

test "refill entries: next_pow2(chunks), ≥ 1" {
    try testing.expectEqual(@as(u32, 1), rqEntriesFor(0));
    try testing.expectEqual(@as(u32, 1), rqEntriesFor(1));
    try testing.expectEqual(@as(u32, 65536), rqEntriesFor(55296));
    try testing.expectEqual(@as(u32, 32768), rqEntriesFor(32768));
}

test "lane geometry: queues clamp(cpus/8, 1, 8), depth clamp(cpus × 2, 4, 64); the depth clamps to MQES + 1 with floor 2" {
    try testing.expectEqual(Geometry{ .io_queues = 1, .depth = 4 }, laneGeometry(1));
    try testing.expectEqual(Geometry{ .io_queues = 1, .depth = 16 }, laneGeometry(8));
    try testing.expectEqual(Geometry{ .io_queues = 4, .depth = 64 }, laneGeometry(32));
    try testing.expectEqual(Geometry{ .io_queues = 8, .depth = 64 }, laneGeometry(256));
    try testing.expectEqual(@as(u32, 64), depthFor(64, 1023));
    try testing.expectEqual(@as(u32, 32), depthFor(64, 31));
    try testing.expectEqual(@as(u32, 2), depthFor(64, 0));
    try testing.expectEqual(@as(u32, 2), depthFor(1, 1023));
}

test "max_xfer: clamp(kb × 1024, lba, 1 MiB) with the saturating multiply — the 2147483644 sentinel arms at the cap" {
    try testing.expectEqual(1 * MiB, maxXferBytes(2147483644, 9));
    try testing.expectEqual(1 * MiB, maxXferBytes(std.math.maxInt(u64), 9));
    try testing.expectEqual(@as(u64, 128 << 10), maxXferBytes(128, 9));
    try testing.expectEqual(@as(u64, 4096), maxXferBytes(0, 12));
}

test "the sim chunk: derived 64 KiB; clamped to [64 B, PMD] and a power of two; the admitted window is half the area" {
    try testing.expectEqual(SIM_CHUNK_DEFAULT, simChunk(null));
    try testing.expectEqual(@as(u64, 64), simChunk(1));
    try testing.expectEqual(PMD_BYTES, simChunk(1 << 40));
    try testing.expectEqual(@as(u64, 8192), simChunk(5000));
    try testing.expectEqual(32 * MiB, simAdmittedWindow(64 * MiB));
}

test "the mapping lands PMD-aligned, chunk-granular, gauged and unmaps clean (venue)" {
    if (builtin.os.tag != .linux) return error.SkipZigTest;
    const before = stats.zcrx_area_bytes.load();
    var m = try Mapped.map(4 * MiB, 64 << 10, null);
    try testing.expectEqual(@as(usize, 0), @intFromPtr(m.base) % PMD_BYTES);
    try testing.expectEqual(@as(u32, 64), m.chunks());
    try testing.expectEqual(before + 4 * MiB, stats.zcrx_area_bytes.load());
    m.chunkBytes(63)[65535] = 0xAB;
    try testing.expectEqual(@as(u8, 0xAB), m.bytes()[4 * MiB - 1]);
    m.unmap();
    try testing.expectEqual(before, stats.zcrx_area_bytes.load());
    try testing.expectError(error.BadGeometry, Mapped.map(4 * MiB + 1, 64 << 10, null));
}
