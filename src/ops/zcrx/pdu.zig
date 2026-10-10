//! NVMe/TCP 1.0 PDU codecs for the read-only host association (charter
//! §6.3; `docs/porting/ops_zcrx.md` §2.1). Little-endian throughout.
//!
//! Read-only by construction: the ENCODER set is exactly {ICReq, Connect,
//! Property Get/Set, Read}; no H2CData, write, flush or reservation builder
//! exists, so D0/PR/fencing surfaces are unreachable from this module.
//! Every decoder is total: arbitrary bytes yield a typed verdict, never a
//! panic (`fuzz` below). Quirks taken (decision 0002): the CH digest flag
//! bits are CHECKED (QUIRK 3 — a target that sends digests after
//! negotiating them off is malformed) and the only IO-queue PDU headers
//! accepted are exactly 24 B (QUIRK 2; `pdu_stream.zig`).
//!
//! `extern struct`s with offset pins are the wire images; the SQE is kept
//! as 64 raw bytes with named setters because the NVMe SQE is a union of
//! command-specific dwords.

const std = @import("std");
const fuzzkit = @import("../../core/fuzzkit.zig");

pub const Type = enum(u8) {
    icreq = 0x00,
    icresp = 0x01,
    h2c_term = 0x02,
    c2h_term = 0x03,
    capsule_cmd = 0x04,
    capsule_resp = 0x05,
    h2c_data = 0x06,
    c2h_data = 0x07,
    r2t = 0x09,
    _,
};

/// C2HData flags.
pub const C2H_LAST: u8 = 1 << 2;
pub const C2H_SUCCESS: u8 = 1 << 3;
/// CH flag bits 0/1: HDGSTF/DDGSTF — a digest the host never negotiated.
pub const CH_DIGEST_BITS: u8 = 0x3;

/// Common header, every PDU.
pub const CommonHeader = extern struct {
    type: u8,
    flags: u8,
    hlen: u8,
    pdo: u8,
    plen: u32,

    pub const len = 8;

    pub fn decode(b: *const [8]u8) CommonHeader {
        return .{ .type = b[0], .flags = b[1], .hlen = b[2], .pdo = b[3], .plen = std.mem.readInt(u32, b[4..8], .little) };
    }

    pub fn encode(self: CommonHeader, b: *[8]u8) void {
        b[0] = self.type;
        b[1] = self.flags;
        b[2] = self.hlen;
        b[3] = self.pdo;
        std.mem.writeInt(u32, b[4..8], self.plen, .little);
    }
};

comptime {
    std.debug.assert(@sizeOf(CommonHeader) == 8);
    std.debug.assert(@offsetOf(CommonHeader, "plen") == 4);
}

// ---------------------------------------------------------------------------
// ICReq / ICResp (128 B each).
// ---------------------------------------------------------------------------

pub const IC_LEN: usize = 128;

/// ICReq (host → controller): PFV 0, HPDA 0, digests OFF, MAXR2T 0 (no
/// writes ⇒ no R2T), the rest zero.
pub const ICReq = extern struct {
    ch: CommonHeader = .{ .type = @backingInt(Type.icreq), .flags = 0, .hlen = IC_LEN, .pdo = 0, .plen = IC_LEN },
    pfv: u16 = 0,
    hpda: u8 = 0,
    digest: u8 = 0,
    maxr2t: u32 = 0,
    resv: [112]u8 = @splat(0),

    pub fn encode(b: *[IC_LEN]u8) void {
        const req: ICReq = .{};
        @memcpy(b, std.mem.asBytes(&req));
    }
};

pub const ICResp = extern struct {
    ch: CommonHeader,
    pfv: u16,
    cpda: u8,
    digest: u8,
    maxh2cdata: u32,
    resv: [112]u8,
};

comptime {
    std.debug.assert(@sizeOf(ICReq) == IC_LEN);
    std.debug.assert(@sizeOf(ICResp) == IC_LEN);
    std.debug.assert(@offsetOf(ICReq, "pfv") == 8);
    std.debug.assert(@offsetOf(ICReq, "hpda") == 10);
    std.debug.assert(@offsetOf(ICReq, "digest") == 11);
    std.debug.assert(@offsetOf(ICReq, "maxr2t") == 12);
    std.debug.assert(@offsetOf(ICResp, "maxh2cdata") == 12);
}

pub const ICRespError = error{
    /// Fewer than 128 bytes.
    Short,
    /// Not an ICResp.
    UnexpectedType,
    /// `pfv != 0`, or the controller demands a digest: never silently
    /// accept a per-byte CRC pass.
    Negotiation,
};

