//! The ADMIN LANE's payloads (`Block.admin = 0x07xx`; charter §8 "CLI …
//! `fsck --online`, `defrag`, `job {list, status, pause, resume, cancel,
//! throttle}`, `volume {add-data, remove-data, undrain}`" acting on a
//! LIVE mount): the operator's verbs ride the ONE listener behind the
//! storage-trust ladder (`wire/auth.zig`: whoever can read the set's
//! `job:enroll` record may administer it — the same credential every
//! member proves), never a second socket, never the read-only `.stats`
//! inode. Canonical `extern struct` heads + counted tails under `wire/
//! codec.zig`'s ONE law; `wire/verbs.zig` imports these for its tables;
//! the handlers live in `admin.zig`.
//!
//! Every reply carries a typed VERDICT byte (never a string the CLI
//! parses for its exit code — decision 0002 row 404); the refusal text a
//! verb needs is the `WireError` reason of a status reply. Portable.

const std = @import("std");
const fuzzkit = @import("../../core/fuzzkit.zig");
const codec = @import("../../wire/codec.zig");
const records = @import("records.zig");

const Builder = codec.Builder;
const Cursor = codec.Cursor;
const Error = codec.Error;

/// Ours; starts at 1.
pub const ADMIN_SCHEMA: u32 = 1;

/// The live table's bound (`fabric.MAX_JOBS`): the list reply's tail cap.
pub const LIST_ROWS_MAX: usize = 256;
/// The fsck report an admin read carries (`fsck.ADMIN_WIRE_CAP`, the
/// bounded JSON the engine renders for the admin lane).
pub const REPORT_BYTES_MAX: usize = 60 * 1024;
/// A device path the online `add-data` carries (PATH_MAX, the kernel's).
pub const URI_MAX: usize = 4096;

/// The admin verdict: `ok`, or WHY not — a class the CLI maps to its exit
/// code and its remedy text.
pub const Verdict = enum(u8) {
    ok = 0,
    /// No job record of that id in the live table.
    unknown_job = 1,
    /// The job is terminal: nothing to control.
    terminal = 2,
    /// This daemon is not volume 0's manager (the coordinator).
    not_coordinator = 3,
    /// The live table is full (`MAX_JOBS`).
    table_full = 4,
    /// The lattice refused (the reason text names the transition).
    refused = 5,
    /// A device / commit error on the daemon (its log names it).
    io = 6,
    /// The job carries no report (not an fsck job, or none ran yet).
    no_report = 7,
};

fn verdictOk(b: u8) bool {
    return std.enums.fromInt(Verdict, b) != null;
}

// ---- admin_job_list ----

pub const AdminJobListReq = extern struct {
    reserved: u64,
};

pub const AdminJobListHead = extern struct {
    n: u32,
    verdict: u8,
    reserved: [3]u8,

    pub fn check(self: AdminJobListHead) bool {
        return self.n <= LIST_ROWS_MAX and verdictOk(self.verdict);
    }
};

/// One record's wire bytes (the CLI decodes with `JobRecordV1.decode`).
pub const JobRow = extern struct {
    bytes: [records.JobRecordV1.LEN]u8,
};

pub const AdminJobList = struct {
    head: AdminJobListHead,
    rows: []align(1) const JobRow,

    pub fn encode(self: AdminJobList, b: *Builder) codec.BuildError!void {
        std.debug.assert(self.head.n == self.rows.len);
        try b.put(AdminJobListHead, self.head);
        try b.putView(JobRow, self.rows);
    }

    pub fn decode(c: *Cursor) Error!AdminJobList {
        const head = try c.take(AdminJobListHead);
        const n = try codec.countAtMost(head.n, LIST_ROWS_MAX);
        return .{ .head = head, .rows = try c.takeArray(JobRow, n) };
    }
};

// ---- admin_job_control ----

pub const ControlOp = enum(u8) { pause = 0, @"resume" = 1, cancel = 2, throttle = 3 };

pub const AdminJobControlReq = extern struct {
    job_id: u64,
    op: u8,
    /// `throttle`'s percentage (1..=100); 0 elsewhere.
    pct: u8,
    reserved: [6]u8,

    pub fn check(self: AdminJobControlReq) bool {
        const op = std.enums.fromInt(ControlOp, self.op) orelse return false;
        if (self.job_id == 0) return false;
        return if (op == .throttle) self.pct >= 1 and self.pct <= 100 else self.pct == 0;
    }

    pub fn operation(self: AdminJobControlReq) ControlOp {
        return @fromBackingInt(@intCast(self.op));
    }
};

