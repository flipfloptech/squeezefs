//! The NVMe passthru command codec of the PR primitive (charter §5.5
//! "Fencing", §5.7 B1; spec `guard_claims.md` §1.2): the 72 B
//! `struct nvme_uring_cmd` (uapi `linux/nvme_ioctl.h`) that rides the head
//! of an SQE128's 80 B command area under `IORING_OP_URING_CMD`, the
//! opcodes and CDW encodings of every command the guard issues, the status
//! classes a CQE's `res` decodes into, and the Reservation Report parser
//! over both wire forms (extended 64 B / short 24 B). Portable: the fake
//! namespace decodes the same bytes the device would (byte-exact proof).

const std = @import("std");
const errors = @import("../../core/errors.zig");

/// One command's answer: the NVMe status (0 = success) and CDW0/CDW1 (`res2`).
pub const Completion = struct { status: Status, result: u64 = 0 };

/// The namespace face the PR client drives: the real `/dev/ngXnY` through
/// `IORING_OP_URING_CMD` (`pr_linux.zig`) or the in-RAM fake
/// (`fake_nvme.zig`). The data buffer is named by `cmd.addr`/`data_len`;
/// a transport failure (the ring, the node) is an errno-class error, a
/// device answer is a status.
pub const Namespace = struct {
    ctx: *anyopaque,
    issue: *const fn (ctx: *anyopaque, queue: Queue, cmd: *const UringCmd) errors.Error!Completion,
};

// ---------------------------------------------------------------------------
// ioctl numbers (uapi `linux/nvme_ioctl.h`): `_IOWR('N', nr, struct)` =
// dir 3 << 30 | size << 16 | 'N' << 8 | nr.
// ---------------------------------------------------------------------------

fn iowr(comptime nr: u8, comptime size: u16) u32 {
    return (3 << 30) | (@as(u32, size) << 16) | (@as(u32, 'N') << 8) | nr;
}

/// NVM (I/O queue) commands through the char node.
pub const NVME_URING_CMD_IO: u32 = iowr(0x80, 72);
/// Admin commands through the char node.
pub const NVME_URING_CMD_ADMIN: u32 = iowr(0x82, 72);

comptime {
    std.debug.assert(NVME_URING_CMD_IO == 0xC048_4E80);
    std.debug.assert(NVME_URING_CMD_ADMIN == 0xC048_4E82);
}

// ---------------------------------------------------------------------------
// The 72 B command (uapi L74-93), little-endian on the wire.
// ---------------------------------------------------------------------------

pub const UringCmd = extern struct {
    opcode: u8,
    flags: u8,
    rsvd1: u16,
    nsid: u32,
    cdw2: u32,
    cdw3: u32,
    metadata: u64,
    /// The user data buffer (host address) and its length.
    addr: u64,
    metadata_len: u32,
    data_len: u32,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
    cdw13: u32,
    cdw14: u32,
    cdw15: u32,
    timeout_ms: u32,
    rsvd2: u32,

    pub fn zeroed() UringCmd {
        return std.mem.zeroes(UringCmd);
    }

    /// The command bytes at the head of an 80 B SQE128 command area,
    /// zero-padded.
    pub fn toCmdArea(self: UringCmd) [80]u8 {
        var out: [80]u8 = @splat(0);
        @memcpy(out[0..CMD_LEN], &std.mem.toBytes(self));
        return out;
    }

    pub fn fromBytes(bytes: *const [CMD_LEN]u8) UringCmd {
        return std.mem.bytesToValue(UringCmd, bytes);
    }
};

pub const CMD_LEN: usize = @sizeOf(UringCmd);

comptime {
    std.debug.assert(CMD_LEN == 72);
    std.debug.assert(@offsetOf(UringCmd, "nsid") == 4);
    std.debug.assert(@offsetOf(UringCmd, "addr") == 24);
    std.debug.assert(@offsetOf(UringCmd, "data_len") == 36);
    std.debug.assert(@offsetOf(UringCmd, "cdw10") == 40);
    std.debug.assert(@offsetOf(UringCmd, "timeout_ms") == 64);
    if (@import("builtin").cpu.arch.endian() != .little) @compileError("the nvme_uring_cmd is spelled as native integers; a big-endian target needs explicit LE fields");
}

