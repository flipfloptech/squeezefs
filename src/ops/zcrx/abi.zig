//! The zcrx io_uring surface, byte-exact (charter §12 "zcrx registration
//! opcodes resolved by probe"; `docs/porting/ops_zcrx.md` §1.2). Zig 0.17's
//! std carries the opcode numbers (`IORING_OP.RECV_ZC = 58`,
//! `IORING_REGISTER.REGISTER_ZCRX_IFQ = 32`) and NONE of the `io_uring_zcrx_*`
//! / `io_uring_region_desc` structs; this module mirrors uapi
//! `<linux/io_uring.h>` (the same ABI on every field kernel and the 7.2.6-sqz
//! venue) with offset pins, plus the three pure laws every registration and
//! every completion obeys: the registration argument carries every
//! kernel-OUT field as ZERO, a CQE names memory this lane owns
//! (`cqeAreaMatches`), and its span lies inside the area (`cqeSpanOffset`).
//! Portable: nothing here touches the kernel.

const std = @import("std");
const linux = std.os.linux;

/// `IORING_REGISTER_ZCRX_IFQ` (uapi io_uring.h): register one NIC RX
/// queue as a zero-copy receive interface queue on the ring.
pub const REGISTER_ZCRX_IFQ: u32 = @backingInt(linux.IORING_REGISTER.REGISTER_ZCRX_IFQ);
/// `IORING_OP_RECV_ZC` = 58.
pub const OP_RECV_ZC: u8 = @backingInt(linux.IORING_OP.RECV_ZC);
/// `IORING_MEM_REGION_TYPE_USER`: the refill region is caller memory.
pub const MEM_REGION_TYPE_USER: u32 = 1;
/// `IORING_ZCRX_AREA_SHIFT`: a CQE's `off` carries the area token in its
/// top 16 bits and the byte offset below.
pub const AREA_SHIFT: u6 = 48;
/// `IORING_MAX_CQ_ENTRIES = 2 × IORING_MAX_ENTRIES` (io_uring.h:160-161):
/// the CLAMP ceiling the lane's derived CQ may hit.
pub const KERNEL_MAX_CQ_ENTRIES: u32 = 2 * 32768;

comptime {
    std.debug.assert(REGISTER_ZCRX_IFQ == 32);
    std.debug.assert(OP_RECV_ZC == 58);
    std.debug.assert(linux.IORING_SETUP_CQSIZE == 1 << 3);
    std.debug.assert(linux.IORING_SETUP_CLAMP == 1 << 4);
    std.debug.assert(linux.IORING_SETUP_CQE32 == 1 << 11);
    std.debug.assert(linux.IORING_SETUP_SINGLE_ISSUER == 1 << 12);
    std.debug.assert(linux.IORING_SETUP_DEFER_TASKRUN == 1 << 13);
    std.debug.assert(linux.IORING_RECV_MULTISHOT == 1 << 1);
    std.debug.assert(linux.IORING_CQE_F_MORE == 1 << 1);
}

/// `struct io_uring_zcrx_rqe`: one refill entry — a span `(off, len)` handed
/// back to the provider pool exactly as the CQE delivered it.
pub const Rqe = extern struct {
    off: u64,
    len: u32,
    __pad: u32 = 0,
};

/// `struct io_uring_zcrx_offsets`: where the refill ring's head, tail and
/// entry array live inside the registered region (kernel OUT).
pub const Offsets = extern struct {
    head: u32 = 0,
    tail: u32 = 0,
    rqes: u32 = 0,
    __resv2: u32 = 0,
    __resv: [2]u64 = @splat(0),
};

/// `struct io_uring_zcrx_area_reg`: the area the NIC fills.
pub const AreaReg = extern struct {
    addr: u64,
    len: u64,
    /// Kernel OUT: the token every CQE of this area carries above `AREA_SHIFT`.
    rq_area_token: u64 = 0,
    flags: u32 = 0,
    dmabuf_fd: u32 = 0,
    __resv2: [2]u64 = @splat(0),
};

