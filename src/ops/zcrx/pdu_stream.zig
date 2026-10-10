//! The push-based PDU stream parser of one IO queue (charter §6.3 "a
//! streaming PDU parser (headers ≤ 24 B copied to a stack scratch; payload
//! spans as `(chunk, off, len)` refs)"; `docs/porting/ops_zcrx.md` §5).
//! Portable, sync, one instance per queue, the same body under the
//! `RECV` venue leg and the `RECV_ZC` leg.
//!
//! The ONLY copy on the receive side is the header edge: ≤ 24 B per PDU
//! into `scratch` (priced: < 24 B per PDU ≈ 0.0015 passes/byte at 16 KiB
//! C2HData grains). Payload is never copied — it is emitted as a span
//! `(slot, off, len)` into the chunk the bytes already lie in; the CALLER
//! (the driver) takes the chunk ref when it records the span (the ledger
//! is the driver's), so the parser needs no allocator and no ledger.
//!
//! Laws: on an IO queue with digests negotiated off BOTH accepted PDU
//! types (CapsuleResp, C2HData) have `hlen == 24` exactly — any other
//! header length is a framing violation (QUIRK 2: the reference's 128 B
//! scratch and `hlen ∈ [8, 128]` were looser than the charter); the CQE of
//! a CapsuleResp IS its PSH (`scratch[8..24]`) — consuming past it
//! desynchronises the stream (the Z1 catch); H2CData / R2T / anything else
//! is an unexpected type (read-only by construction); PDO padding is
//! skipped by arithmetic, never copied or emitted. Headers may split at
//! ANY byte across chunk seams. Every `Error` is a FRAMING violation the
//! caller counts (`zcrx_frame_violations`) and poisons the session on —
//! resync is never attempted: the stream has no self-synchronising marker.

const std = @import("std");
const fuzzkit = @import("../../core/fuzzkit.zig");
const pdu = @import("pdu.zig");

/// THE scratch: exactly the IO-queue header form.
pub const SCRATCH_LEN: usize = 24;

/// A payload span: `len` bytes at ABSOLUTE area offset `off`, pinned by a
/// ref on ledger slot `slot` (the chunk under the sim leg; the span record
/// under the zcrx leg).
pub const Span = struct { slot: u32, off: u32, len: u32 };

pub const Event = union(enum) {
    c2h_span: struct { cid: u16, datao: u32, span: Span },
    c2h_end: struct { cid: u16, last: bool, success: bool },
    cqe: pdu.Cqe,
};

pub const Error = error{
    /// A PDU type other than CapsuleResp / C2HData on an IO queue.
    UnexpectedPduType,
    /// `hlen != 24` (or a CapsuleResp whose `plen != 24`).
    BadHeaderLength,
    /// A digest flag bit after digests were negotiated off.
    DigestFlag,
    /// The C2HData geometry law.
    Geometry,
    /// The sink (the fill table) refused the event: an unknown CID, a span
    /// past the fill's length, SUCCESS on a non-LAST PDU.
    Violation,
};

/// Where events go; the fill table in the product, a recorder in tests.
pub const Sink = struct {
    ctx: *anyopaque,
    emit: *const fn (ctx: *anyopaque, ev: Event) error{Violation}!void,
};

const State = union(enum) {
    header: struct { have: u8 },
    pad: struct { left: u32, cid: u16, datao: u32, datal: u32, last: bool, success: bool },
    payload: struct { left: u32, cid: u16, next_datao: u32, last: bool, success: bool },
};