// ---------------------------------------------------------------------------
// Opcodes and field encodings.
// ---------------------------------------------------------------------------

pub const Queue = enum { admin, io };

pub const OP_ADMIN_IDENTIFY: u8 = 0x06;
pub const OP_ADMIN_GET_FEATURES: u8 = 0x0a;
pub const OP_RESV_REGISTER: u8 = 0x0d;
pub const OP_RESV_REPORT: u8 = 0x0e;
pub const OP_RESV_ACQUIRE: u8 = 0x11;
pub const OP_RESV_RELEASE: u8 = 0x15;

/// Identify Namespace: CNS 0, 4 KiB in; RESCAP is byte 31.
pub const IDENTIFY_NS_LEN: usize = 4096;
pub const RESCAP_OFFSET: usize = 31;
/// RESCAP bit 0: Persist Through Power Loss capable (the register's CPTPL).
pub const RESCAP_PTPL: u8 = 1 << 0;

/// Get Features FID 0x81 Host Identifier; CDW11 bit 0 EXHID selects the
/// 128-bit form.
pub const FID_HOST_IDENTIFIER: u32 = 0x81;
pub const HOSTID_EXTENDED_LEN: usize = 16;
pub const HOSTID_SHORT_LEN: usize = 8;

/// Reservation types (kernel `enum nvme_pr_type`): Write Exclusive = 1 (the
/// flat volume), WERO = 3 (rung 6's manager). 2 = Exclusive Access refused a
/// registrant's READS on nvmet 2026-08-15 — never used.
pub const RTYPE_WRITE_EXCLUSIVE: u8 = 1;
pub const RTYPE_WERO: u8 = 3;

/// Register CDW10: RREGA bits 2:0, IEKEY bit 3, CPTPL bits 31:30.
pub const RREGA_REGISTER: u32 = 0;
pub const RREGA_UNREGISTER: u32 = 1;
pub const RREGA_REPLACE: u32 = 2;
pub const IEKEY: u32 = 1 << 3;
/// CPTPL 11b: persist through power loss.
pub const CPTPL_PERSIST: u32 = 0b11 << 30;

/// Acquire CDW10: RACQA bits 2:0, RTYPE bits 15:8.
pub const RACQA_ACQUIRE: u32 = 0;
pub const RACQA_PREEMPT: u32 = 1;
pub const RACQA_PREEMPT_AND_ABORT: u32 = 2;

/// Release CDW10: RRELA bits 2:0 (0 = release), RTYPE bits 15:8.
pub const RRELA_RELEASE: u32 = 0;

/// Report CDW11 bit 0: Extended Data Structure (128-bit host ids).
pub const EDS: u32 = 1;

pub fn identifyNamespace(nsid: u32, buf: []u8) UringCmd {
    std.debug.assert(buf.len == IDENTIFY_NS_LEN);
    var c = UringCmd.zeroed();
    c.opcode = OP_ADMIN_IDENTIFY;
    c.nsid = nsid;
    c.cdw10 = 0;
    c.addr = @intFromPtr(buf.ptr);
    c.data_len = @intCast(buf.len);
    return c;
}

/// FID 0x81 on nsid 0 (a controller feature); `buf.len` 16 (EXHID = 1) or
/// 8 (the PCIe 64-bit fallback).
pub fn getHostIdentifier(buf: []u8) UringCmd {
    std.debug.assert(buf.len == HOSTID_EXTENDED_LEN or buf.len == HOSTID_SHORT_LEN);
    var c = UringCmd.zeroed();
    c.opcode = OP_ADMIN_GET_FEATURES;
    c.nsid = 0;
    c.cdw10 = FID_HOST_IDENTIFIER;
    c.cdw11 = if (buf.len == HOSTID_EXTENDED_LEN) 1 else 0;
    c.addr = @intFromPtr(buf.ptr);
    c.data_len = @intCast(buf.len);
    return c;
}

/// The 16 B register payload `crkey ‖ nrkey` (LE) written into `buf`.
pub fn reservationRegister(nsid: u32, cdw10: u32, crkey: u64, nrkey: u64, buf: *[16]u8) UringCmd {
    std.mem.writeInt(u64, buf[0..8], crkey, .little);
    std.mem.writeInt(u64, buf[8..16], nrkey, .little);
    var c = UringCmd.zeroed();
    c.opcode = OP_RESV_REGISTER;
    c.nsid = nsid;
    c.cdw10 = cdw10;
    c.addr = @intFromPtr(buf);
    c.data_len = 16;
    return c;
}

