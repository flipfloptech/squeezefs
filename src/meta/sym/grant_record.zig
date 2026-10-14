//! An appender's extent grant record as a FAMILY of chunk records
//! (charter §5.5 "Extent grants"; decision 0003 §AE.4): the manager's
//! attestation of appender `a`'s whole grant is the union of the runs its
//! chunks `extent_grant:{a}:{id}` carry (`control_records.zig`), each chunk
//! stamped, a non-empty normalized run list of at most `GRANT_CHUNK_RUNS`
//! runs, the chunks pairwise disjoint. A record has no
//! size bound but the volume's extents: a custody move of any tree, a carve
//! or a return rewrites only the chunks whose extents it changes.
//!
//! Every change is a REPLACEMENT (`Family.plan`): each effective chunk owns
//! the extent range from its first run to the next chunk's first run (the
//! first chunk from 0); the target run set is cut at those bounds and every
//! chunk whose share changed is deleted and its share written as fresh
//! chunks under ids never used in the family. A replacement that fits the
//! entry of the change it belongs to (the slot lease's, the carve's alloc
//! deltas, the return's frees) rides it: one atomic entry. One that does
//! not is STAGED (`Writes`): the first entry writes the MOVE MARKER naming
//! the fresh ids `[lo, hi]` — chunks inside a present marker's range are
//! ineffective to every reader — the fresh chunks follow in as many entries
//! as they need, and the change's own entry deletes the replaced chunks and
//! the marker. Before that entry lands every reader sees the old record; at
//! it, the new. A marker found at a later load is a staged change that
//! never landed (a crash, a failed entry): its chunks and the marker are
//! deleted before the next change plans (`Family.sweepOps`), and no reader
//! counts them meanwhile.
//!
//! PORTABLE: pure over the decoded chunks; the manager and the recovery
//! driver turn the ops into control records.

const std = @import("std");
const cr = @import("control_records.zig");
const grant = @import("extent_grant.zig");
const journal = @import("../journal.zig");
const geometry = @import("../geometry.zig");
const node = @import("../node.zig");

const Allocator = std.mem.Allocator;
const GrantRun = cr.GrantRun;
const assert = std.debug.assert;

pub const Error = error{ Corrupt, OutOfMemory };

comptime {
    // Every node size the format admits stores a full chunk as one value.
    assert(geometry.recordValueCap(node.MIN_NODE_SIZE) >= cr.GRANT_CHUNK_BYTES_MAX);
}

/// One effective chunk: its id and its runs (the family's arena).
pub const Chunk = struct { id: u64, runs: []const GrantRun };