/// `struct io_uring_region_desc`: the refill ring's memory.
pub const RegionDesc = extern struct {
    user_addr: u64,
    size: u64,
    flags: u32 = MEM_REGION_TYPE_USER,
    id: u32 = 0,
    /// Kernel OUT.
    mmap_offset: u64 = 0,
    __resv: [4]u64 = @splat(0),
};

/// `struct io_uring_zcrx_ifq_reg`: the registration argument.
pub const IfqReg = extern struct {
    if_idx: u32,
    if_rxq: u32,
    /// IN: the refill ring size asked; OUT: the kernel's clamped echo.
    rq_entries: u32,
    flags: u32 = 0,
    area_ptr: u64,
    region_ptr: u64,
    offsets: Offsets = .{},
    /// Kernel OUT: the ifq index a `RECV_ZC` SQE names at +44.
    zcrx_id: u32 = 0,
    /// 0 = the kernel's default page grain (the net_iov granule).
    rx_buf_len: u32 = 0,
    __resv: [3]u64 = @splat(0),
};

comptime {
    std.debug.assert(@sizeOf(Rqe) == 16);
    std.debug.assert(@offsetOf(Rqe, "len") == 8);
    std.debug.assert(@offsetOf(Rqe, "__pad") == 12);
    std.debug.assert(@sizeOf(Offsets) == 32);
    std.debug.assert(@offsetOf(Offsets, "rqes") == 8);
    std.debug.assert(@sizeOf(AreaReg) == 48);
    std.debug.assert(@offsetOf(AreaReg, "rq_area_token") == 16);
    std.debug.assert(@offsetOf(AreaReg, "flags") == 24);
    std.debug.assert(@offsetOf(AreaReg, "dmabuf_fd") == 28);
    std.debug.assert(@sizeOf(RegionDesc) == 64);
    std.debug.assert(@offsetOf(RegionDesc, "flags") == 16);
    std.debug.assert(@offsetOf(RegionDesc, "mmap_offset") == 24);
    std.debug.assert(@sizeOf(IfqReg) == 96);
    std.debug.assert(@offsetOf(IfqReg, "rq_entries") == 8);
    std.debug.assert(@offsetOf(IfqReg, "area_ptr") == 16);
    std.debug.assert(@offsetOf(IfqReg, "region_ptr") == 24);
    std.debug.assert(@offsetOf(IfqReg, "offsets") == 32);
    std.debug.assert(@offsetOf(IfqReg, "zcrx_id") == 64);
    std.debug.assert(@offsetOf(IfqReg, "rx_buf_len") == 68);
    std.debug.assert(@offsetOf(IfqReg, "__resv") == 72);
}

/// The refill region: one page for the head/tail words plus 16 B per rqe,
/// page-rounded (ops_zcrx §1.2; the kernel lays the rqe array past the
/// first page).
pub fn refillRegionLen(rq_entries: u32, page: u32) u64 {
    const raw: u64 = @as(u64, page) + @as(u64, rq_entries) * @sizeOf(Rqe);
    return ((raw + page - 1) / page) * page;
}

/// The registration argument with every kernel-OUT field zero (`rq_area_token`,
/// `mmap_offset`, `offsets`, `zcrx_id`, the `rq_entries` echo is the ask).
/// `area_ptr`/`region_ptr` are WIRED BY THE CALLER after the two structs
/// reach their final slots — a pointer minted here would dangle on move-out.
pub fn buildIfqReg(if_idx: u32, if_rxq: u32, rq_entries: u32) IfqReg {
    return .{ .if_idx = if_idx, .if_rxq = if_rxq, .rq_entries = rq_entries, .area_ptr = 0, .region_ptr = 0 };
}

pub fn buildAreaReg(base: usize, len: usize) AreaReg {
    return .{ .addr = base, .len = len };
}

pub fn buildRegionDesc(base: usize, len: usize) RegionDesc {
    return .{ .user_addr = base, .size = len };
}

/// Provenance gate: the CQE's area token equals the registered area's.
/// A mismatch is a FRAME VIOLATION (the CQE names memory this lane does
/// not own), never a retry.
pub fn cqeAreaMatches(raw_off: u64, token: u64) bool {
    return (raw_off >> AREA_SHIFT) == (token >> AREA_SHIFT);
}