/// Report of `buf.len` bytes (a multiple of 4): NUMD = len/4 − 1.
pub fn reservationReport(nsid: u32, buf: []u8, eds: bool) UringCmd {
    std.debug.assert(buf.len % 4 == 0 and buf.len >= 4);
    var c = UringCmd.zeroed();
    c.opcode = OP_RESV_REPORT;
    c.nsid = nsid;
    c.cdw10 = @intCast(buf.len / 4 - 1);
    c.cdw11 = if (eds) EDS else 0;
    c.addr = @intFromPtr(buf.ptr);
    c.data_len = @intCast(buf.len);
    return c;
}

/// The 16 B acquire payload `crkey ‖ prkey`.
pub fn reservationAcquire(nsid: u32, racqa: u32, rtype: u8, crkey: u64, prkey: u64, buf: *[16]u8) UringCmd {
    std.mem.writeInt(u64, buf[0..8], crkey, .little);
    std.mem.writeInt(u64, buf[8..16], prkey, .little);
    var c = UringCmd.zeroed();
    c.opcode = OP_RESV_ACQUIRE;
    c.nsid = nsid;
    c.cdw10 = racqa | (@as(u32, rtype) << 8);
    c.addr = @intFromPtr(buf);
    c.data_len = 16;
    return c;
}

/// The 8 B release payload `crkey`.
pub fn reservationRelease(nsid: u32, rtype: u8, crkey: u64, buf: *[8]u8) UringCmd {
    std.mem.writeInt(u64, buf, crkey, .little);
    var c = UringCmd.zeroed();
    c.opcode = OP_RESV_RELEASE;
    c.nsid = nsid;
    c.cdw10 = RRELA_RELEASE | (@as(u32, rtype) << 8);
    c.addr = @intFromPtr(buf);
    c.data_len = 8;
    return c;
}

// ---------------------------------------------------------------------------
// Status.
// ---------------------------------------------------------------------------

/// A completed command's NVMe status (`SCT ‖ SC`, the CQE `res` masked to
/// 11 bits) — 0 is success.
pub const Status = u16;

pub const STATUS_SUCCESS: Status = 0;
/// Generic: Invalid Field in Command (the register-shape probe, quirk Q11).
pub const SC_INVALID_FIELD: Status = 0x02;
/// Generic: Host Identifier Inconsistent Format (nvmet's answer to a
/// short-form report on a fabrics association) — why EDS = 1 goes first.
pub const SC_HOSTID_INCONSISTENT: Status = 0x18;
/// Generic: Namespace Not Ready (transient).
pub const SC_NAMESPACE_NOT_READY: Status = 0x82;
/// Generic: Reservation Conflict — the fence.
pub const SC_RESERVATION_CONFLICT: Status = 0x83;

pub fn statusOf(cqe_res: i32) Status {
    if (cqe_res <= 0) return 0;
    return @intCast(@as(u32, @intCast(cqe_res)) & 0x7ff);
}

pub fn isConflict(s: Status) bool {
    return s == SC_RESERVATION_CONFLICT;
}

/// Transient statuses (R:313-351): aborts 0x07-0x0a, 0x21, 0x22, Namespace
/// Not Ready, any path-related status (SCT 3).
pub fn isTransient(s: Status) bool {
    return switch (s) {
        0x07...0x0a, 0x21, 0x22, SC_NAMESPACE_NOT_READY => true,
        else => (s & 0x700) == 0x300,
    };
}

/// Device-answered command-specific (SCT 1) or vendor-specific (SCT 7)
/// status that is not transient: the ONLY class a registrant cap may be
/// LEARNED from (charter §5.5).
pub fn isCapLearnable(s: Status) bool {
    if (isTransient(s)) return false;
    const sct = s & 0x700;
    return sct == 0x100 or sct == 0x700;
}

