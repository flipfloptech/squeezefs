//! The job verb block's payloads (`Block.job = 0x06xx`; spec §1.8;
//! decision 0003 §G "the job verbs as block `0x06xx` on the ONE
//! listener"): canonical `extern struct` heads + counted tails under
//! `wire/codec.zig`'s ONE law — never bincode, never a positional schema
//! bump. `wire/verbs.zig` imports these for its `ReqType`/`RepType`
//! tables; the handlers and screens live in `wire.zig`.
//!
//! The reference pushed `ShardAssign` coordinator → worker over a second
//! listener; on the ONE request/reply listener the worker PULLS
//! (`shard_assign` is a call whose reply is the descriptor or "none"),
//! which is the same protocol with the same words and no second socket.
//!
//! Every count is a CODEC cap here (`n ≤ 64`, a report ≤ the bulk class);
//! the SCREEN against the shard table (fencing, the destination window) is
//! the handler's. Portable.

const std = @import("std");
const fuzzkit = @import("../../core/fuzzkit.zig");
const codec = @import("../../wire/codec.zig");
const records = @import("records.zig");

const Builder = codec.Builder;
const Cursor = codec.Cursor;
const Error = codec.Error;

/// Ours, not the reference's 4 (bincode positional bumps): the block's
/// schema starts at 1.
pub const JOB_SCHEMA: u32 = 1;

/// A worker's capability bits: ROUTING, never security — a lying
/// capability buys a shard the worker can only abandon.
pub const CAP_FLEET_READ: u32 = 1 << 0;
pub const CAPS_KNOWN: u32 = CAP_FLEET_READ;

/// The per-shard window (`records.SHARD_DEST_MAX`): the codec cap of every
/// counted tail but the read-shard payload.
pub const SHARD_ITEMS_MAX: usize = records.SHARD_DEST_MAX;
/// A fleet read-shard report rides the 16 MiB bulk class; the length
/// prefix is checked against the bytes present before the view is taken.
pub const READ_SHARD_PAYLOAD_MAX: usize = 16 << 20;

/// `job_enroll`: sent FIRST after admission; `pr_key` = the worker HOST's
/// registration on the data namespaces (0 = none), `caps` routing bits.
pub const JobEnrollReq = extern struct {
    pr_key: u64,
    caps: u32,
    reserved: u32,

    pub fn check(self: JobEnrollReq) bool {
        return self.caps & ~CAPS_KNOWN == 0;
    }
};

pub const JobEnrolled = extern struct {
    heartbeat_ms: u32,
    lease_ttl_ms: u32,
};

/// The worker pulls work: its caps say what it can take.
pub const ShardAssignReq = extern struct {
    caps: u32,
    reserved: u32,

    pub fn check(self: ShardAssignReq) bool {
        return self.caps & ~CAPS_KNOWN == 0;
    }
};

/// A source or destination tuple on the wire.
pub const TupleWire = extern struct {
    vol_tag: u64,
    block_idx: u64,
};

/// The descriptor head; `job_id == 0` ⇒ no work (every other word zero).
pub const ShardDescriptorHead = extern struct {
    job_id: u64,
    shard: u32,
    shard_fencing_lo: u32,
    shard_fencing_hi: u32,
    lease_ttl_ms: u32,
    n_src: u32,
    n_dest: u32,
    job_type: u8,
    throttle_pct: u8,
    reserved0: [6]u8,
    block_len: u64,
    /// noop: tasks / task_ms; fleet: `(k, n)` ride `fleet_k/fleet_n`.
    arg0: u64,
    arg1: u64,
    fleet_k: u32,
    fleet_n: u32,
    inode_plane: u64,

    pub fn fencing(self: ShardDescriptorHead) u64 {
        return (@as(u64, self.shard_fencing_hi) << 32) | self.shard_fencing_lo;
    }

    pub fn check(self: ShardDescriptorHead) bool {
        if (self.job_id == 0) return self.shard == 0 and self.n_src == 0 and self.n_dest == 0 and self.job_type == 0 and self.lease_ttl_ms == 0;
        if (std.enums.fromInt(records.JobType, self.job_type) == null) return false;
        if (self.throttle_pct > 100) return false;
        if (self.n_src > SHARD_ITEMS_MAX or self.n_dest > SHARD_ITEMS_MAX) return false;
        if (self.fleet_n != 0 and self.fleet_k >= self.fleet_n) return false;
        return true;
    }
};