pub const Parser = struct {
    state: State = .{ .header = .{ .have = 0 } },
    scratch: [SCRATCH_LEN]u8 = @splat(0),
    /// Header bytes copied so far (the driver drains it into
    /// `zcrx_hdr_copy_bytes` once per pass; a plain field keeps the parser
    /// instrument-free and portable).
    hdr_copy_bytes: u64 = 0,
    pdus: u64 = 0,

    pub fn reset(self: *Parser) void {
        self.state = .{ .header = .{ .have = 0 } };
    }

    pub fn idle(self: *const Parser) bool {
        return switch (self.state) {
            .header => |h| h.have == 0,
            else => false,
        };
    }

    /// Walk `bytes` (which lie at absolute area offset `base_off`, pinned by
    /// `slot`), emitting events in stream order. On an error the parser is
    /// undefined for further pushes: the caller poisons.
    pub fn push(self: *Parser, slot: u32, base_off: u32, bytes: []const u8, sink: Sink) Error!void {
        var pos: usize = 0;
        while (pos < bytes.len) {
            const remaining = bytes.len - pos;
            switch (self.state) {
                .header => |*h| {
                    // At 8 bytes the type and hlen are known: an early verdict
                    // before the rest of the header is even in flight.
                    const need: usize = if (h.have < pdu.CommonHeader.len) pdu.CommonHeader.len else SCRATCH_LEN;
                    const take = @min(need - h.have, remaining);
                    @memcpy(self.scratch[h.have..][0..take], bytes[pos..][0..take]);
                    h.have += @intCast(take);
                    pos += take;
                    self.hdr_copy_bytes += take;
                    if (h.have == pdu.CommonHeader.len) {
                        const ch = pdu.CommonHeader.decode(self.scratch[0..8]);
                        switch (ch.type) {
                            @backingInt(pdu.Type.capsule_resp), @backingInt(pdu.Type.c2h_data) => {},
                            else => return error.UnexpectedPduType,
                        }
                        if (ch.hlen != SCRATCH_LEN) return error.BadHeaderLength;
                    }
                    if (h.have == SCRATCH_LEN) try self.dispatchHeader(sink);
                },
                .pad => |*p| {
                    const take: u32 = @intCast(@min(p.left, remaining));
                    p.left -= take;
                    pos += take;
                    if (p.left == 0) {
                        const cid = p.cid;
                        const datao = p.datao;
                        const datal = p.datal;
                        const last = p.last;
                        const success = p.success;
                        try self.enterPayload(cid, datao, datal, last, success, sink);
                    }
                },
                .payload => |*p| {
                    const take: u32 = @intCast(@min(p.left, remaining));
                    try sink.emit(sink.ctx, .{ .c2h_span = .{ .cid = p.cid, .datao = p.next_datao, .span = .{ .slot = slot, .off = base_off + @as(u32, @intCast(pos)), .len = take } } });
                    p.left -= take;
                    p.next_datao += take;
                    pos += take;
                    if (p.left == 0) {
                        const ev: Event = .{ .c2h_end = .{ .cid = p.cid, .last = p.last, .success = p.success } };
                        self.pdus += 1;
                        self.state = .{ .header = .{ .have = 0 } };
                        try sink.emit(sink.ctx, ev);
                    }
                },
            }
        }
    }

    fn dispatchHeader(self: *Parser, sink: Sink) Error!void {
        const ch = pdu.CommonHeader.decode(self.scratch[0..8]);
        switch (ch.type) {
            @backingInt(pdu.Type.c2h_data) => {
                const d = pdu.parseC2HData(&self.scratch) catch |e| return switch (e) {
                    error.UnexpectedType => error.UnexpectedPduType,
                    error.BadHeaderLength => error.BadHeaderLength,
                    error.DigestFlag => error.DigestFlag,
                    error.Geometry => error.Geometry,
                };
                if (d.pad > 0) {
                    self.state = .{ .pad = .{ .left = d.pad, .cid = d.cccid, .datao = d.datao, .datal = d.datal, .last = d.last, .success = d.success } };
                } else {
                    try self.enterPayload(d.cccid, d.datao, d.datal, d.last, d.success, sink);
                }
            },
            @backingInt(pdu.Type.capsule_resp) => {
                if (ch.plen != pdu.CAPSULE_RESP_LEN) return error.BadHeaderLength;
                const cqe = pdu.parseCqe(self.scratch[8..24]);
                self.pdus += 1;
                self.state = .{ .header = .{ .have = 0 } };
                try sink.emit(sink.ctx, .{ .cqe = cqe });
            },
            else => return error.UnexpectedPduType,
        }
    }

    fn enterPayload(self: *Parser, cid: u16, datao: u32, datal: u32, last: bool, success: bool, sink: Sink) Error!void {
        if (datal == 0) {
            self.pdus += 1;
            self.state = .{ .header = .{ .have = 0 } };
            try sink.emit(sink.ctx, .{ .c2h_end = .{ .cid = cid, .last = last, .success = success } });
            return;
        }
        self.state = .{ .payload = .{ .left = datal, .cid = cid, .next_datao = datao, .last = last, .success = success } };
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

const Recorder = struct {
    events: std.ArrayList(Event) = .empty,
    gpa: std.mem.Allocator,

    fn sink(self: *Recorder) Sink {
        return .{ .ctx = self, .emit = &emit };
    }

    fn emit(ctx: *anyopaque, ev: Event) error{Violation}!void {
        const self: *Recorder = @ptrCast(@alignCast(ctx));
        self.events.append(self.gpa, ev) catch return error.Violation;
    }

    fn deinit(self: *Recorder) void {
        self.events.deinit(self.gpa);
    }
};

/// A synthetic stream: C2HData(cid 7, datao 0, 100 B, pad 4, LAST|SUCCESS) ‖
/// CapsuleResp(cid 8, status 0) ‖ C2HData(cid 9, datao 4096, 0 B, LAST) ‖
/// C2HData(cid 7, datao 200, 50 B, no flags).
fn synthStream(buf: []u8) usize {
    var n: usize = 0;
    pdu.encodeC2HDataHeader(buf[n..][0..24], 7, 0, 100, 4, true, true);
    n += 24 + 4;
    for (0..100) |i| buf[n + i] = @intCast(i);
    n += 100;
    pdu.encodeCapsuleResp(buf[n..][0..24], 8, 0, 0xCAFE, 0, true);
    n += 24;
    pdu.encodeC2HDataHeader(buf[n..][0..24], 9, 4096, 0, 0, true, false);
    n += 24;
    pdu.encodeC2HDataHeader(buf[n..][0..24], 7, 200, 50, 0, false, false);
    n += 24;
    for (0..50) |i| buf[n + i] = @intCast(200 + i);
    n += 50;
    return n;
}

/// Spans coalesce by (cid, contiguous datao) into the byte ranges they cover.
const Coverage = struct { cid: u16, datao: u32, len: u32 };

fn coverage(gpa: std.mem.Allocator, events: []const Event) ![]Coverage {
    var out: std.ArrayList(Coverage) = .empty;
    for (events) |ev| switch (ev) {
        .c2h_span => |s| {
            if (out.items.len > 0) {
                const last = &out.items[out.items.len - 1];
                if (last.cid == s.cid and last.datao + last.len == s.datao) {
                    last.len += s.span.len;
                    continue;
                }
            }
            try out.append(gpa, .{ .cid = s.cid, .datao = s.datao, .len = s.span.len });
        },
        else => {},
    };
    return out.toOwnedSlice(gpa);
}

test "the stream splits at EVERY byte (chunk grain 1..N) and yields the identical event sequence; header copy ≤ 24 B per PDU; padding is skipped" {
    const gpa = testing.allocator;
    var buf: [512]u8 = undefined;
    const n = synthStream(&buf);
    // The whole stream in one push: the reference sequence.
    var ref: Recorder = .{ .gpa = gpa };
    defer ref.deinit();
    var p0: Parser = .{};
    try p0.push(0, 0, buf[0..n], ref.sink());
    try testing.expect(p0.idle());
    try testing.expectEqual(@as(u64, 4), p0.pdus);
    try testing.expectEqual(@as(u64, 4 * 24), p0.hdr_copy_bytes);
    const ref_cov = try coverage(gpa, ref.events.items);
    defer gpa.free(ref_cov);
    try testing.expectEqual(@as(usize, 2), ref_cov.len);
    try testing.expectEqual(Coverage{ .cid = 7, .datao = 0, .len = 100 }, ref_cov[0]);
    try testing.expectEqual(Coverage{ .cid = 7, .datao = 200, .len = 50 }, ref_cov[1]);
    // The non-span events in order: end(7,last,success), cqe(8), end(9,last), end(7).
    var ends: usize = 0;
    var cqes: usize = 0;
    for (ref.events.items) |ev| switch (ev) {
        .c2h_end => ends += 1,
        .cqe => |c| {
            cqes += 1;
            try testing.expectEqual(@as(u16, 8), c.cid);
            try testing.expectEqual(@as(u32, 0xCAFE), c.dw0);
            try testing.expect(c.ok());
        },
        else => {},
    };
    try testing.expectEqual(@as(usize, 3), ends);
    try testing.expectEqual(@as(usize, 1), cqes);
    // The first payload span names the bytes where they lie: offset 28 in slot 0.
    try testing.expectEqual(@as(u32, 28), ref.events.items[0].c2h_span.span.off);

    // Every grain 1..n: the same coverage, the same ends/cqes, header copy
    // exactly 24 × PDUs regardless of how the headers straddle chunks.
    var grain: usize = 1;
    while (grain <= n) : (grain += 1) {
        var rec: Recorder = .{ .gpa = gpa };
        defer rec.deinit();
        var p: Parser = .{};
        var pos: usize = 0;
        var slot: u32 = 0;
        while (pos < n) : (slot += 1) {
            const take = @min(grain, n - pos);
            try p.push(slot, 0, buf[pos..][0..take], rec.sink());
            pos += take;
        }
        try testing.expect(p.idle());
        try testing.expectEqual(@as(u64, 4 * 24), p.hdr_copy_bytes);
        const cov = try coverage(gpa, rec.events.items);
        defer gpa.free(cov);
        try testing.expectEqualSlices(Coverage, ref_cov, cov);
        var e2: usize = 0;
        var c2: usize = 0;
        for (rec.events.items) |ev| switch (ev) {
            .c2h_end => e2 += 1,
            .cqe => c2 += 1,
            .c2h_span => |s| try testing.expect(s.span.len <= grain),
        };
        try testing.expectEqual(ends, e2);
        try testing.expectEqual(cqes, c2);
    }
}

test "the 24 B scratch law: a 28 B header (hlen 28), an H2CData, an R2T and a digest flag are framing violations at the earliest byte that proves them" {
    const gpa = testing.allocator;
    var rec: Recorder = .{ .gpa = gpa };
    defer rec.deinit();
    var b: [64]u8 = undefined;
    pdu.encodeC2HDataHeader(b[0..24], 1, 0, 8, 0, true, true);
    b[2] = 28;
    var p: Parser = .{};
    // The verdict lands at byte 8 (hlen known), before the rest arrives.
    try p.push(0, 0, b[0..7], rec.sink());
    try testing.expectError(error.BadHeaderLength, p.push(0, 7, b[7..8], rec.sink()));
    pdu.encodeC2HDataHeader(b[0..24], 1, 0, 8, 0, true, true);
    b[0] = @backingInt(pdu.Type.h2c_data);
    p = .{};
    try testing.expectError(error.UnexpectedPduType, p.push(0, 0, b[0..24], rec.sink()));
    b[0] = @backingInt(pdu.Type.r2t);
    p = .{};
    try testing.expectError(error.UnexpectedPduType, p.push(0, 0, b[0..24], rec.sink()));
    pdu.encodeC2HDataHeader(b[0..24], 1, 0, 8, 0, true, true);
    b[1] |= 0x2;
    p = .{};
    try testing.expectError(error.DigestFlag, p.push(0, 0, b[0..24], rec.sink()));
    // A CapsuleResp whose plen is not 24 would desynchronise: refused.
    pdu.encodeCapsuleResp(b[0..24], 1, 0, 0, 0, false);
    std.mem.writeInt(u32, b[4..8], 32, .little);
    p = .{};
    try testing.expectError(error.BadHeaderLength, p.push(0, 0, b[0..24], rec.sink()));
    // The geometry law.
    pdu.encodeC2HDataHeader(b[0..24], 1, 0, 8, 0, true, true);
    std.mem.writeInt(u32, b[4..8], 40, .little);
    p = .{};
    try testing.expectError(error.Geometry, p.push(0, 0, b[0..24], rec.sink()));
    try testing.expectEqual(@as(usize, 0), rec.events.items.len);
}

test "a sink refusal (an unknown CID) surfaces as Violation and nothing was emitted past it" {
    const Refuser = struct {
        fn emit(_: *anyopaque, _: Event) error{Violation}!void {
            return error.Violation;
        }
    };
    var dummy: u8 = 0;
    var b: [64]u8 = undefined;
    pdu.encodeC2HDataHeader(b[0..24], 1, 0, 8, 0, true, true);
    var p: Parser = .{};
    try testing.expectError(error.Violation, p.push(0, 0, b[0..32], .{ .ctx = &dummy, .emit = &Refuser.emit }));
}

fn streamFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [1024]u8 = undefined;
    const len = smith.slice(&buf);
    var grain_b: [1]u8 = undefined;
    smith.bytes(&grain_b);
    const grain: usize = @as(usize, grain_b[0]) + 1;
    var rec: Recorder = .{ .gpa = testing.allocator };
    defer rec.deinit();
    var p: Parser = .{};
    var pos: usize = 0;
    var slot: u32 = 0;
    while (pos < len) : (slot += 1) {
        const take = @min(grain, len - pos);
        p.push(slot, 0, buf[pos..][0..take], rec.sink()) catch break;
        pos += take;
    }
    // Totality: no panic; the header copy never exceeds 24 B per PDU seen
    // plus the one in flight.
    try testing.expect(p.hdr_copy_bytes <= 24 * (p.pdus + 1));
}

test "fuzz: the parser is total on arbitrary bytes at arbitrary chunk grains" {
    try fuzzkit.fuzz({}, streamFuzz, .{});
}