// ---------------------------------------------------------------------------
// Reservation Report (NVMe Base Spec §5.x; byte-wise verified against
// nvmet, R:142-147): header `gen u32 @0, rtype u8 @4, regctl u16 @5,
// rsvd @7..9, ptpls u8 @9`; registrants at `hdr + i × stride` with
// `cntlid u16 @0, rcsts u8 @2`; extended `rkey u64 @8, hostid[16] @16`;
// short `hostid u64 @8, rkey u64 @16`. The stride equals the header length
// in both forms.
// ---------------------------------------------------------------------------

pub const REPORT_HEADER_EXTENDED: usize = 64;
pub const REPORT_HEADER_SHORT: usize = 24;
pub const PTPLS_OFFSET: usize = 9;

pub const Registrant = struct {
    cntlid: u16,
    /// `rcsts & 1`: this registrant holds the reservation.
    holds: bool,
    rkey: u64,
    /// 16 B in the extended form; the short form's u64 sits in the low 8
    /// bytes with the high 8 zero.
    host_id: [16]u8,
};

pub const Report = struct {
    generation: u32,
    rtype: u8,
    regctl: u16,
    ptpls: u8,
    extended: bool,
    /// Decoded registrants (those the image holds; a header naming more
    /// than fit decodes the ones that fit — total).
    registrants: []Registrant,

    pub fn holder(self: Report) ?u64 {
        if (self.rtype == 0) return null;
        for (self.registrants) |r| if (r.holds) return r.rkey;
        return null;
    }

    pub fn deinit(self: *Report, gpa: std.mem.Allocator) void {
        gpa.free(self.registrants);
        self.* = undefined;
    }
};

pub fn reportHeaderLen(extended: bool) usize {
    return if (extended) REPORT_HEADER_EXTENDED else REPORT_HEADER_SHORT;
}

/// The bytes a full report of `regctl` registrants needs.
pub fn reportLen(extended: bool, regctl: u16) usize {
    const h = reportHeaderLen(extended);
    return h + h * @as(usize, regctl);
}

/// `regctl` from a header-only image (null when the image is shorter than
/// the header).
pub fn reportRegctl(image: []const u8, extended: bool) ?u16 {
    if (image.len < reportHeaderLen(extended)) return null;
    return std.mem.readInt(u16, image[5..7], .little);
}

/// Total parse: a short image refuses; registrants past the image's end are
/// dropped (the header's count is never an allocation authority).
pub fn parseReport(gpa: std.mem.Allocator, image: []const u8, extended: bool) error{ Corrupt, OutOfMemory }!Report {
    const h = reportHeaderLen(extended);
    if (image.len < h) return error.Corrupt;
    const regctl = std.mem.readInt(u16, image[5..7], .little);
    const fit: usize = @min(@as(usize, regctl), (image.len - h) / h);
    const regs = try gpa.alloc(Registrant, fit);
    errdefer gpa.free(regs);
    for (regs, 0..) |*r, i| {
        const at = image[h + i * h ..][0..h];
        var host: [16]u8 = @splat(0);
        var rkey: u64 = undefined;
        if (extended) {
            rkey = std.mem.readInt(u64, at[8..16], .little);
            host = at[16..32].*;
        } else {
            @memcpy(host[0..8], at[8..16]);
            rkey = std.mem.readInt(u64, at[16..24], .little);
        }
        r.* = .{ .cntlid = std.mem.readInt(u16, at[0..2], .little), .holds = at[2] & 1 != 0, .rkey = rkey, .host_id = host };
    }
    return .{ .generation = std.mem.readInt(u32, image[0..4], .little), .rtype = image[4], .regctl = regctl, .ptpls = image[PTPLS_OFFSET], .extended = extended, .registrants = regs };
}