/// The descriptor: head ‖ sources ‖ destinations.
pub const ShardDescriptor = struct {
    head: ShardDescriptorHead,
    sources: []align(1) const TupleWire,
    destinations: []align(1) const TupleWire,

    pub const none: ShardDescriptor = .{ .head = std.mem.zeroes(ShardDescriptorHead), .sources = &.{}, .destinations = &.{} };

    pub fn encode(self: ShardDescriptor, b: *Builder) codec.BuildError!void {
        std.debug.assert(self.head.n_src == self.sources.len and self.head.n_dest == self.destinations.len);
        try b.put(ShardDescriptorHead, self.head);
        try b.putView(TupleWire, self.sources);
        try b.putView(TupleWire, self.destinations);
    }

    pub fn decode(c: *Cursor) Error!ShardDescriptor {
        const head = try c.take(ShardDescriptorHead);
        const ns = try codec.countAtMost(head.n_src, SHARD_ITEMS_MAX);
        const nd = try codec.countAtMost(head.n_dest, SHARD_ITEMS_MAX);
        return .{ .head = head, .sources = try c.takeArray(TupleWire, ns), .destinations = try c.takeArray(TupleWire, nd) };
    }

    pub fn isNone(self: ShardDescriptor) bool {
        return self.head.job_id == 0;
    }
};

/// The shard identity + fencing every per-shard call carries.
pub const ShardRef = extern struct {
    job_id: u64,
    shard: u32,
    reserved: u32,
    shard_fencing: u64,

    pub fn check(self: ShardRef) bool {
        return self.job_id != 0;
    }
};

pub const HeartbeatReq = ShardRef;

pub const HeartbeatAck = extern struct {
    lease_ttl_ms: u32,
    /// `LeaseVerdict`: 0 live, 1 unknown, 2 stale.
    verdict: u8,
    reserved: [3]u8,

    pub fn check(self: HeartbeatAck) bool {
        return self.verdict <= 2;
    }
};

/// One written destination's checksum the coordinator verify-reads.
pub const BlockChecksum = extern struct {
    vol_tag: u64,
    block_idx: u64,
    len: u64,
    xxh3: u64,
};

pub const ResultSubmitHead = extern struct {
    ref: ShardRef,
    n: u32,
    reserved: u32,

    pub fn check(self: ResultSubmitHead) bool {
        return self.ref.check() and self.n <= SHARD_ITEMS_MAX;
    }
};

pub const ResultSubmitReq = struct {
    head: ResultSubmitHead,
    checksums: []align(1) const BlockChecksum,

    pub fn encode(self: ResultSubmitReq, b: *Builder) codec.BuildError!void {
        std.debug.assert(self.head.n == self.checksums.len);
        try b.put(ResultSubmitHead, self.head);
        try b.putView(BlockChecksum, self.checksums);
    }

    pub fn decode(c: *Cursor) Error!ResultSubmitReq {
        const head = try c.take(ResultSubmitHead);
        const n = try codec.countAtMost(head.n, SHARD_ITEMS_MAX);
        return .{ .head = head, .checksums = try c.takeArray(BlockChecksum, n) };
    }
};

/// Why a proposal was refused (a CLASS, never a string).
pub const RefusalReason = enum(u8) {
    accepted = 0,
    /// Unknown `(job, shard)`.
    unknown_shard = 1,
    /// The fencing moved or the shard is done.
    stale = 2,
    /// A tuple outside the shard's pre-allocated window, a count mismatch,
    /// a missing checksum.
    window = 3,
    /// A length or xxh3 mismatch on verify-read (treated as an expired
    /// lease: fencing bump + quarantine).
    verify_failed = 4,
    /// Not the coordinator.
    not_coordinator = 5,
};

pub const ResultAck = extern struct {
    reason: u8,
    reserved: [7]u8,

    pub fn of(r: RefusalReason) ResultAck {
        return .{ .reason = @backingInt(r), .reserved = @splat(0) };
    }

    pub fn check(self: ResultAck) bool {
        return std.enums.fromInt(RefusalReason, self.reason) != null;
    }

    pub fn verdict(self: ResultAck) RefusalReason {
        return @fromBackingInt(@intCast(self.reason));
    }
};

pub const ReadShardResultHead = extern struct {
    ref: ShardRef,
    len: u32,
    reserved: u32,

    pub fn check(self: ReadShardResultHead) bool {
        return self.ref.check() and self.len <= READ_SHARD_PAYLOAD_MAX;
    }
};