/// Span gate: the byte offset below the token iff `off + len ≤ area_len`
/// (checked add); null = a span outside the area (terminal).
pub fn cqeSpanOffset(raw_off: u64, len: u32, area_len: u64) ?u64 {
    const off = raw_off & ((@as(u64, 1) << AREA_SHIFT) - 1);
    const end = std.math.add(u64, off, len) catch return null;
    if (end > area_len) return null;
    return off;
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "refill region length is page-rounded: a page of words plus 16 B per entry" {
    // The head/tail page plus the first rqe already spills into a second page.
    try testing.expectEqual(@as(u64, 8192), refillRegionLen(1, 4096));
    try testing.expectEqual(@as(u64, 8192), refillRegionLen(256, 4096));
    try testing.expectEqual(@as(u64, 16384), refillRegionLen(1, 8192));
    // 4096 + 4096 × 16 = 69632 = 17 pages exactly.
    try testing.expectEqual(@as(u64, 17 * 4096), refillRegionLen(4096, 4096));
    // One entry over: an 18th page.
    try testing.expectEqual(@as(u64, 18 * 4096), refillRegionLen(4097, 4096));
}

test "the registration argument echoes its inputs and carries every kernel-OUT field as zero" {
    const reg = buildIfqReg(6, 31, 512);
    const bytes = std.mem.asBytes(&reg);
    try testing.expectEqual(@as(u32, 6), std.mem.readInt(u32, bytes[0..4], .little));
    try testing.expectEqual(@as(u32, 31), std.mem.readInt(u32, bytes[4..8], .little));
    try testing.expectEqual(@as(u32, 512), std.mem.readInt(u32, bytes[8..12], .little));
    // flags, area_ptr, region_ptr, offsets, zcrx_id, rx_buf_len, resv: all zero.
    for (bytes[12..96]) |b| try testing.expectEqual(@as(u8, 0), b);
    const area = buildAreaReg(0x1000, 1 << 21);
    try testing.expectEqual(@as(u64, 0), area.rq_area_token);
    try testing.expectEqual(@as(u32, 0), area.flags);
    try testing.expectEqual(@as(u32, 0), area.dmabuf_fd);
    const region = buildRegionDesc(0x2000, 8192);
    try testing.expectEqual(MEM_REGION_TYPE_USER, region.flags);
    try testing.expectEqual(@as(u64, 0), region.mmap_offset);
    try testing.expectEqual(@as(u32, 0), region.id);
}

test "the two CQE gates: provenance by the top 16 bits; span bounds with a checked add" {
    const token: u64 = 0x0003 << 48;
    try testing.expect(cqeAreaMatches(token | 4096, token));
    try testing.expect(!cqeAreaMatches((@as(u64, 0x0004) << 48) | 4096, token));
    try testing.expectEqual(@as(?u64, 4096), cqeSpanOffset(token | 4096, 4096, 8192));
    try testing.expectEqual(@as(?u64, null), cqeSpanOffset(token | 4096, 4097, 8192));
    try testing.expectEqual(@as(?u64, 0), cqeSpanOffset(token, 0, 0));
    try testing.expectEqual(@as(?u64, null), cqeSpanOffset(token | ((1 << 48) - 1), 2, 1 << 48));
}

fn gatesFuzz(_: void, smith: *std.testing.Smith) !void {
    var b: [24]u8 = undefined;
    smith.bytes(&b);
    const raw_off = std.mem.readInt(u64, b[0..8], .little);
    const token = std.mem.readInt(u64, b[8..16], .little);
    const len = std.mem.readInt(u32, b[16..20], .little);
    const area_len = std.mem.readInt(u32, b[20..24], .little);
    const matches = cqeAreaMatches(raw_off, token);
    try testing.expectEqual(raw_off >> 48 == token >> 48, matches);
    if (cqeSpanOffset(raw_off, len, area_len)) |off| {
        try testing.expect(off + len <= area_len);
        try testing.expect(off == raw_off & ((1 << 48) - 1));
    } else {
        const off = raw_off & ((1 << 48) - 1);
        try testing.expect(off + len > area_len);
    }
}

test "fuzz: the CQE gates are total and admit only in-area spans" {
    try std.testing.fuzz({}, gatesFuzz, .{});
}
