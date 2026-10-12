//! The durable `job:` records (charter §8 "durable `job:` records on ino 1";
//! `docs/porting/ops_jobs_lifecycle.md` §1.2; decision 0003 §G): two
//! tree-0 control records on VOLUME 0 — `Tag.job` (the job) and
//! `Tag.job_shard` (one remote shard's lease words) — beside `set_config`
//! and `job:enroll`. The reference kept them as additive JSON xattrs with
//! string `error`/`created_by` fields (decision 0002 rows 121–124's law
//! applied to the job plane): here they are canonical `extern struct`s,
//! explicit enum bytes, a TYPED error class, the stable identity, hash64.
//!
//! The record is ADVISORY: correctness is plan regeneration (§1.9). What
//! it must get right is the TERMINAL law — a terminal state persists
//! before the live state flips, and never regresses — and the operator's
//! PAUSE intent surviving a crash (bit 0 of `flags`).
//!
//! Decode is total: wrong length / version / checksum / unknown enum byte /
//! reserved ≠ 0 ⇒ `Corrupt`, counted and SKIPPED by the adopting scan with
//! an error log naming the key. Portable.

const std = @import("std");
const fuzzkit = @import("../../core/fuzzkit.zig");
const checksum = @import("../../core/checksum.zig");
const control_records = @import("../../meta/sym/control_records.zig");

const assert = std.debug.assert;

pub const DecodeError = error{Corrupt};

pub const Tag = control_records.Tag;
pub const ControlKey = control_records.ControlKey;
pub const jobKey = control_records.jobKey;
pub const jobShardKey = control_records.jobShardKey;
pub const jobIdOfKey = control_records.jobIdOfKey;
pub const shardOfKey = control_records.shardOfKey;
pub const rangeOf = control_records.rangeOf;
pub const StableIdentity = control_records.StableIdentity;

/// Every maintenance op is a job TYPE on the one fabric (charter §8). The
/// numbers are the wire's and the disk's: never a declaration index.
pub const JobType = enum(u8) {
    /// `{tasks, task_ms}`: the fabric's own probe (duty, park, crash-resume).
    noop = 1,
    /// `volume remove-data`: the drain via THE ONE MOVER.
    evacuate_volume = 2,
    /// `add-data`'s auto pass / `defrag --rebalance`.
    rebalance = 3,
    /// `volume migrate-meta-slot` on a live mount.
    migrate_meta_slot = 4,
    /// `fsck --online` (the fsck spec's engine; the report rides a shard-class record).
    fsck = 5,
    /// `defrag --data`: D2 locality then D1 tail compaction.
    defrag_data = 6,
    /// `defrag --pack`: the §6.1 compaction.
    defrag_pack = 7,
    /// `defrag --meta`: D4 leaf compaction + the merge sweep lap.
    defrag_meta = 8,
    /// `defrag --fold`: D3, every parked/spilled block kicked through the W2 fold.
    defrag_fold = 9,

    /// A mover-class job holds a volume (or whole-set) SCOPE: one per
    /// volume at a time (the mover-scope law, §1.3).
    pub fn isMover(self: JobType) bool {
        return switch (self) {
            .evacuate_volume, .rebalance, .defrag_data, .defrag_pack => true,
            .noop, .migrate_meta_slot, .fsck, .defrag_meta, .defrag_fold => false,
        };
    }

    /// Only `noop` ships whole jobs over the wire: a mover's publish is a
    /// per-ino meta act only the coordinator commits; fleet READ shards are
    /// the second wire shape (§1.9a).
    pub fn wireExecutable(self: JobType) bool {
        return self == .noop;
    }

    pub fn name(self: JobType) []const u8 {
        return @tagName(self);
    }

    /// The dense table index (the wire/disk numbers start at 1).
    pub fn slot(self: JobType) usize {
        return @backingInt(self) - 1;
    }

    pub const count = @typeInfo(JobType).@"enum".field_names.len;
};