/// A fleet read shard's report (bulk class).
pub const ReadShardResultReq = struct {
    head: ReadShardResultHead,
    payload: []const u8,

    pub fn encode(self: ReadShardResultReq, b: *Builder) codec.BuildError!void {
        std.debug.assert(self.head.len == self.payload.len);
        try b.put(ReadShardResultHead, self.head);
        try b.putBytes(self.payload);
        try b.putPadding(self.payload.len, codec.TAIL_ALIGN);
    }

    pub fn decode(c: *Cursor) Error!ReadShardResultReq {
        const head = try c.take(ReadShardResultHead);
        const payload = try c.takeBytes(head.len);
        try c.takePadding(head.len, codec.TAIL_ALIGN);
        return .{ .head = head, .payload = payload };
    }
};

pub const AbandonReason = enum(u8) {
    /// The worker's lease clock expired before the submit.
    lease_expired = 1,
    /// Memory Red on the member (`job_fleet_worker_red_aborts`).
    memory_red = 2,
    /// The worker cannot execute the shard (a lying capability, a missing engine).
    incapable = 3,
    /// Operator stop / shutdown.
    stopped = 4,
};

pub const ShardAbandonReq = extern struct {
    ref: ShardRef,
    reason: u8,
    reserved: [7]u8,

    pub fn check(self: ShardAbandonReq) bool {
        return self.ref.check() and std.enums.fromInt(AbandonReason, self.reason) != null;
    }
};