pub const Negotiated = struct { cpda: u8, maxh2cdata: u32 };

/// The negotiation laws: type 0x01, PFV 0, digests off.
pub fn parseICResp(b: []const u8) ICRespError!Negotiated {
    if (b.len < IC_LEN) return error.Short;
    const ch = CommonHeader.decode(b[0..8]);
    if (ch.type != @backingInt(Type.icresp)) return error.UnexpectedType;
    const pfv = std.mem.readInt(u16, b[8..10], .little);
    const digest = b[11];
    if (pfv != 0 or digest != 0) return error.Negotiation;
    return .{ .cpda = b[10], .maxh2cdata = std.mem.readInt(u32, b[12..16], .little) };
}

// ---------------------------------------------------------------------------
// CapsuleCmd: CH ‖ 64 B SQE ‖ optional in-capsule data.
// ---------------------------------------------------------------------------

pub const SQE_LEN: usize = 64;
pub const CAPSULE_HDR_LEN: usize = CommonHeader.len + SQE_LEN;
/// `sizeof(struct nvmf_connect_data)`: nvmet's `nvmet_check_transfer_len`
/// refuses any other in-capsule length with `0x400f` (Data SGL Length
/// Invalid ‖ DNR) — the 2026-08-04 field arm refusal (a 4096 B blob).
pub const CONNECT_DATA_LEN: usize = 1024;
pub const CONNECT_CAPSULE_LEN: usize = CAPSULE_HDR_LEN + CONNECT_DATA_LEN;

pub const OPC_FABRICS: u8 = 0x7F;
pub const OPC_READ: u8 = 0x02;
pub const FCTYPE_PROPERTY_SET: u8 = 0x00;
pub const FCTYPE_CONNECT: u8 = 0x01;
pub const FCTYPE_PROPERTY_GET: u8 = 0x04;
/// PSDT = 01b: SGLs are used for this command (NVMe-oF requires it).
pub const PSDT_SGL: u8 = 0x40;
/// SGL Data Block, offset form (in-capsule data): type 0, subtype 1.
pub const SGL_IN_CAPSULE: u8 = 0x01;
/// Transport SGL Data Block: type 5, subtype 0xA — data rides C2HData.
pub const SGL_TRANSPORT: u8 = 0x5A;

/// Property offsets (NVMe base spec §3.1).
pub const PROP_CAP: u32 = 0x00;
pub const PROP_CC: u32 = 0x14;
pub const PROP_CSTS: u32 = 0x1C;
/// CC: EN ‖ IOSQES 6 (64 B) ‖ IOCQES 4 (16 B); CSS 0 = NVM.
pub const CC_ENABLE_NVM: u32 = 1 | (6 << 16) | (4 << 20);
pub const CSTS_RDY: u32 = 1 << 0;
pub const CSTS_CFS: u32 = 1 << 1;
/// Property Get/Set `attrib`: 8-byte access (CAP) vs 4-byte (CC, CSTS).
/// nvmet refuses an 8-byte Set, and an 8-byte Get of anything but CAP,
/// with Invalid Field ‖ DNR — the attribute is the register's width.
pub const ATTRIB_4B: u8 = 0;
pub const ATTRIB_8B: u8 = 1;
/// `NVME_AQ_DEPTH` (32) − 1: the admin queue's 0-based sqsize, bring-up
/// only (QUIRK 4 — the literal carries its parity).
pub const ADMIN_SQSIZE0: u16 = 31;
pub const CNTLID_DYNAMIC: u16 = 0xFFFF;

/// A capsule: the common header, the SQE and up to the Connect data.
pub const Capsule = struct {
    buf: [CONNECT_CAPSULE_LEN]u8 = @splat(0),
    len: usize = 0,

    pub fn bytes(self: *const Capsule) []const u8 {
        return self.buf[0..self.len];
    }

    fn sqe(self: *Capsule) *[SQE_LEN]u8 {
        return self.buf[CommonHeader.len..][0..SQE_LEN];
    }

    fn scaffold(self: *Capsule, opc: u8, cid: u16, plen: u32, pdo: u8) void {
        self.buf = @splat(0);
        self.len = plen;
        (CommonHeader{ .type = @backingInt(Type.capsule_cmd), .flags = 0, .hlen = CAPSULE_HDR_LEN, .pdo = pdo, .plen = plen }).encode(self.buf[0..8]);
        const s = self.sqe();
        s[0] = opc;
        s[1] = PSDT_SGL;
        std.mem.writeInt(u16, s[2..4], cid, .little);
    }

    fn sglInCapsule(self: *Capsule, len: u32) void {
        const s = self.sqe();
        std.mem.writeInt(u64, s[24..32], 0, .little);
        std.mem.writeInt(u32, s[32..36], len, .little);
        s[39] = SGL_IN_CAPSULE;
    }

    fn sglTransport(self: *Capsule, len: u32) void {
        const s = self.sqe();
        std.mem.writeInt(u32, s[32..36], len, .little);
        s[39] = SGL_TRANSPORT;
    }
};