pub const JobState = enum(u8) {
    queued = 1,
    running = 2,
    paused = 3,
    /// The drain's checkpoint re-verification found the slack consumed
    /// (§5.2): `job resume` after freeing space is the recovery.
    paused_capacity = 4,
    cancelled = 5,
    completed = 6,
    failed = 7,

    pub fn isTerminal(self: JobState) bool {
        return switch (self) {
            .cancelled, .completed, .failed => true,
            .queued, .running, .paused, .paused_capacity => false,
        };
    }

    pub fn isPaused(self: JobState) bool {
        return self == .paused or self == .paused_capacity;
    }
};

/// The typed failure class (never a string — decision 0002 rows 9/10).
pub const ErrorClass = enum(u8) {
    none = 0,
    /// The plan named a volume that is not placement-eligible (a defrag
    /// naming a drain's victim, §3.3).
    scope_ineligible = 1,
    /// The lever the arm needs is off (`--pack` under packing off).
    lever_off = 2,
    /// The mover's device/commit path failed terminally.
    io = 3,
    /// A job body returned an error the fabric does not classify further.
    body = 4,
    /// The set's capacity closed form refused mid-flight and the operator cancelled.
    capacity = 5,
    /// The coordinator lost its claim (the manager lease moved).
    coordinator_lost = 6,
};

/// `flags` bits.
pub const FLAG_PAUSE_INTENT: u16 = 1 << 0;
pub const FLAG_CAPACITY_PAUSE: u16 = 1 << 1;
pub const FLAG_MOVER_CLASS: u16 = 1 << 2;
const FLAGS_KNOWN: u16 = FLAG_PAUSE_INTENT | FLAG_CAPACITY_PAUSE | FLAG_MOVER_CLASS;

/// The decoded job record.
pub const JobRecord = struct {
    job_id: u64,
    job_type: JobType,
    state: JobState,
    /// 0 = unthrottled, 1..=100.
    throttle_pct: u8,
    error_class: ErrorClass = .none,
    pause_intent: bool = false,
    capacity_pause: bool = false,
    created_realtime_s: u64,
    created_by: StableIdentity,
    created_pid: u32,
    tasks_done: u64 = 0,
    tasks_total: u64 = 0,
    /// evacuate/defrag_data/defrag_pack: vol_tag (0 = every eligible
    /// volume); migrate_meta_slot: `slot u16 ‖ target u16`; fsck: option
    /// bits; noop: tasks.
    arg0: u64 = 0,
    /// noop: task_ms; fsck: the quarantine-dir hash; else 0.
    arg1: u64 = 0,
    updated_realtime_s: u64,

    pub fn encode(self: JobRecord) [JobRecordV1.LEN]u8 {
        return JobRecordV1.init(self).encode();
    }

    pub fn isTerminal(self: JobRecord) bool {
        return self.state.isTerminal();
    }

    /// `job-{16 hex}`.
    pub fn format(self: JobRecord, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try writeJobId(w, self.job_id);
    }
};

pub fn writeJobId(w: *std.Io.Writer, job_id: u64) std.Io.Writer.Error!void {
    try w.print("job-{x:0>16}", .{job_id});
}

pub const JOB_ID_TEXT_LEN: usize = "job-".len + 16;

/// Strict: exactly `job-` + 16 hex digits; 0 is never a job id.
pub fn parseJobId(s: []const u8) ?u64 {
    if (s.len != JOB_ID_TEXT_LEN or !std.mem.startsWith(u8, s, "job-")) return null;
    const v = std.fmt.parseInt(u64, s[4..], 16) catch return null;
    return if (v == 0) null else v;
}

inline fn rd(comptime T: type, b: *const [@sizeOf(T)]u8) T {
    return std.mem.readInt(T, b, .little);
}

inline fn wr(comptime T: type, b: *[@sizeOf(T)]u8, v: T) void {
    std.mem.writeInt(T, b, v, .little);
}