pub const AdminVerdictAck = extern struct {
    verdict: u8,
    reserved: [7]u8,

    pub fn of(v: Verdict) AdminVerdictAck {
        return .{ .verdict = @backingInt(v), .reserved = @splat(0) };
    }

    pub fn check(self: AdminVerdictAck) bool {
        return verdictOk(self.verdict);
    }

    pub fn get(self: AdminVerdictAck) Verdict {
        return @fromBackingInt(@intCast(self.verdict));
    }
};

// ---- admin_job_submit ----

pub const AdminJobSubmitReq = extern struct {
    job_type: u8,
    /// 0 ⇒ the fabric's default.
    throttle_pct: u8,
    /// 1 ⇒ submitted paused (the operator's `--paused`).
    paused: u8,
    reserved: [5]u8,
    arg0: u64,
    arg1: u64,

    pub fn check(self: AdminJobSubmitReq) bool {
        return std.enums.fromInt(records.JobType, self.job_type) != null and self.throttle_pct <= 100 and self.paused <= 1;
    }

    pub fn jobType(self: AdminJobSubmitReq) records.JobType {
        return @fromBackingInt(@intCast(self.job_type));
    }
};

pub const AdminJobSubmitted = extern struct {
    job_id: u64,
    verdict: u8,
    reserved: [7]u8,

    pub fn check(self: AdminJobSubmitted) bool {
        return verdictOk(self.verdict);
    }

    pub fn get(self: AdminJobSubmitted) Verdict {
        return @fromBackingInt(@intCast(self.verdict));
    }
};

// ---- admin_fsck_report ----

pub const AdminFsckReportReq = extern struct {
    job_id: u64,

    pub fn check(self: AdminFsckReportReq) bool {
        return self.job_id != 0;
    }
};

pub const AdminFsckReportHead = extern struct {
    len: u32,
    verdict: u8,
    reserved: [3]u8,

    pub fn check(self: AdminFsckReportHead) bool {
        return self.len <= REPORT_BYTES_MAX and verdictOk(self.verdict);
    }
};

/// The fsck job's last bounded report (JSON) — empty with a non-ok verdict.
pub const AdminFsckReport = struct {
    head: AdminFsckReportHead,
    json: []const u8,

    pub fn encode(self: AdminFsckReport, b: *Builder) codec.BuildError!void {
        std.debug.assert(self.head.len == self.json.len);
        try b.put(AdminFsckReportHead, self.head);
        try b.putBytes(self.json);
        try b.putPadding(self.json.len, codec.TAIL_ALIGN);
    }

    pub fn decode(c: *Cursor) Error!AdminFsckReport {
        const head = try c.take(AdminFsckReportHead);
        const json = try c.takeBytes(head.len);
        try c.takePadding(head.len, codec.TAIL_ALIGN);
        return .{ .head = head, .json = json };
    }
};

// ---- admin_volume ----

pub const VolumeOp = enum(u8) { add_data = 0, remove_data = 1, undrain = 2 };

pub const AdminVolumeHead = extern struct {
    op: u8,
    /// `add_data`: 1 ⇒ no queued rebalance.
    no_rebalance: u8,
    reserved: [6]u8,
    /// `remove_data` / `undrain`: the victim; 0 for `add_data`.
    vol_tag: u64,
    /// `add_data`: the device path's length; 0 otherwise.
    uri_len: u32,
    reserved2: u32,

    pub fn check(self: AdminVolumeHead) bool {
        const op = std.enums.fromInt(VolumeOp, self.op) orelse return false;
        if (self.no_rebalance > 1 or self.uri_len > URI_MAX) return false;
        return switch (op) {
            .add_data => self.vol_tag == 0 and self.uri_len != 0,
            .remove_data, .undrain => self.vol_tag != 0 and self.uri_len == 0 and self.no_rebalance == 0,
        };
    }

    pub fn operation(self: AdminVolumeHead) VolumeOp {
        return @fromBackingInt(@intCast(self.op));
    }
};

pub const AdminVolumeReq = struct {
    head: AdminVolumeHead,
    uri: []const u8,

    pub fn encode(self: AdminVolumeReq, b: *Builder) codec.BuildError!void {
        std.debug.assert(self.head.uri_len == self.uri.len);
        try b.put(AdminVolumeHead, self.head);
        try b.putBytes(self.uri);
        try b.putPadding(self.uri.len, codec.TAIL_ALIGN);
    }

    pub fn decode(c: *Cursor) Error!AdminVolumeReq {
        const head = try c.take(AdminVolumeHead);
        const uri = try c.takeBytes(head.uri_len);
        try c.takePadding(head.uri_len, codec.TAIL_ALIGN);
        return .{ .head = head, .uri = uri };
    }
};