pub const ConnectParams = struct {
    cid: u16,
    qid: u16,
    /// 0-based queue size.
    sqsize0: u16,
    /// 0 = same-host-managed, fails loud on TCP errors.
    kato: u32,
    hostid: [16]u8,
    /// `CNTLID_DYNAMIC` on the admin queue; the assigned id on IO queues.
    cntlid: u16,
    subsysnqn: []const u8,
    hostnqn: []const u8,
};

/// Fabrics Connect: the 72 B capsule header + the 1024 B `nvmf_connect_data`.
pub fn encodeConnect(p: ConnectParams) Capsule {
    std.debug.assert(p.subsysnqn.len <= 256 and p.hostnqn.len <= 256);
    var c: Capsule = .{};
    c.scaffold(OPC_FABRICS, p.cid, CONNECT_CAPSULE_LEN, CAPSULE_HDR_LEN);
    const s = c.sqe();
    s[4] = FCTYPE_CONNECT;
    c.sglInCapsule(CONNECT_DATA_LEN);
    std.mem.writeInt(u16, s[40..42], 0, .little); // recfmt
    std.mem.writeInt(u16, s[42..44], p.qid, .little);
    std.mem.writeInt(u16, s[44..46], p.sqsize0, .little);
    std.mem.writeInt(u16, s[46..48], 0, .little); // cattr
    std.mem.writeInt(u32, s[48..52], p.kato, .little);
    const d = c.buf[CAPSULE_HDR_LEN..][0..CONNECT_DATA_LEN];
    @memcpy(d[0..16], &p.hostid);
    std.mem.writeInt(u16, d[16..18], p.cntlid, .little);
    @memcpy(d[256..][0..p.subsysnqn.len], p.subsysnqn);
    @memcpy(d[512..][0..p.hostnqn.len], p.hostnqn);
    return c;
}

/// Property Get at `offset` with the register's width (`attrib`).
pub fn encodePropertyGet(cid: u16, offset: u32, attrib: u8) Capsule {
    var c: Capsule = .{};
    c.scaffold(OPC_FABRICS, cid, CAPSULE_HDR_LEN, 0);
    const s = c.sqe();
    s[4] = FCTYPE_PROPERTY_GET;
    s[40] = attrib;
    std.mem.writeInt(u32, s[44..48], offset, .little);
    return c;
}

/// Property Set of a 4-byte register (CC is the only one the lane sets).
pub fn encodePropertySet(cid: u16, offset: u32, value: u32) Capsule {
    var c: Capsule = .{};
    c.scaffold(OPC_FABRICS, cid, CAPSULE_HDR_LEN, 0);
    const s = c.sqe();
    s[4] = FCTYPE_PROPERTY_SET;
    s[40] = ATTRIB_4B;
    std.mem.writeInt(u32, s[44..48], offset, .little);
    std.mem.writeInt(u64, s[48..56], value, .little);
    return c;
}

/// NVM Read of `nlb` blocks (1 ≤ nlb ≤ 65536) at `slba`, `xfer_len` bytes
/// over a Transport SGL (the data rides C2HData).
pub fn encodeRead(cid: u16, nsid: u32, slba: u64, nlb: u32, xfer_len: u32) Capsule {
    std.debug.assert(nlb >= 1 and nlb <= 65536);
    var c: Capsule = .{};
    c.scaffold(OPC_READ, cid, CAPSULE_HDR_LEN, 0);
    const s = c.sqe();
    std.mem.writeInt(u32, s[4..8], nsid, .little);
    c.sglTransport(xfer_len);
    std.mem.writeInt(u64, s[40..48], slba, .little);
    std.mem.writeInt(u16, s[48..50], @intCast(nlb - 1), .little);
    return c;
}