/// The 96 B wire shape (spec §1.2, byte-exact).
pub const JobRecordV1 = extern struct {
    version_le: [2]u8,
    flags_le: [2]u8,
    job_type: u8,
    state: u8,
    throttle_pct: u8,
    error_class: u8,
    job_id_le: [8]u8,
    created_realtime_s_le: [8]u8,
    created_node_token_le: [8]u8,
    created_mount_slot_le: [4]u8,
    created_pid_le: [4]u8,
    tasks_done_le: [8]u8,
    tasks_total_le: [8]u8,
    arg0_le: [8]u8,
    arg1_le: [8]u8,
    updated_realtime_s_le: [8]u8,
    reserved: [8]u8,
    csum_le: [8]u8,

    pub const VERSION: u16 = 1;
    pub const LEN: usize = 96;

    pub fn init(r: JobRecord) JobRecordV1 {
        assert(r.job_id != 0);
        assert(r.throttle_pct <= 100);
        var w: JobRecordV1 = undefined;
        wr(u16, &w.version_le, VERSION);
        var flags: u16 = 0;
        if (r.pause_intent) flags |= FLAG_PAUSE_INTENT;
        if (r.capacity_pause) flags |= FLAG_CAPACITY_PAUSE;
        if (r.job_type.isMover()) flags |= FLAG_MOVER_CLASS;
        wr(u16, &w.flags_le, flags);
        w.job_type = @backingInt(r.job_type);
        w.state = @backingInt(r.state);
        w.throttle_pct = r.throttle_pct;
        w.error_class = @backingInt(r.error_class);
        wr(u64, &w.job_id_le, r.job_id);
        wr(u64, &w.created_realtime_s_le, r.created_realtime_s);
        wr(u64, &w.created_node_token_le, r.created_by.node_token);
        wr(u32, &w.created_mount_slot_le, r.created_by.mount_slot);
        wr(u32, &w.created_pid_le, r.created_pid);
        wr(u64, &w.tasks_done_le, r.tasks_done);
        wr(u64, &w.tasks_total_le, r.tasks_total);
        wr(u64, &w.arg0_le, r.arg0);
        wr(u64, &w.arg1_le, r.arg1);
        wr(u64, &w.updated_realtime_s_le, r.updated_realtime_s);
        w.reserved = @splat(0);
        const body = std.mem.asBytes(&w)[0 .. LEN - 8];
        wr(u64, &w.csum_le, checksum.hash64(body));
        return w;
    }

    pub fn encode(self: JobRecordV1) [LEN]u8 {
        return std.mem.toBytes(self);
    }

    /// Total: exact length, the checksum, version 1, known enum bytes,
    /// known flag bits, reserved zero, `throttle_pct ≤ 100`, `job_id ≠ 0`,
    /// the mover-class flag consistent with the type (canonical: encode ∘
    /// decode = id).
    pub fn decode(bytes: []const u8) DecodeError!JobRecord {
        if (bytes.len != LEN) return error.Corrupt;
        if (checksum.hash64(bytes[0 .. LEN - 8]) != rd(u64, bytes[LEN - 8 ..][0..8])) return error.Corrupt;
        const w = std.mem.bytesToValue(JobRecordV1, bytes[0..LEN]);
        if (rd(u16, &w.version_le) != VERSION) return error.Corrupt;
        const flags = rd(u16, &w.flags_le);
        if (flags & ~FLAGS_KNOWN != 0) return error.Corrupt;
        if (!std.mem.allEqual(u8, &w.reserved, 0)) return error.Corrupt;
        const job_type = std.enums.fromInt(JobType, w.job_type) orelse return error.Corrupt;
        const state = std.enums.fromInt(JobState, w.state) orelse return error.Corrupt;
        const error_class = std.enums.fromInt(ErrorClass, w.error_class) orelse return error.Corrupt;
        if (w.throttle_pct > 100) return error.Corrupt;
        if ((flags & FLAG_MOVER_CLASS != 0) != job_type.isMover()) return error.Corrupt;
        const job_id = rd(u64, &w.job_id_le);
        if (job_id == 0) return error.Corrupt;
        return .{
            .job_id = job_id,
            .job_type = job_type,
            .state = state,
            .throttle_pct = w.throttle_pct,
            .error_class = error_class,
            .pause_intent = flags & FLAG_PAUSE_INTENT != 0,
            .capacity_pause = flags & FLAG_CAPACITY_PAUSE != 0,
            .created_realtime_s = rd(u64, &w.created_realtime_s_le),
            .created_by = .{ .node_token = rd(u64, &w.created_node_token_le), .mount_slot = rd(u32, &w.created_mount_slot_le) },
            .created_pid = rd(u32, &w.created_pid_le),
            .tasks_done = rd(u64, &w.tasks_done_le),
            .tasks_total = rd(u64, &w.tasks_total_le),
            .arg0 = rd(u64, &w.arg0_le),
            .arg1 = rd(u64, &w.arg1_le),
            .updated_realtime_s = rd(u64, &w.updated_realtime_s_le),
        };
    }
};