/// One appender's family as a range walk of `extentGrantFamily` read it.
pub const Family = struct {
    arena: std.heap.ArenaAllocator,
    appender_id: u32,
    /// The effective chunks, ascending by first run, pairwise disjoint.
    chunks: []Chunk,
    /// The record: the effective chunks' runs merged (adjacent runs across
    /// a chunk bound coalesced) — normalized.
    record: []GrantRun,
    /// A present marker's range, and the chunk ids it stages that exist.
    marker: ?struct { lo: u64, hi: u64 } = null,
    staged: []u64,
    /// The first id no chunk of the family ever took: one past the largest
    /// present (an empty family starts again at 0).
    next_id: u64,

    pub fn deinit(self: *Family) void {
        self.arena.deinit();
    }

    /// `kvs`: every key/value of the appender's family range (any order;
    /// each item has `.key` and `.value`). Every chunk verified
    /// (`decodeGrantChunk`), the effective ones disjoint — else `Corrupt`.
    pub fn load(gpa: Allocator, appender_id: u32, kvs: anytype) Error!Family {
        var self: Family = .{ .arena = .init(gpa), .appender_id = appender_id, .chunks = &.{}, .record = &.{}, .staged = &.{}, .next_id = 0 };
        errdefer self.arena.deinit();
        const a = self.arena.allocator();
        var all: std.ArrayList(Chunk) = .empty;
        for (kvs) |kv| {
            if (cr.appenderOfKey(kv.key) != appender_id) return error.Corrupt;
            const id = cr.chunkOfKey(kv.key) orelse return error.Corrupt;
            switch (try cr.decodeGrantChunk(appender_id, id, kv.value)) {
                .marker => |m| self.marker = .{ .lo = m.lo, .hi = m.hi },
                .runs => |d| {
                    const runs = try a.alloc(GrantRun, d.n_runs);
                    _ = d.collect(runs);
                    try all.append(a, .{ .id = id, .runs = runs });
                    self.next_id = @max(self.next_id, id + 1);
                },
            }
        }
        var eff: std.ArrayList(Chunk) = .empty;
        var staged: std.ArrayList(u64) = .empty;
        for (all.items) |c| {
            if (self.marker) |m| if (c.id >= m.lo and c.id <= m.hi) {
                try staged.append(a, c.id);
                continue;
            };
            try eff.append(a, c);
        }
        std.mem.sort(Chunk, eff.items, {}, struct {
            fn lt(_: void, x: Chunk, y: Chunk) bool {
                return x.runs[0].start < y.runs[0].start;
            }
        }.lt);
        var merged: std.ArrayList(GrantRun) = .empty;
        var prev_end: u64 = 0;
        for (eff.items, 0..) |c, i| {
            // Disjoint: a chunk starts at or past the end of the one before.
            if (i > 0 and c.runs[0].start < prev_end) return error.Corrupt;
            for (c.runs) |r| {
                if (merged.items.len > 0 and merged.items[merged.items.len - 1].end() == r.start) {
                    // An extent count never exceeds u32 (spec Q-12): a merge
                    // past it is no record the format admits.
                    const last = &merged.items[merged.items.len - 1];
                    last.len = std.math.add(u32, last.len, r.len) catch return error.Corrupt;
                } else try merged.append(a, r);
            }
            prev_end = c.runs[c.runs.len - 1].end();
        }
        self.chunks = eff.items;
        self.staged = staged.items;
        self.record = merged.items;
        return self;
    }

    pub fn contains(self: *const Family, extent: u64) bool {
        return grant.runsContain(self.record, extent);
    }

    pub fn total(self: *const Family) u64 {
        var n: u64 = 0;
        for (self.record) |r| n += r.len;
        return n;
    }

    /// The deletes that clear a marker that never landed: its staged
    /// chunks, then the marker (the marker LAST — while it stands, the
    /// staged chunks count for no reader, so any prefix is safe).
    pub fn sweepOps(self: *const Family, gpa: Allocator) Allocator.Error![]Op {
        var ops: std.ArrayList(Op) = .empty;
        errdefer ops.deinit(gpa);
        if (self.marker == null) return ops.toOwnedSlice(gpa);
        for (self.staged) |id| try ops.append(gpa, .{ .delete = cr.extentGrantKey(self.appender_id, id) });
        try ops.append(gpa, .{ .delete = cr.extentGrantKey(self.appender_id, cr.GRANT_MARKER_ID) });
        return ops.toOwnedSlice(gpa);
    }

    /// The replacement that turns the record into `target` (normalized):
    /// the chunks whose owned share changed are deleted, each share written
    /// as fresh chunks of at most `max_runs` runs. A family with a marker
    /// must be swept first.
    pub fn plan(self: *const Family, gpa: Allocator, target: []const GrantRun, max_runs: u32) Error!Plan {
        assert(self.marker == null and cr.runsNormalized(target) and max_runs > 0);
        var p: Plan = .{ .arena = .init(gpa), .appender_id = self.appender_id, .first_id = self.next_id };
        errdefer p.arena.deinit();
        const a = p.arena.allocator();
        var deletes: std.ArrayList(u64) = .empty;
        var puts: std.ArrayList(Chunk) = .empty;
        var next = self.next_id;
        var share: std.ArrayList(GrantRun) = .empty;
        var t: usize = 0; // the first target run not wholly below the window
        const n_owners = @max(self.chunks.len, 1);
        for (0..n_owners) |i| {
            const lo: u64 = if (i == 0) 0 else self.chunks[i].runs[0].start;
            const hi: u64 = if (i + 1 < self.chunks.len) self.chunks[i + 1].runs[0].start else std.math.maxInt(u64);
            share.clearRetainingCapacity();
            while (t < target.len and target[t].end() <= lo) t += 1;
            var j = t;
            while (j < target.len and target[j].start < hi) : (j += 1) {
                const s = @max(target[j].start, lo);
                const e = @min(target[j].end(), hi);
                if (e > s) try share.append(a, .{ .start = s, .len = @intCast(e - s) });
            }
            const old: []const GrantRun = if (self.chunks.len == 0) &.{} else self.chunks[i].runs;
            if (sameRuns(old, share.items)) continue;
            if (self.chunks.len != 0) try deletes.append(a, self.chunks[i].id);
            var k: usize = 0;
            while (k < share.items.len) {
                const n = @min(share.items.len - k, max_runs);
                try puts.append(a, .{ .id = next, .runs = try a.dupe(GrantRun, share.items[k..][0..n]) });
                next += 1;
                k += n;
            }
        }
        // The fresh ids stay below the marker's (an id near it on the device
        // is no family this code wrote).
        if (next >= cr.GRANT_MARKER_ID) return error.Corrupt;
        p.deletes = deletes.items;
        p.puts = puts.items;
        return p;
    }
};