// ---------------------------------------------------------------------------
// CapsuleResp (24 B): CH ‖ 16 B CQE.
// ---------------------------------------------------------------------------

pub const CAPSULE_RESP_LEN: usize = 24;

pub const Cqe = struct {
    dw0: u32,
    dw1: u32,
    sqhd: u16,
    sqid: u16,
    cid: u16,
    /// Status Field with the phase bit STRIPPED (transport-reserved on
    /// NVMe-oF): `SCT = bits 10:8`, `SC = bits 7:0`, MORE 13, DNR 14.
    status: u16,

    pub fn ok(self: Cqe) bool {
        return self.status == 0;
    }
};

/// The 16 CQE bytes (the PSH of a CapsuleResp, `b[8..24]` of the PDU).
pub fn parseCqe(b: *const [16]u8) Cqe {
    return .{
        .dw0 = std.mem.readInt(u32, b[0..4], .little),
        .dw1 = std.mem.readInt(u32, b[4..8], .little),
        .sqhd = std.mem.readInt(u16, b[8..10], .little),
        .sqid = std.mem.readInt(u16, b[10..12], .little),
        .cid = std.mem.readInt(u16, b[12..14], .little),
        .status = std.mem.readInt(u16, b[14..16], .little) >> 1,
    };
}

pub const CapsuleRespError = error{ Short, UnexpectedType, BadHeaderLength };

pub fn parseCapsuleResp(b: []const u8) CapsuleRespError!Cqe {
    if (b.len < CAPSULE_RESP_LEN) return error.Short;
    const ch = CommonHeader.decode(b[0..8]);
    if (ch.type != @backingInt(Type.capsule_resp)) return error.UnexpectedType;
    if (ch.hlen != CAPSULE_RESP_LEN or ch.plen != CAPSULE_RESP_LEN) return error.BadHeaderLength;
    return parseCqe(b[8..24]);
}

/// Status rendering: SCT/SC/MORE/DNR with names only for codes this
/// surface can meet; honest null otherwise (a raw hex cost a field
/// session — `0x400f` named nothing).
pub const Status = struct {
    sf: u16,
    sct: u3,
    sc: u8,
    more: bool,
    dnr: bool,

    pub fn describe(sf: u16) Status {
        return .{ .sf = sf, .sct = @intCast((sf >> 8) & 0x7), .sc = @truncate(sf), .more = sf & (1 << 13) != 0, .dnr = sf & (1 << 14) != 0 };
    }

    pub fn name(self: Status) ?[]const u8 {
        return switch (self.sct) {
            0 => switch (self.sc) {
                0x00 => "Successful Completion",
                0x01 => "Invalid Command Opcode",
                0x02 => "Invalid Field in Command",
                0x03 => "Command ID Conflict",
                0x04 => "Data Transfer Error",
                0x05 => "Commands Aborted due to Power Loss Notification",
                0x06 => "Internal Error",
                0x07 => "Command Abort Requested",
                0x08 => "Command Aborted due to SQ Deletion",
                0x09 => "Command Aborted due to Failed Fused Command",
                0x0A => "Command Aborted due to Missing Fused Command",
                0x0B => "Invalid Namespace or Format",
                0x0C => "Command Sequence Error",
                0x0D => "Invalid SGL Segment Descriptor",
                0x0E => "Invalid Number of SGL Descriptors",
                0x0F => "Data SGL Length Invalid",
                0x10 => "Metadata SGL Length Invalid",
                0x11 => "SGL Descriptor Type Invalid",
                0x80 => "LBA Out of Range",
                0x81 => "Capacity Exceeded",
                0x82 => "Namespace Not Ready",
                else => null,
            },
            1 => switch (self.sc) {
                0x80 => "Connect Incompatible Format",
                0x81 => "Connect Controller Busy",
                0x82 => "Connect Invalid Parameters",
                0x83 => "Connect Restart Discovery",
                0x84 => "Connect Invalid Host",
                else => null,
            },
            2 => switch (self.sc) {
                0x81 => "Unrecovered Read Error",
                else => null,
            },
            else => null,
        };
    }

    pub fn format(self: Status, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.print("status 0x{x:0>4} (sct {d} sc 0x{x:0>2}{s}{s})", .{ self.sf, self.sct, self.sc, if (self.more) " MORE" else "", if (self.dnr) " DNR" else "" });
        if (self.name()) |n| try w.print(" {s}", .{n});
    }
};

// ---------------------------------------------------------------------------
// C2HData header (24 B).
// ---------------------------------------------------------------------------