comptime {
    assert(@sizeOf(JobRecordV1) == JobRecordV1.LEN);
}

// ---------------------------------------------------------------------------
// job_shard:{job_id, shard}
// ---------------------------------------------------------------------------

pub const ShardState = enum(u8) {
    assigned = 1,
    /// The lease expired or a verification failed: the fencing bumped, the
    /// destinations quarantined, the shard re-queued (§1.6).
    reclaimed = 2,
    completed = 3,
};

/// A pre-allocated UNPUBLISHED destination (`vol_tag`, `block_idx`): the
/// only offsets a worker may write (KD-15).
pub const DestTuple = struct {
    vol_tag: u64,
    block_idx: u64,

    pub fn eql(a: DestTuple, b: DestTuple) bool {
        return a.vol_tag == b.vol_tag and a.block_idx == b.block_idx;
    }
};

pub const DestTupleWire = extern struct {
    vol_tag_le: [8]u8,
    block_idx_le: [8]u8,

    pub const LEN: usize = 16;
};

/// The shard's destination window IS the per-worker in-flight window
/// (`EVACUATE_INFLIGHT_WINDOW_BLOCKS`, §1.5): 64 × 4 MiB = 256 MiB per
/// worker. The count is checked against the byte length before any tuple
/// is read; the max record is 56 + 1024 + 8 = 1088 B.
pub const SHARD_DEST_MAX: usize = 64;

pub const JobShardV1Head = extern struct {
    version_le: [2]u8,
    flags_le: [2]u8,
    state: u8,
    reserved0: [3]u8,
    job_id_le: [8]u8,
    shard_le: [4]u8,
    n_dest_le: [4]u8,
    holder_node_token_le: [8]u8,
    holder_mount_slot_le: [4]u8,
    lease_ttl_ms_le: [4]u8,
    shard_fencing_le: [8]u8,
    updated_realtime_s_le: [8]u8,

    pub const VERSION: u16 = 1;
    pub const LEN: usize = 56;
};

comptime {
    assert(@sizeOf(JobShardV1Head) == JobShardV1Head.LEN);
    assert(@sizeOf(DestTupleWire) == DestTupleWire.LEN);
}

pub const JOB_SHARD_MAX_LEN: usize = JobShardV1Head.LEN + SHARD_DEST_MAX * DestTupleWire.LEN + 8;