/// The encoder the fake namespace answers with (and the tests fixture):
/// writes as many registrants as `out` holds.
pub fn encodeReport(out: []u8, extended: bool, generation: u32, rtype: u8, ptpls: u8, regs: []const Registrant) void {
    @memset(out, 0);
    const h = reportHeaderLen(extended);
    if (out.len < h) return;
    std.mem.writeInt(u32, out[0..4], generation, .little);
    out[4] = rtype;
    std.mem.writeInt(u16, out[5..7], @intCast(regs.len), .little);
    out[PTPLS_OFFSET] = ptpls;
    for (regs, 0..) |r, i| {
        const off = h + i * h;
        if (off + h > out.len) break;
        const at = out[off..][0..h];
        std.mem.writeInt(u16, at[0..2], r.cntlid, .little);
        at[2] = if (r.holds) 1 else 0;
        if (extended) {
            std.mem.writeInt(u64, at[8..16], r.rkey, .little);
            at[16..32].* = r.host_id;
        } else {
            @memcpy(at[8..16], r.host_id[0..8]);
            std.mem.writeInt(u64, at[16..24], r.rkey, .little);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the 72 B command sits at the head of the 80 B area zero-padded; every builder encodes its opcode, nsid, CDW10/11 and LE payload byte-exactly" {
    var payload: [16]u8 = undefined;
    const reg = reservationRegister(7, RREGA_REGISTER | IEKEY | CPTPL_PERSIST, 0, 0x1122_3344_5566_7788, &payload);
    const area = reg.toCmdArea();
    try testing.expectEqual(OP_RESV_REGISTER, area[0]);
    try testing.expectEqual(@as(u32, 7), std.mem.readInt(u32, area[4..8], .little));
    try testing.expectEqual(@as(u32, 0xC000_0008), std.mem.readInt(u32, area[40..44], .little));
    try testing.expectEqual(@as(u32, 16), std.mem.readInt(u32, area[36..40], .little));
    try testing.expectEqual(@intFromPtr(&payload), std.mem.readInt(u64, area[24..32], .little));
    try testing.expect(std.mem.allEqual(u8, area[72..80], 0));
    try testing.expectEqualSlices(u8, &[_]u8{ 0, 0, 0, 0, 0, 0, 0, 0, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11 }, &payload);
    const back = UringCmd.fromBytes(area[0..CMD_LEN]);
    try testing.expectEqual(reg, back);

    var idbuf: [IDENTIFY_NS_LEN]u8 = undefined;
    const id = identifyNamespace(3, &idbuf);
    try testing.expectEqual(OP_ADMIN_IDENTIFY, id.opcode);
    try testing.expectEqual(@as(u32, 0), id.cdw10);
    try testing.expectEqual(@as(u32, 4096), id.data_len);

    var hid: [16]u8 = undefined;
    const gf = getHostIdentifier(&hid);
    try testing.expectEqual(OP_ADMIN_GET_FEATURES, gf.opcode);
    try testing.expectEqual(@as(u32, 0), gf.nsid);
    try testing.expectEqual(FID_HOST_IDENTIFIER, gf.cdw10);
    try testing.expectEqual(@as(u32, 1), gf.cdw11);
    var hid8: [8]u8 = undefined;
    try testing.expectEqual(@as(u32, 0), getHostIdentifier(&hid8).cdw11);

    var rep: [64]u8 = undefined;
    const rp = reservationReport(3, &rep, true);
    try testing.expectEqual(@as(u32, 15), rp.cdw10);
    try testing.expectEqual(EDS, rp.cdw11);
    try testing.expectEqual(@as(u32, 0), reservationReport(3, &rep, false).cdw11);

    const acq = reservationAcquire(3, RACQA_PREEMPT, RTYPE_WRITE_EXCLUSIVE, 0xa, 0xb, &payload);
    try testing.expectEqual(OP_RESV_ACQUIRE, acq.opcode);
    try testing.expectEqual(@as(u32, 0x101), acq.cdw10);
    try testing.expectEqual(@as(u64, 0xa), std.mem.readInt(u64, payload[0..8], .little));
    try testing.expectEqual(@as(u64, 0xb), std.mem.readInt(u64, payload[8..16], .little));

    var rel_payload: [8]u8 = undefined;
    const rel = reservationRelease(3, RTYPE_WERO, 0xc, &rel_payload);
    try testing.expectEqual(OP_RESV_RELEASE, rel.opcode);
    try testing.expectEqual(@as(u32, 0x300), rel.cdw10);
    try testing.expectEqual(@as(u32, 8), rel.data_len);
}

test "status: conflict is 0x83 masked to 11 bits; transient and cap-learnable classes; a negative res is a transport errno, not a status" {
    try testing.expectEqual(SC_RESERVATION_CONFLICT, statusOf(0x83));
    try testing.expectEqual(SC_RESERVATION_CONFLICT, statusOf(0x1_0083));
    try testing.expectEqual(STATUS_SUCCESS, statusOf(0));
    try testing.expectEqual(STATUS_SUCCESS, statusOf(-52));
    try testing.expect(isConflict(0x83));
    try testing.expect(!isConflict(0x82));
    try testing.expect(isTransient(0x07));
    try testing.expect(isTransient(0x0a));
    try testing.expect(isTransient(0x21));
    try testing.expect(isTransient(0x82));
    try testing.expect(isTransient(0x301));
    try testing.expect(!isTransient(0x83));
    try testing.expect(isCapLearnable(0x1a0));
    try testing.expect(isCapLearnable(0x7ff));
    try testing.expect(!isCapLearnable(0x002));
    try testing.expect(!isCapLearnable(0x301));
    try testing.expect(!isCapLearnable(0x083));
}

test "report: both forms round-trip through encode/parse; the holder is the rcsts registrant under a non-zero rtype; a short image refuses; a header naming more than the image holds decodes the ones that fit" {
    const gpa = testing.allocator;
    const regs = [_]Registrant{
        .{ .cntlid = 1, .holds = false, .rkey = 0xaaaa, .host_id = "host-a-host-a-01".* },
        .{ .cntlid = 2, .holds = true, .rkey = 0xbbbb, .host_id = "host-b-host-b-02".* },
    };
    inline for (.{ true, false }) |ext| {
        var img: [64 * 3]u8 = undefined;
        const len = reportLen(ext, 2);
        encodeReport(img[0..len], ext, 9, RTYPE_WRITE_EXCLUSIVE, 1, &regs);
        try testing.expectEqual(@as(?u16, 2), reportRegctl(img[0..reportHeaderLen(ext)], ext));
        var r = try parseReport(gpa, img[0..len], ext);
        defer r.deinit(gpa);
        try testing.expectEqual(@as(u32, 9), r.generation);
        try testing.expectEqual(@as(u16, 2), r.regctl);
        try testing.expectEqual(@as(u8, 1), r.ptpls);
        try testing.expectEqual(@as(?u64, 0xbbbb), r.holder());
        try testing.expectEqual(@as(usize, 2), r.registrants.len);
        try testing.expectEqual(@as(u64, 0xaaaa), r.registrants[0].rkey);
        if (ext) {
            try testing.expectEqualSlices(u8, "host-b-host-b-02", &r.registrants[1].host_id);
        } else {
            try testing.expectEqualSlices(u8, "host-b-h", r.registrants[1].host_id[0..8]);
            try testing.expect(std.mem.allEqual(u8, r.registrants[1].host_id[8..], 0));
        }
        // rtype 0 ⇒ no holder regardless of rcsts.
        var none = img;
        none[4] = 0;
        var r0 = try parseReport(gpa, none[0..len], ext);
        defer r0.deinit(gpa);
        try testing.expectEqual(@as(?u64, null), r0.holder());
        // Truncated below the header: refuse; header says 2 but one fits: 1.
        try testing.expectError(error.Corrupt, parseReport(gpa, img[0 .. reportHeaderLen(ext) - 1], ext));
        var r1 = try parseReport(gpa, img[0 .. reportHeaderLen(ext) * 2], ext);
        defer r1.deinit(gpa);
        try testing.expectEqual(@as(u16, 2), r1.regctl);
        try testing.expectEqual(@as(usize, 1), r1.registrants.len);
    }
    // 64+ registrants decode completely at the REGCTL-sized length (the
    // 63rd-registrant truncation of a fixed 4 KiB read, TS:405).
    var many: [70]Registrant = undefined;
    for (&many, 0..) |*m, i| m.* = .{ .cntlid = @intCast(i), .holds = i == 69, .rkey = 1000 + i, .host_id = @splat(@intCast(i)) };
    const big_len = reportLen(true, 70);
    try testing.expect(big_len > 4096);
    const big = try gpa.alloc(u8, big_len);
    defer gpa.free(big);
    encodeReport(big, true, 1, RTYPE_WERO, 0, &many);
    var rb = try parseReport(gpa, big, true);
    defer rb.deinit(gpa);
    try testing.expectEqual(@as(usize, 70), rb.registrants.len);
    try testing.expectEqual(@as(?u64, 1069), rb.holder());
    var r4k = try parseReport(gpa, big[0..4096], true);
    defer r4k.deinit(gpa);
    try testing.expectEqual(@as(usize, 63), r4k.registrants.len);
}