pub const C2H_HDR_LEN: usize = 24;

pub const C2HData = struct {
    cccid: u16,
    datao: u32,
    datal: u32,
    /// Bytes of PDO padding past the header.
    pad: u32,
    last: bool,
    success: bool,
};

pub const C2HError = error{
    UnexpectedType,
    /// `hlen != 24`.
    BadHeaderLength,
    /// A digest flag on a connection that negotiated digests off (QUIRK 3).
    DigestFlag,
    /// `pdo < hlen` or `plen != pdo + datal`: with digests off the payload
    /// runs exactly to the PDU end.
    Geometry,
};

/// The geometry law over the 24 header bytes.
pub fn parseC2HData(b: *const [C2H_HDR_LEN]u8) C2HError!C2HData {
    const ch = CommonHeader.decode(b[0..8]);
    if (ch.type != @backingInt(Type.c2h_data)) return error.UnexpectedType;
    if (ch.hlen != C2H_HDR_LEN) return error.BadHeaderLength;
    if (ch.flags & CH_DIGEST_BITS != 0) return error.DigestFlag;
    const datal = std.mem.readInt(u32, b[16..20], .little);
    if (ch.pdo < ch.hlen) return error.Geometry;
    const expect_plen = std.math.add(u32, ch.pdo, datal) catch return error.Geometry;
    if (ch.plen != expect_plen) return error.Geometry;
    return .{
        .cccid = std.mem.readInt(u16, b[8..10], .little),
        .datao = std.mem.readInt(u32, b[12..16], .little),
        .datal = datal,
        .pad = ch.pdo - ch.hlen,
        .last = ch.flags & C2H_LAST != 0,
        .success = ch.flags & C2H_SUCCESS != 0,
    };
}

/// Test/mock-target encoder of a C2HData header (the product never sends
/// one; it lives here so the parser tests and the mock speak one form).
pub fn encodeC2HDataHeader(b: *[C2H_HDR_LEN]u8, cccid: u16, datao: u32, datal: u32, pad: u8, last: bool, success: bool) void {
    @memset(b, 0);
    var flags: u8 = 0;
    if (last) flags |= C2H_LAST;
    if (success) flags |= C2H_SUCCESS;
    (CommonHeader{ .type = @backingInt(Type.c2h_data), .flags = flags, .hlen = C2H_HDR_LEN, .pdo = @intCast(C2H_HDR_LEN + @as(usize, pad)), .plen = @as(u32, C2H_HDR_LEN) + pad + datal }).encode(b[0..8]);
    std.mem.writeInt(u16, b[8..10], cccid, .little);
    std.mem.writeInt(u32, b[12..16], datao, .little);
    std.mem.writeInt(u32, b[16..20], datal, .little);
}

/// Test/mock-target encoder of a CapsuleResp.
pub fn encodeCapsuleResp(b: *[CAPSULE_RESP_LEN]u8, cid: u16, status_sf: u16, dw0: u32, dw1: u32, phase: bool) void {
    @memset(b, 0);
    (CommonHeader{ .type = @backingInt(Type.capsule_resp), .flags = 0, .hlen = CAPSULE_RESP_LEN, .pdo = 0, .plen = CAPSULE_RESP_LEN }).encode(b[0..8]);
    std.mem.writeInt(u32, b[8..12], dw0, .little);
    std.mem.writeInt(u32, b[12..16], dw1, .little);
    std.mem.writeInt(u16, b[20..22], cid, .little);
    std.mem.writeInt(u16, b[22..24], (status_sf << 1) | @intFromBool(phase), .little);
}