/// The decoded shard record: the lease words + the destination window.
pub const JobShard = struct {
    job_id: u64,
    shard: u32,
    state: ShardState,
    /// The holder's stable identity (zero when unassigned/reclaimed).
    holder: StableIdentity = .{ .node_token = 0, .mount_slot = 0 },
    lease_ttl_ms: u32,
    /// Persisted at every bump (quirk §6 #11): a coordinator restart seeds
    /// the live word from it and never re-issues a fencing a zombie holds.
    shard_fencing: u64,
    updated_realtime_s: u64,
    dests: []const DestTuple,

    pub fn encodedLen(self: JobShard) usize {
        return JobShardV1Head.LEN + self.dests.len * DestTupleWire.LEN + 8;
    }

    /// Encode into `out` (≥ `encodedLen`); asserts the window bound.
    pub fn encodeInto(self: JobShard, out: []u8) []u8 {
        assert(self.dests.len <= SHARD_DEST_MAX);
        assert(self.job_id != 0);
        const n = self.encodedLen();
        assert(out.len >= n);
        var h: JobShardV1Head = undefined;
        wr(u16, &h.version_le, JobShardV1Head.VERSION);
        wr(u16, &h.flags_le, 0);
        h.state = @backingInt(self.state);
        h.reserved0 = @splat(0);
        wr(u64, &h.job_id_le, self.job_id);
        wr(u32, &h.shard_le, self.shard);
        wr(u32, &h.n_dest_le, @intCast(self.dests.len));
        wr(u64, &h.holder_node_token_le, self.holder.node_token);
        wr(u32, &h.holder_mount_slot_le, self.holder.mount_slot);
        wr(u32, &h.lease_ttl_ms_le, self.lease_ttl_ms);
        wr(u64, &h.shard_fencing_le, self.shard_fencing);
        wr(u64, &h.updated_realtime_s_le, self.updated_realtime_s);
        @memcpy(out[0..JobShardV1Head.LEN], std.mem.asBytes(&h));
        var off: usize = JobShardV1Head.LEN;
        for (self.dests) |d| {
            var t: DestTupleWire = undefined;
            wr(u64, &t.vol_tag_le, d.vol_tag);
            wr(u64, &t.block_idx_le, d.block_idx);
            @memcpy(out[off..][0..DestTupleWire.LEN], std.mem.asBytes(&t));
            off += DestTupleWire.LEN;
        }
        wr(u64, out[off..][0..8], checksum.hash64(out[0..off]));
        return out[0..n];
    }

    pub fn encodeAlloc(self: JobShard, gpa: std.mem.Allocator) std.mem.Allocator.Error![]u8 {
        const buf = try gpa.alloc(u8, self.encodedLen());
        return self.encodeInto(buf);
    }

    /// Total: the head's exact presence, version, flags/reserved zero, a
    /// known state, `n_dest ≤ 64` AND exactly `n_dest` tuples + the
    /// checksum present (the count is checked against the bytes BEFORE
    /// any tuple is read), the checksum. `dests` views `gpa`-owned
    /// memory the caller frees.
    pub fn decode(gpa: std.mem.Allocator, bytes: []const u8) (DecodeError || std.mem.Allocator.Error)!JobShard {
        if (bytes.len < JobShardV1Head.LEN + 8) return error.Corrupt;
        const h = std.mem.bytesToValue(JobShardV1Head, bytes[0..JobShardV1Head.LEN]);
        if (rd(u16, &h.version_le) != JobShardV1Head.VERSION) return error.Corrupt;
        if (rd(u16, &h.flags_le) != 0 or !std.mem.allEqual(u8, &h.reserved0, 0)) return error.Corrupt;
        const state = std.enums.fromInt(ShardState, h.state) orelse return error.Corrupt;
        const n_dest = rd(u32, &h.n_dest_le);
        if (n_dest > SHARD_DEST_MAX) return error.Corrupt;
        const n = JobShardV1Head.LEN + @as(usize, n_dest) * DestTupleWire.LEN + 8;
        if (bytes.len != n) return error.Corrupt;
        if (checksum.hash64(bytes[0 .. n - 8]) != rd(u64, bytes[n - 8 ..][0..8])) return error.Corrupt;
        const job_id = rd(u64, &h.job_id_le);
        if (job_id == 0) return error.Corrupt;
        const dests = try gpa.alloc(DestTuple, n_dest);
        errdefer gpa.free(dests);
        for (dests, 0..) |*d, i| {
            const t = std.mem.bytesToValue(DestTupleWire, bytes[JobShardV1Head.LEN + i * DestTupleWire.LEN ..][0..DestTupleWire.LEN]);
            d.* = .{ .vol_tag = rd(u64, &t.vol_tag_le), .block_idx = rd(u64, &t.block_idx_le) };
        }
        return .{
            .job_id = job_id,
            .shard = rd(u32, &h.shard_le),
            .state = state,
            .holder = .{ .node_token = rd(u64, &h.holder_node_token_le), .mount_slot = rd(u32, &h.holder_mount_slot_le) },
            .lease_ttl_ms = rd(u32, &h.lease_ttl_ms_le),
            .shard_fencing = rd(u64, &h.shard_fencing_le),
            .updated_realtime_s = rd(u64, &h.updated_realtime_s_le),
            .dests = dests,
        };
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

const fixture: JobRecord = .{
    .job_id = 0x0123_4567_89ab_cdef,
    .job_type = .evacuate_volume,
    .state = .running,
    .throttle_pct = 25,
    .pause_intent = true,
    .created_realtime_s = 1_800_000_000,
    .created_by = .{ .node_token = 0xAAAA_BBBB_CCCC_DDDD, .mount_slot = 7 },
    .created_pid = 4242,
    .tasks_done = 100,
    .tasks_total = 1000,
    .arg0 = 0x5555_6666_7777_8888,
    .updated_realtime_s = 1_800_000_060,
};

test "the job tags are 13/14 with ids 8/12 B inside KEY_MAX_LEN; one prefix walk lists jobs in id order; the shard key carries its job id and shard" {
    try testing.expectEqual(@as(u8, 13), @backingInt(Tag.job));
    try testing.expectEqual(@as(u8, 14), @backingInt(Tag.job_shard));
    try testing.expectEqual(@as(usize, 10), Tag.job.keyLen());
    try testing.expectEqual(@as(usize, 14), Tag.job_shard.keyLen());
    try testing.expect(Tag.job_shard.keyLen() <= control_records.KEY_MAX_LEN);
    const a = jobKey(2);
    const b = jobKey(10);
    try testing.expect(std.mem.lessThan(u8, a.slice(), b.slice()));
    try testing.expectEqual(@as(?u64, 10), jobIdOfKey(b.slice()));
    const s = jobShardKey(10, 3);
    try testing.expectEqual(@as(?u64, 10), jobIdOfKey(s.slice()));
    try testing.expectEqual(@as(?u32, 3), shardOfKey(s.slice()));
    try testing.expectEqual(@as(?u32, null), shardOfKey(b.slice()));
    const r = rangeOf(.job);
    try testing.expect(std.mem.lessThan(u8, r.start.slice(), a.slice()) and std.mem.lessThan(u8, b.slice(), r.end.slice()));
    // The enroll record and `set_config` are not in the walk's range.
    try testing.expectEqual(@as(?u64, null), jobIdOfKey("set_config"));
}

test "JobRecordV1 is 96 B at the spec's offsets; encode∘decode = id; the mover-class flag follows the type; the pause intent survives" {
    try testing.expectEqual(@as(usize, 96), JobRecordV1.LEN);
    const bytes = fixture.encode();
    try testing.expectEqual(@as(u16, 1), std.mem.readInt(u16, bytes[0..2], .little));
    try testing.expectEqual(FLAG_PAUSE_INTENT | FLAG_MOVER_CLASS, std.mem.readInt(u16, bytes[2..4], .little));
    try testing.expectEqual(@as(u8, 2), bytes[4]);
    try testing.expectEqual(@as(u8, 2), bytes[5]);
    try testing.expectEqual(@as(u8, 25), bytes[6]);
    try testing.expectEqual(fixture.job_id, std.mem.readInt(u64, bytes[8..16], .little));
    const back = try JobRecordV1.decode(&bytes);
    try testing.expectEqual(fixture, back);
    try testing.expect(!back.isTerminal());
    var buf: [64]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try w.print("{f}", .{back});
    try testing.expectEqualStrings("job-0123456789abcdef", w.buffered());
    try testing.expectEqual(@as(?u64, fixture.job_id), parseJobId("job-0123456789abcdef"));
    try testing.expectEqual(@as(?u64, null), parseJobId("job-0000000000000000"));
    try testing.expectEqual(@as(?u64, null), parseJobId("vol-0123456789abcdef"));
}

test "JobRecordV1 decode is total: a tampered byte, a wrong version, an unknown type/state/error byte, an unknown flag bit, a reserved bit, a throttle > 100, job id 0 and a wrong length are Corrupt" {
    const good = fixture.encode();
    var b = good;
    b[20] ^= 1;
    try testing.expectError(error.Corrupt, JobRecordV1.decode(&b));
    b = good;
    b[0] = 2;
    try testing.expectError(error.Corrupt, JobRecordV1.decode(&restamp(b)));
    b = good;
    b[4] = 200;
    try testing.expectError(error.Corrupt, JobRecordV1.decode(&restamp(b)));
    b = good;
    b[5] = 0;
    try testing.expectError(error.Corrupt, JobRecordV1.decode(&restamp(b)));
    b = good;
    b[7] = 99;
    try testing.expectError(error.Corrupt, JobRecordV1.decode(&restamp(b)));
    b = good;
    b[2] |= 0x80;
    try testing.expectError(error.Corrupt, JobRecordV1.decode(&restamp(b)));
    b = good;
    b[80] = 1;
    try testing.expectError(error.Corrupt, JobRecordV1.decode(&restamp(b)));
    b = good;
    b[6] = 101;
    try testing.expectError(error.Corrupt, JobRecordV1.decode(&restamp(b)));
    b = good;
    @memset(b[8..16], 0);
    try testing.expectError(error.Corrupt, JobRecordV1.decode(&restamp(b)));
    // The mover flag on a non-mover type is a lie.
    b = good;
    b[4] = @backingInt(JobType.noop);
    try testing.expectError(error.Corrupt, JobRecordV1.decode(&restamp(b)));
    try testing.expectError(error.Corrupt, JobRecordV1.decode(good[0..95]));
}

fn restamp(b: [JobRecordV1.LEN]u8) [JobRecordV1.LEN]u8 {
    var out = b;
    std.mem.writeInt(u64, out[88..96], checksum.hash64(out[0..88]), .little);
    return out;
}

test "JobShardV1: the head is 56 B; encode∘decode = id with 0, 1 and 64 destinations; the count is checked before any tuple; 65 destinations, a short tail, a flipped bit and state 0 are Corrupt" {
    const gpa = testing.allocator;
    try testing.expectEqual(@as(usize, 56), JobShardV1Head.LEN);
    try testing.expectEqual(@as(usize, 1088), JOB_SHARD_MAX_LEN);
    var dests: [SHARD_DEST_MAX + 1]DestTuple = undefined;
    for (&dests, 0..) |*d, i| d.* = .{ .vol_tag = 0x1000 + i, .block_idx = 77 * i };
    for ([_]usize{ 0, 1, 64 }) |n| {
        const s: JobShard = .{ .job_id = 9, .shard = 3, .state = .assigned, .holder = .{ .node_token = 1, .mount_slot = 2 }, .lease_ttl_ms = 30_000, .shard_fencing = 5, .updated_realtime_s = 100, .dests = dests[0..n] };
        const bytes = try s.encodeAlloc(gpa);
        defer gpa.free(bytes);
        try testing.expectEqual(JobShardV1Head.LEN + n * 16 + 8, bytes.len);
        const back = try JobShard.decode(gpa, bytes);
        defer gpa.free(back.dests);
        try testing.expectEqual(s.job_id, back.job_id);
        try testing.expectEqual(s.shard, back.shard);
        try testing.expectEqual(s.state, back.state);
        try testing.expectEqual(s.holder, back.holder);
        try testing.expectEqual(s.shard_fencing, back.shard_fencing);
        try testing.expectEqualSlices(DestTuple, s.dests, back.dests);
        // A lying count: more tuples claimed than bytes present.
        var lie = try gpa.dupe(u8, bytes);
        defer gpa.free(lie);
        std.mem.writeInt(u32, lie[20..24], @intCast(n + 1), .little);
        try testing.expectError(error.Corrupt, JobShard.decode(gpa, lie));
        if (bytes.len > 64) {
            try testing.expectError(error.Corrupt, JobShard.decode(gpa, bytes[0 .. bytes.len - 1]));
            var flipped = try gpa.dupe(u8, bytes);
            defer gpa.free(flipped);
            flipped[60] ^= 0x40;
            try testing.expectError(error.Corrupt, JobShard.decode(gpa, flipped));
        }
    }
    var over = try (JobShard{ .job_id = 9, .shard = 0, .state = .completed, .lease_ttl_ms = 0, .shard_fencing = 0, .updated_realtime_s = 0, .dests = dests[0..64] }).encodeAlloc(gpa);
    defer gpa.free(over);
    std.mem.writeInt(u32, over[20..24], 65, .little);
    try testing.expectError(error.Corrupt, JobShard.decode(gpa, over));
    var zero_state = try (JobShard{ .job_id = 9, .shard = 0, .state = .completed, .lease_ttl_ms = 0, .shard_fencing = 0, .updated_realtime_s = 0, .dests = &.{} }).encodeAlloc(gpa);
    defer gpa.free(zero_state);
    zero_state[4] = 0;
    std.mem.writeInt(u64, zero_state[zero_state.len - 8 ..][0..8], checksum.hash64(zero_state[0 .. zero_state.len - 8]), .little);
    try testing.expectError(error.Corrupt, JobShard.decode(gpa, zero_state));
}

fn recordsTotal(gpa: std.mem.Allocator, bytes: []const u8) !void {
    if (JobRecordV1.decode(bytes)) |r| {
        try testing.expectEqualSlices(u8, bytes, &r.encode());
    } else |_| {}
    if (JobShard.decode(gpa, bytes)) |s| {
        defer gpa.free(s.dests);
        const again = try s.encodeAlloc(gpa);
        defer gpa.free(again);
        try testing.expectEqualSlices(u8, bytes, again);
    } else |_| {}
    _ = parseJobId(bytes);
}

fn recordsFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [JOB_SHARD_MAX_LEN + 16]u8 = undefined;
    const len = smith.slice(&buf);
    try recordsTotal(testing.allocator, buf[0..len]);
}

test "fuzz: the job record, job shard and job-id decoders are total on arbitrary bytes; whatever decodes re-encodes to itself" {
    try fuzzkit.fuzz({}, recordsFuzz, .{ .corpus = &.{fuzzkit.slice("job-00000000000000ff")} });
}

test "property: neither durable decoder panics on mutations of valid records; a byte string that decodes re-encodes to itself" {
    const gpa = testing.allocator;
    var prng = std.Random.DefaultPrng.init(0x6a6f62);
    const rnd = prng.random();
    var buf: [JOB_SHARD_MAX_LEN + 16]u8 = undefined;
    var round: usize = 0;
    while (round < 4000) : (round += 1) {
        const len = rnd.uintLessThan(usize, buf.len);
        rnd.bytes(buf[0..len]);
        // Half the rounds start from a valid record so the inner checks
        // (not just the checksum) see traffic.
        if (round & 1 == 1 and len >= JobRecordV1.LEN) {
            var r = fixture;
            r.job_type = rnd.enumValue(JobType);
            r.state = rnd.enumValue(JobState);
            r.throttle_pct = rnd.uintAtMost(u8, 100);
            buf[0..96].* = r.encode();
            const flips = rnd.uintLessThan(usize, 3);
            var f: usize = 0;
            while (f < flips) : (f += 1) buf[rnd.uintLessThan(usize, 88)] ^= @as(u8, 1) << rnd.int(u3);
            if (flips > 0) std.mem.writeInt(u64, buf[88..96], checksum.hash64(buf[0..88]), .little);
        }
        if (JobRecordV1.decode(buf[0..len])) |r| {
            try testing.expectEqualSlices(u8, buf[0..len], &r.encode());
        } else |_| {}
        if (JobShard.decode(gpa, buf[0..len])) |s| {
            defer gpa.free(s.dests);
            const again = try s.encodeAlloc(gpa);
            defer gpa.free(again);
            try testing.expectEqualSlices(u8, buf[0..len], again);
        } else |_| {}
    }
}

test "the type predicates: movers are evacuate/rebalance/defrag_data/defrag_pack; only noop is wire-executable; terminal states are cancelled/completed/failed" {
    try testing.expect(JobType.evacuate_volume.isMover() and JobType.rebalance.isMover() and JobType.defrag_data.isMover() and JobType.defrag_pack.isMover());
    try testing.expect(!JobType.noop.isMover() and !JobType.fsck.isMover() and !JobType.defrag_meta.isMover() and !JobType.defrag_fold.isMover() and !JobType.migrate_meta_slot.isMover());
    inline for (@typeInfo(JobType).@"enum".field_names) |n| {
        const t = @field(JobType, n);
        try testing.expectEqual(t == .noop, t.wireExecutable());
    }
    try testing.expect(JobState.cancelled.isTerminal() and JobState.completed.isTerminal() and JobState.failed.isTerminal());
    try testing.expect(!JobState.queued.isTerminal() and !JobState.running.isTerminal() and !JobState.paused.isTerminal() and !JobState.paused_capacity.isTerminal());
    try testing.expect(JobState.paused.isPaused() and JobState.paused_capacity.isPaused() and !JobState.queued.isPaused());
}