fn sameRuns(a: []const GrantRun, b: []const GrantRun) bool {
    if (a.len != b.len) return false;
    for (a, b) |x, y| if (x.start != y.start or x.len != y.len) return false;
    return true;
}

/// A replacement: the ids it deletes and the chunks it writes (fresh ids
/// `first_id …`, ascending).
pub const Plan = struct {
    arena: std.heap.ArenaAllocator,
    appender_id: u32,
    first_id: u64,
    deletes: []const u64 = &.{},
    puts: []const Chunk = &.{},

    pub fn deinit(self: *Plan) void {
        self.arena.deinit();
    }

    pub fn empty(self: *const Plan) bool {
        return self.deletes.len == 0 and self.puts.len == 0;
    }
};

/// One record change: a put of an encoded value, or a delete.
pub const Op = union(enum) {
    put: struct { key: cr.ControlKey, value: []const u8 },
    delete: cr.ControlKey,

    /// The journal bytes this op costs in a control entry.
    pub fn frameLen(self: *const Op) u64 {
        return switch (self.*) {
            .put => |p| journal.recordFrameLen(p.key.len, p.value.len),
            .delete => |k| journal.recordFrameLen(k.len, 0),
        };
    }
};

/// A plan as entries: `staged` (written in order BEFORE the change's own
/// entry; empty when the replacement rides that entry whole) and `final`
/// (appended to the change's own entry). A change whose own entry never
/// lands leaves a marker: the next load's `sweepOps` clears it.
pub const Writes = struct {
    arena: std.heap.ArenaAllocator,
    staged: []const []const Op = &.{},
    final: []const Op = &.{},

    pub fn deinit(self: *Writes) void {
        self.arena.deinit();
    }

    /// `final_room`: the payload bytes the change's own entry leaves for
    /// the record's ops (the entry's payload cap less the change's own
    /// records). Staged entries take up to `entry_room` each.
    pub fn build(gpa: Allocator, p: *const Plan, final_room: u64, entry_room: u64) error{ OutOfMemory, EntryTooLarge }!Writes {
        var w: Writes = .{ .arena = .init(gpa) };
        errdefer w.arena.deinit();
        const a = w.arena.allocator();
        var puts: std.ArrayList(Op) = .empty;
        var put_bytes: u64 = 0;
        for (p.puts) |c| {
            const buf = try a.alloc(u8, cr.grantChunkLen(c.runs.len));
            const v = cr.encodeGrantChunk(p.appender_id, c.id, c.runs, buf) catch unreachable;
            try puts.append(a, .{ .put = .{ .key = cr.extentGrantKey(p.appender_id, c.id), .value = v } });
            put_bytes += puts.items[puts.items.len - 1].frameLen();
        }
        var deletes: std.ArrayList(Op) = .empty;
        var delete_bytes: u64 = 0;
        for (p.deletes) |id| {
            try deletes.append(a, .{ .delete = cr.extentGrantKey(p.appender_id, id) });
            delete_bytes += deletes.items[deletes.items.len - 1].frameLen();
        }
        if (put_bytes + delete_bytes <= final_room) {
            try deletes.appendSlice(a, puts.items);
            w.final = deletes.items;
            return w;
        }
        // Staged: the marker first, the fresh chunks after, the change's own
        // entry deletes the replaced chunks and the marker.
        const marker_key = cr.extentGrantKey(p.appender_id, cr.GRANT_MARKER_ID);
        const marker_del: Op = .{ .delete = marker_key };
        // The change's own entry must carry the replaced chunks' deletes
        // and the marker's: 32 B each, ≈ 2,000 chunks (≈ 500 k runs) of
        // room beside the largest change record — the envelope, stated.
        if (p.puts.len == 0 or delete_bytes + marker_del.frameLen() > final_room) return error.EntryTooLarge;
        const mbuf = try a.create([cr.ExtentGrantChunkHead.LEN]u8);
        const last_id = p.puts[p.puts.len - 1].id;
        const marker_put: Op = .{ .put = .{ .key = marker_key, .value = cr.encodeGrantMarker(p.appender_id, p.first_id, last_id, mbuf) catch unreachable } };
        var entries: std.ArrayList([]const Op) = .empty;
        var cur: std.ArrayList(Op) = .empty;
        try cur.append(a, marker_put);
        var cur_bytes = marker_put.frameLen();
        for (puts.items) |op| {
            if (cur_bytes + op.frameLen() > entry_room) {
                try entries.append(a, cur.items);
                cur = .empty;
                cur_bytes = 0;
            }
            try cur.append(a, op);
            cur_bytes += op.frameLen();
        }
        try entries.append(a, cur.items);
        try deletes.append(a, marker_del);
        w.staged = entries.items;
        w.final = deletes.items;
        return w;
    }
};