/// The lattice's outcome: the volume touched, the job queued (0 = none),
/// the config generation after, the preflight's four terms (`remove_data`;
/// 0 elsewhere), the evacuations cancelled (`undrain`).
pub const AdminVolumeReply = extern struct {
    verdict: u8,
    reserved: [7]u8,
    vol_tag: u64,
    job_id: u64,
    config_generation: u64,
    needed: u64,
    avail: u64,
    transient: u64,
    headroom: u64,
    cancelled: u32,
    capacity_blocks: u32,

    pub fn check(self: AdminVolumeReply) bool {
        return verdictOk(self.verdict);
    }

    pub fn get(self: AdminVolumeReply) Verdict {
        return @fromBackingInt(@intCast(self.verdict));
    }

    pub fn refusal(v: Verdict) AdminVolumeReply {
        return .{ .verdict = @backingInt(v), .reserved = @splat(0), .vol_tag = 0, .job_id = 0, .config_generation = 0, .needed = 0, .avail = 0, .transient = 0, .headroom = 0, .cancelled = 0, .capacity_blocks = 0 };
    }
};

comptime {
    std.debug.assert(codec.wireSize(AdminJobListReq) == 8);
    std.debug.assert(codec.wireSize(AdminJobListHead) == 8);
    std.debug.assert(codec.wireSize(JobRow) == 96);
    std.debug.assert(codec.wireSize(AdminJobControlReq) == 16);
    std.debug.assert(codec.wireSize(AdminVerdictAck) == 8);
    std.debug.assert(codec.wireSize(AdminJobSubmitReq) == 24);
    std.debug.assert(codec.wireSize(AdminJobSubmitted) == 16);
    std.debug.assert(codec.wireSize(AdminFsckReportReq) == 8);
    std.debug.assert(codec.wireSize(AdminFsckReportHead) == 8);
    std.debug.assert(codec.wireSize(AdminVolumeHead) == 24);
    std.debug.assert(codec.wireSize(AdminVolumeReply) == 72);
    // The widest list reply stays far inside the control class (1 MiB).
    std.debug.assert(8 + LIST_ROWS_MAX * 96 < 1 << 20);
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

test "every admin payload round-trips; the op/verdict/type bytes are total; a pct outside the op's law, a lying count or length, an add with no uri are Malformed" {
    const gpa = testing.allocator;
    const buf = try gpa.alloc(u8, 1 << 20);
    defer gpa.free(buf);
    var rows: [LIST_ROWS_MAX + 1]JobRow = undefined;
    for (&rows, 0..) |*r, i| r.* = .{ .bytes = @splat(@intCast(i & 0xff)) };
    const l = try roundTrip(AdminJobList, .{ .head = .{ .n = LIST_ROWS_MAX, .verdict = 0, .reserved = @splat(0) }, .rows = rows[0..LIST_ROWS_MAX] }, buf);
    try testing.expectEqual(@as(usize, LIST_ROWS_MAX), l.rows.len);
    try testing.expectEqual(@as(u8, 255), l.rows[255].bytes[0]);
    try testing.expectError(error.Malformed, roundTrip(AdminJobList, .{ .head = .{ .n = LIST_ROWS_MAX + 1, .verdict = 0, .reserved = @splat(0) }, .rows = rows[0..] }, buf));
    {
        var b: Builder = .init(buf);
        try b.put(AdminJobListHead, .{ .n = 4, .verdict = 0, .reserved = @splat(0) });
        try b.putView(JobRow, rows[0..2]);
        try testing.expectError(error.Malformed, codec.decodePayload(AdminJobList, b.written()));
    }
    const c = try roundTrip(AdminJobControlReq, .{ .job_id = 7, .op = @backingInt(ControlOp.throttle), .pct = 25, .reserved = @splat(0) }, buf);
    try testing.expectEqual(ControlOp.throttle, c.operation());
    try testing.expectError(error.Malformed, roundTrip(AdminJobControlReq, .{ .job_id = 7, .op = @backingInt(ControlOp.throttle), .pct = 0, .reserved = @splat(0) }, buf));
    try testing.expectError(error.Malformed, roundTrip(AdminJobControlReq, .{ .job_id = 7, .op = @backingInt(ControlOp.pause), .pct = 25, .reserved = @splat(0) }, buf));
    try testing.expectError(error.Malformed, roundTrip(AdminJobControlReq, .{ .job_id = 0, .op = @backingInt(ControlOp.pause), .pct = 0, .reserved = @splat(0) }, buf));
    try testing.expectError(error.Malformed, roundTrip(AdminJobControlReq, .{ .job_id = 7, .op = 9, .pct = 0, .reserved = @splat(0) }, buf));
    const a = try roundTrip(AdminVerdictAck, AdminVerdictAck.of(.terminal), buf);
    try testing.expectEqual(Verdict.terminal, a.get());
    try testing.expectError(error.Malformed, roundTrip(AdminVerdictAck, .{ .verdict = 200, .reserved = @splat(0) }, buf));
    const s = try roundTrip(AdminJobSubmitReq, .{ .job_type = @backingInt(records.JobType.fsck), .throttle_pct = 50, .paused = 1, .reserved = @splat(0), .arg0 = 1, .arg1 = 0 }, buf);
    try testing.expectEqual(records.JobType.fsck, s.jobType());
    try testing.expectError(error.Malformed, roundTrip(AdminJobSubmitReq, .{ .job_type = 0, .throttle_pct = 50, .paused = 0, .reserved = @splat(0), .arg0 = 0, .arg1 = 0 }, buf));
    try testing.expectError(error.Malformed, roundTrip(AdminJobSubmitReq, .{ .job_type = 1, .throttle_pct = 101, .paused = 0, .reserved = @splat(0), .arg0 = 0, .arg1 = 0 }, buf));
    const sub = try roundTrip(AdminJobSubmitted, .{ .job_id = 9, .verdict = @backingInt(Verdict.ok), .reserved = @splat(0) }, buf);
    try testing.expectEqual(Verdict.ok, sub.get());
    const json = "{\"schema\":1,\"findings\":[]}";
    const rep = try roundTrip(AdminFsckReport, .{ .head = .{ .len = json.len, .verdict = 0, .reserved = @splat(0) }, .json = json }, buf);
    try testing.expectEqualStrings(json, rep.json);
    const over_cap = try gpa.alloc(u8, REPORT_BYTES_MAX + 1);
    defer gpa.free(over_cap);
    @memset(over_cap, 'x');
    try testing.expectError(error.Malformed, roundTrip(AdminFsckReport, .{ .head = .{ .len = REPORT_BYTES_MAX + 1, .verdict = 0, .reserved = @splat(0) }, .json = over_cap }, buf));
    const uri = "/dev/nvme2n1";
    const add = try roundTrip(AdminVolumeReq, .{ .head = .{ .op = @backingInt(VolumeOp.add_data), .no_rebalance = 1, .reserved = @splat(0), .vol_tag = 0, .uri_len = uri.len, .reserved2 = 0 }, .uri = uri }, buf);
    try testing.expectEqualStrings(uri, add.uri);
    try testing.expectEqual(VolumeOp.add_data, add.head.operation());
    try testing.expectError(error.Malformed, roundTrip(AdminVolumeReq, .{ .head = .{ .op = @backingInt(VolumeOp.add_data), .no_rebalance = 0, .reserved = @splat(0), .vol_tag = 0, .uri_len = 0, .reserved2 = 0 }, .uri = "" }, buf));
    try testing.expectError(error.Malformed, roundTrip(AdminVolumeReq, .{ .head = .{ .op = @backingInt(VolumeOp.remove_data), .no_rebalance = 0, .reserved = @splat(0), .vol_tag = 0, .uri_len = 0, .reserved2 = 0 }, .uri = "" }, buf));
    const rm = try roundTrip(AdminVolumeReq, .{ .head = .{ .op = @backingInt(VolumeOp.remove_data), .no_rebalance = 0, .reserved = @splat(0), .vol_tag = 0x55, .uri_len = 0, .reserved2 = 0 }, .uri = "" }, buf);
    try testing.expectEqual(@as(u64, 0x55), rm.head.vol_tag);
    const vr = try roundTrip(AdminVolumeReply, AdminVolumeReply.refusal(.refused), buf);
    try testing.expectEqual(Verdict.refused, vr.get());
}

fn adminPayloadsTotal(bytes: []const u8) void {
    _ = codec.decodePayload(AdminJobList, bytes) catch {};
    _ = codec.decodePayload(AdminJobControlReq, bytes) catch {};
    _ = codec.decodePayload(AdminJobSubmitReq, bytes) catch {};
    _ = codec.decodePayload(AdminFsckReport, bytes) catch {};
    _ = codec.decodePayload(AdminVolumeReq, bytes) catch {};
    _ = codec.decodePayload(AdminVolumeReply, bytes) catch {};
}

fn adminPayloadsFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [4096]u8 = undefined;
    const len = smith.slice(&buf);
    adminPayloadsTotal(buf[0..len]);
}

test "fuzz: no admin payload decoder panics on arbitrary bytes" {
    try fuzzkit.fuzz({}, adminPayloadsFuzz, .{});
}