/// Test/mock-target encoder of an ICResp.
pub fn encodeICResp(b: *[IC_LEN]u8, pfv: u16, cpda: u8, digest: u8, maxh2cdata: u32) void {
    @memset(b, 0);
    (CommonHeader{ .type = @backingInt(Type.icresp), .flags = 0, .hlen = IC_LEN, .pdo = 0, .plen = IC_LEN }).encode(b[0..8]);
    std.mem.writeInt(u16, b[8..10], pfv, .little);
    b[10] = cpda;
    b[11] = digest;
    std.mem.writeInt(u32, b[12..16], maxh2cdata, .little);
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "ICReq: CH {0x00, 0, 128, 0, 128} and zeroed negotiation bytes (PFV 0, HPDA 0, digests off, MAXR2T 0)" {
    var b: [IC_LEN]u8 = undefined;
    ICReq.encode(&b);
    try testing.expectEqualSlices(u8, &.{ 0x00, 0x00, 128, 0x00, 128, 0, 0, 0 }, b[0..8]);
    for (b[8..]) |x| try testing.expectEqual(@as(u8, 0), x);
}

test "ICResp negotiation laws: digests demanded refuses, PFV ≠ 0 refuses, a wrong type and a short buffer refuse; the clean form decodes" {
    var b: [IC_LEN]u8 = undefined;
    encodeICResp(&b, 0, 0, 0, 0x20000);
    const n = try parseICResp(&b);
    try testing.expectEqual(@as(u32, 0x20000), n.maxh2cdata);
    encodeICResp(&b, 0, 0, 0x3, 0);
    try testing.expectError(error.Negotiation, parseICResp(&b));
    encodeICResp(&b, 1, 0, 0, 0);
    try testing.expectError(error.Negotiation, parseICResp(&b));
    encodeICResp(&b, 0, 0, 0, 0);
    b[0] = 0x05;
    try testing.expectError(error.UnexpectedType, parseICResp(&b));
    try testing.expectError(error.Short, parseICResp(b[0..127]));
}

test "Connect capsule layout: every byte incl. the 1024 B blob (hostid @0, cntlid @16, subsysnqn @256, hostnqn @512)" {
    var hostid: [16]u8 = undefined;
    for (&hostid, 0..) |*h, i| h.* = @intCast(0xA0 + i);
    const c = encodeConnect(.{ .cid = 0, .qid = 0, .sqsize0 = ADMIN_SQSIZE0, .kato = 0, .hostid = hostid, .cntlid = CNTLID_DYNAMIC, .subsysnqn = "nqn.2026-10.io.squeezefs:x", .hostnqn = "nqn.2014-08.org.nvmexpress:uuid:a0a0" });
    const b = c.bytes();
    try testing.expectEqual(CONNECT_CAPSULE_LEN, b.len);
    try testing.expectEqualSlices(u8, &.{ 0x04, 0x00, 72, 72 }, b[0..4]);
    try testing.expectEqual(@as(u32, 1096), std.mem.readInt(u32, b[4..8], .little));
    const s = b[8..72];
    try testing.expectEqual(OPC_FABRICS, s[0]);
    try testing.expectEqual(PSDT_SGL, s[1]);
    try testing.expectEqual(@as(u16, 0), std.mem.readInt(u16, s[2..4], .little));
    try testing.expectEqual(FCTYPE_CONNECT, s[4]);
    try testing.expectEqual(@as(u64, 0), std.mem.readInt(u64, s[24..32], .little));
    try testing.expectEqual(@as(u32, 1024), std.mem.readInt(u32, s[32..36], .little));
    try testing.expectEqual(SGL_IN_CAPSULE, s[39]);
    try testing.expectEqual(@as(u16, 0), std.mem.readInt(u16, s[40..42], .little));
    try testing.expectEqual(@as(u16, 0), std.mem.readInt(u16, s[42..44], .little));
    try testing.expectEqual(@as(u16, 31), std.mem.readInt(u16, s[44..46], .little));
    try testing.expectEqual(@as(u16, 0), std.mem.readInt(u16, s[46..48], .little));
    try testing.expectEqual(@as(u32, 0), std.mem.readInt(u32, s[48..52], .little));
    const d = b[72..];
    try testing.expectEqualSlices(u8, &hostid, d[0..16]);
    try testing.expectEqual(@as(u16, 0xFFFF), std.mem.readInt(u16, d[16..18], .little));
    for (d[18..256]) |x| try testing.expectEqual(@as(u8, 0), x);
    try testing.expectEqualStrings("nqn.2026-10.io.squeezefs:x", d[256..][0..26]);
    try testing.expectEqual(@as(u8, 0), d[256 + 26]);
    try testing.expectEqualStrings("nqn.2014-08.org.nvmexpress:uuid:a0a0", d[512..][0..36]);
    for (d[768..]) |x| try testing.expectEqual(@as(u8, 0), x);
    // An IO-queue connect carries the assigned cntlid and its qid/depth.
    const io = encodeConnect(.{ .cid = 0, .qid = 3, .sqsize0 = 15, .kato = 0, .hostid = hostid, .cntlid = 7, .subsysnqn = "a", .hostnqn = "b" });
    try testing.expectEqual(@as(u16, 3), std.mem.readInt(u16, io.buf[8 + 42 ..][0..2], .little));
    try testing.expectEqual(@as(u16, 15), std.mem.readInt(u16, io.buf[8 + 44 ..][0..2], .little));
    try testing.expectEqual(@as(u16, 7), std.mem.readInt(u16, io.buf[72 + 16 ..][0..2], .little));
}

test "Property Get/Set: 72 B capsule, pdo 0, fctype, the attrib is the register's width (CAP 8-byte, CC/CSTS 4-byte), offset, the Set value" {
    const g = encodePropertyGet(1, PROP_CAP, ATTRIB_8B);
    try testing.expectEqual(@as(usize, 72), g.len);
    try testing.expectEqualSlices(u8, &.{ 0x04, 0, 72, 0, 72, 0, 0, 0 }, g.buf[0..8]);
    try testing.expectEqual(FCTYPE_PROPERTY_GET, g.buf[8 + 4]);
    try testing.expectEqual(@as(u16, 1), std.mem.readInt(u16, g.buf[10..12], .little));
    try testing.expectEqual(ATTRIB_8B, g.buf[8 + 40]);
    try testing.expectEqual(@as(u32, 0), std.mem.readInt(u32, g.buf[8 + 44 ..][0..4], .little));
    try testing.expectEqual(ATTRIB_4B, encodePropertyGet(3, PROP_CSTS, ATTRIB_4B).buf[8 + 40]);
    const s = encodePropertySet(2, PROP_CC, CC_ENABLE_NVM);
    try testing.expectEqual(FCTYPE_PROPERTY_SET, s.buf[8 + 4]);
    try testing.expectEqual(ATTRIB_4B, s.buf[8 + 40]);
    try testing.expectEqual(@as(u32, 0x14), std.mem.readInt(u32, s.buf[8 + 44 ..][0..4], .little));
    try testing.expectEqual(@as(u64, 0x460001), std.mem.readInt(u64, s.buf[8 + 48 ..][0..8], .little));
    try testing.expectEqual(CC_ENABLE_NVM, 0x460001);
}

test "Read capsule layout: nsid @4, Transport SGL 0x5A with the xfer length, slba @40, nlb0 @48" {
    const r = encodeRead(0x1234, 1, 0x1_0000_0000, 256, 1 << 20);
    try testing.expectEqual(@as(usize, 72), r.len);
    const s = r.buf[8..72];
    try testing.expectEqual(OPC_READ, s[0]);
    try testing.expectEqual(PSDT_SGL, s[1]);
    try testing.expectEqual(@as(u16, 0x1234), std.mem.readInt(u16, s[2..4], .little));
    try testing.expectEqual(@as(u32, 1), std.mem.readInt(u32, s[4..8], .little));
    try testing.expectEqual(@as(u32, 1 << 20), std.mem.readInt(u32, s[32..36], .little));
    try testing.expectEqual(SGL_TRANSPORT, s[39]);
    try testing.expectEqual(@as(u64, 0x1_0000_0000), std.mem.readInt(u64, s[40..48], .little));
    try testing.expectEqual(@as(u16, 255), std.mem.readInt(u16, s[48..50], .little));
    const one = encodeRead(0, 1, 0, 65536, 1);
    try testing.expectEqual(@as(u16, 65535), std.mem.readInt(u16, one.buf[8 + 48 ..][0..2], .little));
}

test "CapsuleResp: the phase bit is stripped from the status; cid/sqhd/dw0 decode; the header laws refuse" {
    var b: [CAPSULE_RESP_LEN]u8 = undefined;
    encodeCapsuleResp(&b, 9, 0x400f, 0x1234, 0, true);
    const c = try parseCapsuleResp(&b);
    try testing.expectEqual(@as(u16, 9), c.cid);
    try testing.expectEqual(@as(u16, 0x400f), c.status);
    try testing.expectEqual(@as(u32, 0x1234), c.dw0);
    try testing.expect(!c.ok());
    encodeCapsuleResp(&b, 1, 0, 0, 0, false);
    try testing.expect((try parseCapsuleResp(&b)).ok());
    b[0] = 0x07;
    try testing.expectError(error.UnexpectedType, parseCapsuleResp(&b));
    b[0] = 0x05;
    b[2] = 72;
    try testing.expectError(error.BadHeaderLength, parseCapsuleResp(&b));
    try testing.expectError(error.Short, parseCapsuleResp(b[0..23]));
}

test "status decode names SCT/SC/DNR: 0x400f, 0x4182 and an unknown code stays honest" {
    var buf: [128]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    const a = Status.describe(0x400f);
    try testing.expectEqual(@as(u3, 0), a.sct);
    try testing.expectEqual(@as(u8, 0x0f), a.sc);
    try testing.expect(a.dnr and !a.more);
    try testing.expectEqualStrings("Data SGL Length Invalid", a.name().?);
    try w.print("{f}", .{a});
    try testing.expectEqualStrings("status 0x400f (sct 0 sc 0x0f DNR) Data SGL Length Invalid", w.buffered());
    const b = Status.describe(0x4182);
    try testing.expectEqual(@as(u3, 1), b.sct);
    try testing.expectEqualStrings("Connect Invalid Parameters", b.name().?);
    try testing.expectEqualStrings("Unrecovered Read Error", Status.describe(0x0281).name().?);
    try testing.expect(Status.describe(0x0777).name() == null);
    try testing.expect(Status.describe(0x2000).more);
}

test "C2HData geometry law: plen == pdo + datal with pdo ≥ hlen; padding derives; the digest bits, hlen and type refuse" {
    var b: [C2H_HDR_LEN]u8 = undefined;
    encodeC2HDataHeader(&b, 5, 4096, 8192, 8, true, true);
    const d = try parseC2HData(&b);
    try testing.expectEqual(@as(u16, 5), d.cccid);
    try testing.expectEqual(@as(u32, 4096), d.datao);
    try testing.expectEqual(@as(u32, 8192), d.datal);
    try testing.expectEqual(@as(u32, 8), d.pad);
    try testing.expect(d.last and d.success);
    // plen one short of the law.
    std.mem.writeInt(u32, b[4..8], 24 + 8 + 8192 - 1, .little);
    try testing.expectError(error.Geometry, parseC2HData(&b));
    encodeC2HDataHeader(&b, 5, 0, 16, 0, false, false);
    b[3] = 23; // pdo < hlen
    std.mem.writeInt(u32, b[4..8], 23 + 16, .little);
    try testing.expectError(error.Geometry, parseC2HData(&b));
    encodeC2HDataHeader(&b, 5, 0, 16, 0, false, false);
    b[1] |= 0x1;
    try testing.expectError(error.DigestFlag, parseC2HData(&b));
    encodeC2HDataHeader(&b, 5, 0, 16, 0, false, false);
    b[2] = 28;
    try testing.expectError(error.BadHeaderLength, parseC2HData(&b));
    b[2] = 24;
    b[0] = 0x06;
    try testing.expectError(error.UnexpectedType, parseC2HData(&b));
}

fn pduFuzz(_: void, smith: *std.testing.Smith) !void {
    var b: [IC_LEN]u8 = undefined;
    smith.bytes(&b);
    // Total: every decoder yields a verdict.
    _ = parseICResp(&b) catch {};
    _ = parseCapsuleResp(b[0..CAPSULE_RESP_LEN]) catch {};
    _ = parseC2HData(b[0..C2H_HDR_LEN]) catch {};
    _ = Status.describe(std.mem.readInt(u16, b[0..2], .little)).name();
    // Exactly the encoder's form decodes: a C2HData header the encoder
    // produced round-trips, and the decoded fields are the encoder's.
    const datao = std.mem.readInt(u32, b[0..4], .little);
    const datal = std.mem.readInt(u32, b[4..8], .little) & 0x00FF_FFFF;
    const pad: u8 = b[8] & 0x7F;
    const cccid = std.mem.readInt(u16, b[10..12], .little);
    var h: [C2H_HDR_LEN]u8 = undefined;
    encodeC2HDataHeader(&h, cccid, datao, datal, pad, b[12] & 1 != 0, b[12] & 2 != 0);
    const d = try parseC2HData(&h);
    try testing.expectEqual(cccid, d.cccid);
    try testing.expectEqual(datao, d.datao);
    try testing.expectEqual(datal, d.datal);
    try testing.expectEqual(@as(u32, pad), d.pad);
    var r: [CAPSULE_RESP_LEN]u8 = undefined;
    encodeCapsuleResp(&r, cccid, std.mem.readInt(u16, b[14..16], .little) & 0x7FFF, datao, datal, b[13] & 1 != 0);
    const c = try parseCapsuleResp(&r);
    try testing.expectEqual(cccid, c.cid);
    try testing.expectEqual(std.mem.readInt(u16, b[14..16], .little) & 0x7FFF, c.status);
}

test "fuzz: every PDU decoder is total on arbitrary bytes and exactly the encoder's form decodes" {
    try fuzzkit.fuzz({}, pduFuzz, .{});
}