comptime {
    std.debug.assert(codec.wireSize(JobEnrollReq) == 16);
    std.debug.assert(codec.wireSize(JobEnrolled) == 8);
    std.debug.assert(codec.wireSize(ShardAssignReq) == 8);
    std.debug.assert(codec.wireSize(TupleWire) == 16);
    std.debug.assert(codec.wireSize(ShardDescriptorHead) == 80);
    std.debug.assert(codec.wireSize(ShardRef) == 24);
    std.debug.assert(codec.wireSize(HeartbeatAck) == 8);
    std.debug.assert(codec.wireSize(BlockChecksum) == 32);
    std.debug.assert(codec.wireSize(ResultSubmitHead) == 32);
    std.debug.assert(codec.wireSize(ResultAck) == 8);
    std.debug.assert(codec.wireSize(ReadShardResultHead) == 32);
    std.debug.assert(codec.wireSize(ShardAbandonReq) == 32);
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

fn roundTrip(comptime P: type, v: P, buf: []u8) !P {
    var b: Builder = .init(buf);
    try codec.encodePayload(P, v, &b);
    return codec.decodePayload(P, b.written());
}

test "every job payload round-trips; the none descriptor is all-zero; counts past 64 and a lying length are Malformed; unknown caps/type/verdict bytes fail check" {
    const gpa = testing.allocator;
    const buf = try gpa.alloc(u8, 1 << 20);
    defer gpa.free(buf);
    const e = try roundTrip(JobEnrollReq, .{ .pr_key = 0x1234, .caps = CAP_FLEET_READ, .reserved = 0 }, buf);
    try testing.expectEqual(@as(u64, 0x1234), e.pr_key);
    try testing.expectError(error.Malformed, roundTrip(JobEnrollReq, .{ .pr_key = 0, .caps = 0x80, .reserved = 0 }, buf));
    var srcs: [SHARD_ITEMS_MAX + 1]TupleWire = undefined;
    for (&srcs, 0..) |*s, i| s.* = .{ .vol_tag = 1, .block_idx = i };
    const head: ShardDescriptorHead = .{ .job_id = 9, .shard = 2, .shard_fencing_lo = 5, .shard_fencing_hi = 1, .lease_ttl_ms = 30_000, .job_type = @backingInt(records.JobType.noop), .throttle_pct = 25, .reserved0 = @splat(0), .n_src = 64, .n_dest = 3, .block_len = 4 << 20, .arg0 = 10, .arg1 = 1, .fleet_k = 0, .fleet_n = 0, .inode_plane = 0 };
    const d = try roundTrip(ShardDescriptor, .{ .head = head, .sources = srcs[0..64], .destinations = srcs[0..3] }, buf);
    try testing.expectEqual(@as(u64, (1 << 32) | 5), d.head.fencing());
    try testing.expectEqual(@as(usize, 64), d.sources.len);
    try testing.expectEqual(@as(u64, 63), d.sources[63].block_idx);
    try testing.expect(!d.isNone());
    var over = head;
    over.n_src = 65;
    over.n_dest = 0;
    try testing.expectError(error.Malformed, roundTrip(ShardDescriptor, .{ .head = over, .sources = srcs[0..65], .destinations = &.{} }, buf));
    var bad_type = head;
    bad_type.job_type = 200;
    try testing.expectError(error.Malformed, roundTrip(ShardDescriptor, .{ .head = bad_type, .sources = srcs[0..64], .destinations = srcs[0..3] }, buf));
    const none = try roundTrip(ShardDescriptor, ShardDescriptor.none, buf);
    try testing.expect(none.isNone());
    // A lying count: the head says 64, two tuples follow.
    {
        var b: Builder = .init(buf);
        try b.put(ShardDescriptorHead, head);
        try b.putView(TupleWire, srcs[0..2]);
        try testing.expectError(error.Malformed, codec.decodePayload(ShardDescriptor, b.written()));
    }
    const cs = [_]BlockChecksum{ .{ .vol_tag = 1, .block_idx = 2, .len = 4096, .xxh3 = 7 }, .{ .vol_tag = 1, .block_idx = 3, .len = 4096, .xxh3 = 8 } };
    const r = try roundTrip(ResultSubmitReq, .{ .head = .{ .ref = .{ .job_id = 9, .shard = 2, .reserved = 0, .shard_fencing = 1 }, .n = 2, .reserved = 0 }, .checksums = &cs }, buf);
    try testing.expectEqual(@as(u64, 8), r.checksums[1].xxh3);
    try testing.expectError(error.Malformed, roundTrip(ResultSubmitReq, .{ .head = .{ .ref = .{ .job_id = 0, .shard = 2, .reserved = 0, .shard_fencing = 1 }, .n = 2, .reserved = 0 }, .checksums = &cs }, buf));
    const ack = try roundTrip(ResultAck, ResultAck.of(.verify_failed), buf);
    try testing.expectEqual(RefusalReason.verify_failed, ack.verdict());
    try testing.expectError(error.Malformed, roundTrip(ResultAck, .{ .reason = 99, .reserved = @splat(0) }, buf));
    const hb = try roundTrip(HeartbeatAck, .{ .lease_ttl_ms = 30_000, .verdict = 2, .reserved = @splat(0) }, buf);
    try testing.expectEqual(@as(u8, 2), hb.verdict);
    const payload = "a fleet report of odd length";
    const rs = try roundTrip(ReadShardResultReq, .{ .head = .{ .ref = .{ .job_id = 9, .shard = 1 << 20, .reserved = 0, .shard_fencing = 0 }, .len = payload.len, .reserved = 0 }, .payload = payload }, buf);
    try testing.expectEqualStrings(payload, rs.payload);
    const ab = try roundTrip(ShardAbandonReq, .{ .ref = .{ .job_id = 9, .shard = 1, .reserved = 0, .shard_fencing = 0 }, .reason = @backingInt(AbandonReason.memory_red), .reserved = @splat(0) }, buf);
    try testing.expectEqual(@as(u8, 2), ab.reason);
    try testing.expectError(error.Malformed, roundTrip(ShardAbandonReq, .{ .ref = .{ .job_id = 9, .shard = 1, .reserved = 0, .shard_fencing = 0 }, .reason = 0, .reserved = @splat(0) }, buf));
}

fn jobPayloadsTotal(bytes: []const u8) void {
    _ = codec.decodePayload(ShardDescriptor, bytes) catch {};
    _ = codec.decodePayload(ResultSubmitReq, bytes) catch {};
    _ = codec.decodePayload(ReadShardResultReq, bytes) catch {};
    _ = codec.decodePayload(ShardAbandonReq, bytes) catch {};
    _ = codec.decodePayload(JobEnrollReq, bytes) catch {};
    _ = codec.decodePayload(HeartbeatAck, bytes) catch {};
    _ = codec.decodePayload(ResultAck, bytes) catch {};
}

fn jobPayloadsFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [4096]u8 = undefined;
    const len = smith.slice(&buf);
    jobPayloadsTotal(buf[0..len]);
}

test "fuzz: no job payload decoder panics on arbitrary bytes" {
    try fuzzkit.fuzz({}, jobPayloadsFuzz, .{});
}