/// The payload bytes a staged entry may fill: half the entry's payload cap,
/// so every staged entry admits beside the user class's commits it competes
/// with (`tryAdmit(len, .user)`) without approaching the window a whole
/// 128 KiB entry needs.
pub fn stagedEntryRoom() u64 {
    return journal.entryPayloadCap() / 2;
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

const KV = struct { key: []const u8, value: []const u8 };

/// A RAM tree 0 holding one appender's family: applies ops like an entry.
const Tree0 = struct {
    gpa: Allocator,
    map: std.StringArrayHashMapUnmanaged([]u8) = .empty,

    fn deinit(self: *Tree0) void {
        for (self.map.keys(), self.map.values()) |k, v| {
            self.gpa.free(k);
            self.gpa.free(v);
        }
        self.map.deinit(self.gpa);
    }

    fn apply(self: *Tree0, ops: []const Op) !void {
        for (ops) |op| switch (op) {
            .put => |p| {
                const gop = try self.map.getOrPut(self.gpa, p.key.slice());
                if (gop.found_existing) self.gpa.free(gop.value_ptr.*) else gop.key_ptr.* = try self.gpa.dupe(u8, p.key.slice());
                gop.value_ptr.* = try self.gpa.dupe(u8, p.value);
            },
            .delete => |k| if (self.map.fetchOrderedRemove(k.slice())) |kv| {
                self.gpa.free(kv.key);
                self.gpa.free(kv.value);
            },
        };
    }

    fn family(self: *Tree0, appender_id: u32) !Family {
        var kvs: std.ArrayList(KV) = .empty;
        defer kvs.deinit(self.gpa);
        for (self.map.keys(), self.map.values()) |k, v| if (cr.appenderOfKey(k) == appender_id) try kvs.append(self.gpa, .{ .key = k, .value = v });
        return Family.load(self.gpa, appender_id, kvs.items);
    }

    fn count(self: *Tree0) usize {
        return self.map.count();
    }
};

fn singles(gpa: Allocator, from: u64, n: usize, stride: u64) ![]GrantRun {
    const runs = try gpa.alloc(GrantRun, n);
    for (runs, 0..) |*r, i| r.* = .{ .start = from + i * stride, .len = 1 };
    return runs;
}

fn change(t0: *Tree0, appender_id: u32, target: []const GrantRun, max_runs: u32) !void {
    var fam = try t0.family(appender_id);
    defer fam.deinit();
    var p = try fam.plan(testing.allocator, target, max_runs);
    defer p.deinit();
    var w = try Writes.build(testing.allocator, &p, journal.entryPayloadCap(), stagedEntryRoom());
    defer w.deinit();
    for (w.staged) |e| try t0.apply(e);
    try t0.apply(w.final);
}

test "a grant record past one chunk is a family: a replacement rewrites only the chunks whose share changed, the record reads back whole, chunk ids are never reused while the family stands" {
    const gpa = testing.allocator;
    var t0: Tree0 = .{ .gpa = gpa };
    defer t0.deinit();
    // 1,000 scattered single-extent runs at 64 runs a chunk: 16 chunks.
    const base = try singles(gpa, 1000, 1000, 3);
    defer gpa.free(base);
    try change(&t0, 4, base, 64);
    try testing.expectEqual(@as(usize, 16), t0.count());
    {
        var fam = try t0.family(4);
        defer fam.deinit();
        try testing.expectEqualSlices(GrantRun, base, fam.record);
        try testing.expect(fam.contains(1000 + 3 * 999) and !fam.contains(1001));
    }
    // One extent added inside the 6th chunk's range (adjacent to one of its
    // runs, so that run grows): one delete, one put.
    var plus: std.ArrayList(GrantRun) = .empty;
    defer plus.deinit(gpa);
    try plus.appendSlice(gpa, base);
    plus.items[5 * 64 + 10].len = 2;
    {
        var fam = try t0.family(4);
        defer fam.deinit();
        var p = try fam.plan(gpa, plus.items, 64);
        defer p.deinit();
        try testing.expectEqual(@as(usize, 1), p.deletes.len);
        try testing.expectEqual(@as(usize, 1), p.puts.len);
        try testing.expectEqual(@as(u64, 16), p.puts[0].id);
    }
    try change(&t0, 4, plus.items, 64);
    {
        var fam = try t0.family(4);
        defer fam.deinit();
        try testing.expectEqualSlices(GrantRun, plus.items, fam.record);
        try testing.expectEqual(@as(u64, 17), fam.next_id);
    }
    // Everything returned: the family is gone.
    try change(&t0, 4, &.{}, 64);
    try testing.expectEqual(@as(usize, 0), t0.count());
}

test "a replacement too large for its change's entry is STAGED behind the move marker: no reader counts a staged chunk before the change's own entry lands, every reader sees the whole new record after it; a marker that never landed is swept before the next change and its chunks never counted" {
    const gpa = testing.allocator;
    var t0: Tree0 = .{ .gpa = gpa };
    defer t0.deinit();
    const small = try singles(gpa, 10, 4, 2);
    defer gpa.free(small);
    try change(&t0, 9, small, 253);
    // 12,000 scattered images at the shipped chunk size: past one entry.
    const big = try singles(gpa, 100_000, 12_000, 2);
    defer gpa.free(big);
    var target: std.ArrayList(GrantRun) = .empty;
    defer target.deinit(gpa);
    try target.appendSlice(gpa, small);
    try target.appendSlice(gpa, big);
    var fam = try t0.family(9);
    var p = try fam.plan(gpa, target.items, 253);
    fam.deinit();
    defer p.deinit();
    // The change's own entry has room for its records only (a slot state):
    // the replacement cannot ride it.
    var w = try Writes.build(gpa, &p, 4096, stagedEntryRoom());
    defer w.deinit();
    try testing.expect(w.staged.len >= 2);
    for (w.staged) |e| {
        var bytes: u64 = 0;
        for (e) |op| bytes += op.frameLen();
        try testing.expect(bytes <= stagedEntryRoom());
    }
    for (w.staged) |e| {
        try t0.apply(e);
        var mid = try t0.family(9);
        defer mid.deinit();
        try testing.expectEqualSlices(GrantRun, small, mid.record);
        try testing.expect(mid.marker != null);
    }
    try t0.apply(w.final);
    {
        var after = try t0.family(9);
        defer after.deinit();
        try testing.expectEqualSlices(GrantRun, target.items, after.record);
        try testing.expect(after.marker == null and after.staged.len == 0);
    }
    // A staged change that never lands: its marker and chunks stand, count
    // for nobody, and the sweep clears them.
    const more = try singles(gpa, 500_000, 9_000, 2);
    defer gpa.free(more);
    var target3: std.ArrayList(GrantRun) = .empty;
    defer target3.deinit(gpa);
    try target3.appendSlice(gpa, target.items);
    try target3.appendSlice(gpa, more);
    var fam3 = try t0.family(9);
    var p3 = try fam3.plan(gpa, target3.items, 253);
    fam3.deinit();
    defer p3.deinit();
    var w3 = try Writes.build(gpa, &p3, 4096, stagedEntryRoom());
    defer w3.deinit();
    try t0.apply(w3.staged[0]);
    {
        var mid = try t0.family(9);
        defer mid.deinit();
        try testing.expectEqualSlices(GrantRun, target.items, mid.record);
        const sweep = try mid.sweepOps(gpa);
        defer gpa.free(sweep);
        try testing.expectEqual(mid.staged.len + 1, sweep.len);
        try t0.apply(sweep);
    }
    var swept = try t0.family(9);
    defer swept.deinit();
    try testing.expect(swept.marker == null);
    try testing.expectEqualSlices(GrantRun, target.items, swept.record);
}

test "a family that does not assemble is Corrupt: overlapping chunks, a chunk under another appender's key, a stamp that does not verify" {
    const gpa = testing.allocator;
    var b1: [256]u8 = undefined;
    var b2: [256]u8 = undefined;
    const c1 = try cr.encodeGrantChunk(3, 0, &.{.{ .start = 10, .len = 5 }}, &b1);
    const c2 = try cr.encodeGrantChunk(3, 1, &.{.{ .start = 12, .len = 1 }}, &b2);
    const k1 = cr.extentGrantKey(3, 0);
    const k2 = cr.extentGrantKey(3, 1);
    try testing.expectError(error.Corrupt, Family.load(gpa, 3, &[_]KV{ .{ .key = k1.slice(), .value = c1 }, .{ .key = k2.slice(), .value = c2 } }));
    try testing.expectError(error.Corrupt, Family.load(gpa, 4, &[_]KV{.{ .key = k1.slice(), .value = c1 }}));
    var bad = b1;
    bad[50] ^= 1;
    try testing.expectError(error.Corrupt, Family.load(gpa, 3, &[_]KV{.{ .key = k1.slice(), .value = bad[0..c1.len] }}));
    // Adjacent runs across a chunk bound are one run of the record.
    const c3 = try cr.encodeGrantChunk(3, 1, &.{.{ .start = 15, .len = 2 }}, &b2);
    var fam = try Family.load(gpa, 3, &[_]KV{ .{ .key = k2.slice(), .value = c3 }, .{ .key = k1.slice(), .value = c1 } });
    defer fam.deinit();
    try testing.expectEqualSlices(GrantRun, &.{.{ .start = 10, .len = 7 }}, fam.record);
}
