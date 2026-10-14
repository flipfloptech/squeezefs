//! The fill table, CID custody, whole-read admission and THE gather
//! (charter §6.3 "ONE gather pass fused into the caller's registered
//! destination", "engagement law `gather ≡ fill`"; `docs/porting/ops_zcrx.md`
//! §4, §2.3 MEM-3). Portable.
//!
//! One table per lane queue, `depth` preallocated cells keyed by CID (no
//! map, no per-op allocation — charter §12). A cell is written by exactly
//! one party per phase: the REQUESTER prepares it (length, reset) before
//! the capsule is queued (the command ring's release/acquire pair publishes
//! it), the DRIVER appends spans and completes it, the requester gathers
//! and releases after the completion cell published. The CID and its depth
//! permit are ONE unit (`CidPool`), returned by the requester after the
//! gather — or by the driver's drain for a requester that abandoned.
//!
//! Laws (§4): a span past the fill's length, an unknown CID or SUCCESS on
//! a non-LAST C2HData are violations (the parser's sink refusal — the
//! caller poisons); a status ≠ 0 fails the OP, the session lives;
//! `received == len` exactly at completion, else the op fails ("completed
//! with N of M bytes") — short data + success is a violation, never
//! partial data. The gather is ONE `@memcpy` per span at `dest + datao`:
//! the single CPU pass over payload bytes. `zcrx_gather_bytes` and
//! `zcrx_fill_bytes` are counted at the WHOLE-read success boundary, by the
//! session — a torn multi-segment read contributes to neither.
//!
//! Whole-read atomic admission (§4, round-8 finding I): the segment plan
//! first; per queue the TOTAL units `ceil(seg / 4096)` are taken in ONE
//! `tryAcquire` — never per segment (partial holds shredded the window);
//! any queue's refusal drops every held permit and the read DECLINES
//! (`WouldBlock`, counted once per read). Declines never park.

const std = @import("std");
const testkit = @import("../../core/testkit.zig");
const builtin = @import("builtin");
const completion = @import("../../sync/completion.zig");
const mpmc = @import("../../sync/mpmc.zig");
const derive = @import("../../core/derive.zig");
const pdu = @import("pdu.zig");
const pdu_stream = @import("pdu_stream.zig");
const span_ledger = @import("span_ledger.zig");
const area = @import("area.zig");

pub const Span = pdu_stream.Span;
pub const Seg = struct { datao: u32, span: Span };

/// Why a fill ended.
pub const Verdict = union(enum) {
    /// `received == len`; the spans are in the cell for the gather.
    ok,
    /// The controller answered a status ≠ 0: the OP failed, the session lives.
    status: u16,
    /// The data did not add up: `received` of `len` with SUCCESS/CQE.
    short: struct { received: u32, len: u32 },
    /// The capsule never reached the wire (send failure).
    send_failed,
    /// The session drained (poison, teardown, starvation failover).
    drained,
    /// Too many spans for the cell (cannot happen by construction: spans ≤
    /// chunks touched + 1; the tripwire is the caller's).
    span_records_exhausted,
};

pub const Done = completion.Completion(Verdict);

pub const Cell = struct {
    len: u32 = 0,
    received: u32 = 0,
    nsegs: u32 = 0,
    segs: []Seg = &.{},
    /// Driver-side: a capsule for this CID was accepted onto the wire path.
    pending: bool = false,
    done: Done = .{},
};

pub const Table = struct {
    cells: []Cell,
    seg_store: []Seg,
    max_segs: u32,
    /// The CIDs: a requester takes one before `prepare`, returns it after
    /// the gather; the driver returns an abandoned requester's.
    cids: CidPool,
    /// Driver-side census of pending fills.
    pending_count: u32 = 0,

    pub fn init(gpa: std.mem.Allocator, cid_count: u32, max_segs: u32) std.mem.Allocator.Error!Table {
        const cells = try gpa.alloc(Cell, cid_count);
        errdefer gpa.free(cells);
        const store = try gpa.alloc(Seg, @as(usize, cid_count) * max_segs);
        errdefer gpa.free(store);
        for (cells, 0..) |*c, i| c.* = .{ .segs = store[i * max_segs ..][0..max_segs] };
        return .{ .cells = cells, .seg_store = store, .max_segs = max_segs, .cids = try CidPool.init(gpa, cid_count) };
    }

    pub fn deinit(self: *Table, gpa: std.mem.Allocator) void {
        self.cids.deinit(gpa);
        gpa.free(self.cells);
        gpa.free(self.seg_store);
        self.* = undefined;
    }

    // ---- requester side ----

    /// Arm the cell BEFORE the capsule is queued.
    pub fn prepare(self: *Table, cid: u16, len: u32) void {
        const c = &self.cells[cid];
        c.len = len;
        c.received = 0;
        c.nsegs = 0;
        c.done.reset();
    }

    /// After `done` published `.ok`: ONE copy per span into `dest`
    /// (`dest.len == len`). Spans are not released here (`releaseSpans`).
    pub fn gatherInto(self: *const Table, cid: u16, dest: []u8, area_bytes: []const u8) void {
        const c = &self.cells[cid];
        std.debug.assert(dest.len == c.len);
        for (c.segs[0..c.nsegs]) |s| @memcpy(dest[s.datao..][0..s.span.len], area_bytes[s.span.off..][0..s.span.len]);
    }

    /// Drop every span's chunk ref (after the gather, or on a failure).
    pub fn releaseSpans(self: *Table, cid: u16, ledger: *span_ledger.SpanLedger) void {
        const c = &self.cells[cid];
        for (c.segs[0..c.nsegs]) |s| _ = ledger.release(s.span.slot);
        c.nsegs = 0;
    }

    // ---- driver side ----

    /// The capsule was taken off the command ring: the fill is pending.
    pub fn activate(self: *Table, cid: u16) void {
        const c = &self.cells[cid];
        std.debug.assert(!c.pending);
        c.pending = true;
        self.pending_count += 1;
    }

    pub fn pendingCount(self: *const Table) u32 {
        return self.pending_count;
    }

    /// The parser's sink: the §4 laws. The driver takes the chunk ref for
    /// every recorded span (the ledger is the driver's).
    pub fn apply(self: *Table, ev: pdu_stream.Event, ledger: *span_ledger.SpanLedger) error{Violation}!void {
        switch (ev) {
            .c2h_span => |s| {
                if (s.cid >= self.cells.len) return error.Violation;
                const c = &self.cells[s.cid];
                if (!c.pending) return error.Violation;
                const end = std.math.add(u32, s.datao, s.span.len) catch return error.Violation;
                if (end > c.len) return error.Violation;
                if (c.nsegs == self.max_segs) {
                    self.finish(s.cid, .span_records_exhausted, ledger);
                    return error.Violation;
                }
                ledger.addRef(s.span.slot);
                c.segs[c.nsegs] = .{ .datao = s.datao, .span = s.span };
                c.nsegs += 1;
                c.received += s.span.len;
            },
            .c2h_end => |e| {
                if (e.cid >= self.cells.len) return error.Violation;
                const c = &self.cells[e.cid];
                if (!c.pending) return error.Violation;
                if (!e.success) return;
                if (!e.last) return error.Violation;
                self.completeExact(e.cid, ledger);
            },
            .cqe => |q| {
                if (q.cid >= self.cells.len) return error.Violation;
                const c = &self.cells[q.cid];
                if (!c.pending) return error.Violation;
                if (!q.ok()) {
                    self.finish(q.cid, .{ .status = q.status }, ledger);
                    return;
                }
                self.completeExact(q.cid, ledger);
            },
        }
    }

    fn completeExact(self: *Table, cid: u16, ledger: *span_ledger.SpanLedger) void {
        const c = &self.cells[cid];
        if (c.received == c.len) {
            self.finish(cid, .ok, ledger);
        } else {
            self.finish(cid, .{ .short = .{ .received = c.received, .len = c.len } }, ledger);
        }
    }

    /// Publish the verdict. On a failure the spans are released HERE (the
    /// requester gathers nothing). If the requester abandoned the cell, the
    /// driver owns the cleanup: spans released, the CID back in the pool.
    fn finish(self: *Table, cid: u16, v: Verdict, ledger: *span_ledger.SpanLedger) void {
        const c = &self.cells[cid];
        c.pending = false;
        self.pending_count -= 1;
        if (v != .ok) self.releaseSpans(cid, ledger);
        if (!c.done.complete(v)) {
            // The waiter left: whatever it would have gathered is dropped.
            self.releaseSpans(cid, ledger);
            self.cids.put(cid);
        }
    }

    /// A send-side failure before the wire.
    pub fn failSend(self: *Table, cid: u16, ledger: *span_ledger.SpanLedger) void {
        if (!self.cells[cid].pending) return;
        self.finish(cid, .send_failed, ledger);
    }

    /// The drain: every pending fill fails; dropping their spans is the
    /// recycle edge that refills the pool.
    pub fn failAll(self: *Table, ledger: *span_ledger.SpanLedger) void {
        for (self.cells, 0..) |*c, i| if (c.pending) self.finish(@intCast(i), .drained, ledger);
    }
};

// ---------------------------------------------------------------------------
// CID custody: the CID + the depth permit as ONE unit.
// ---------------------------------------------------------------------------

pub const CidPool = struct {
    free: mpmc.BoundedMpmc(u16),
    free_count: std.atomic.Value(u32),
    total: u32,

    pub fn init(gpa: std.mem.Allocator, depth: u32) std.mem.Allocator.Error!CidPool {
        var q = try mpmc.BoundedMpmc(u16).init(gpa, depth);
        errdefer q.deinit(gpa);
        var i: u16 = 0;
        while (i < depth) : (i += 1) std.debug.assert(q.tryPush(i));
        return .{ .free = q, .free_count = .init(depth), .total = depth };
    }

    pub fn deinit(self: *CidPool, gpa: std.mem.Allocator) void {
        self.free.deinit(gpa);
    }

    /// Null = the depth is in flight (the caller declines or parks).
    pub fn take(self: *CidPool) ?u16 {
        const cid = self.free.tryPop() orelse return null;
        _ = self.free_count.fetchSub(1, .monotonic);
        return cid;
    }

    /// The ring holds every cid: a refused push is a taker mid-pop, never a
    /// full ring — retried (decision 0003 §AI.4).
    pub fn put(self: *CidPool, cid: u16) void {
        self.free.pushSpin(cid);
        _ = self.free_count.fetchAdd(1, .monotonic);
    }

    /// `(free, total)`: `free == total` at quiescence is the no-leak law.
    pub fn slots(self: *const CidPool) struct { free: u32, total: u32 } {
        return .{ .free = self.free_count.load(.monotonic), .total = self.total };
    }
};

// ---------------------------------------------------------------------------
// Admission: a try-only atomic units word per queue.
// ---------------------------------------------------------------------------

pub const Admission = struct {
    units: std.atomic.Value(u64),
    capacity: u64,

    pub fn init(units: u64) Admission {
        return .{ .units = .init(units), .capacity = units };
    }

    /// Take `n` units or none; never parks.
    pub fn tryAcquire(self: *Admission, n: u64) bool {
        var cur = self.units.load(.monotonic);
        while (true) {
            if (cur < n) return false;
            if (self.units.cmpxchgWeak(cur, cur - n, .monotonic, .monotonic)) |actual| {
                cur = actual;
                continue;
            }
            return true;
        }
    }

    pub fn release(self: *Admission, n: u64) void {
        const prev = self.units.fetchAdd(n, .monotonic);
        std.debug.assert(prev + n <= self.capacity);
    }

    pub fn available(self: *const Admission) u64 {
        return self.units.load(.monotonic);
    }
};

pub fn unitsFor(len: u64) u64 {
    return derive.ceilDiv(len, area.ADMISSION_UNIT);
}

// ---------------------------------------------------------------------------
// The whole-read segment plan.
// ---------------------------------------------------------------------------

/// `ceil(4 MiB / 1 MiB) = 4` on the largest block; 16 bounds a wider block
/// size without a heap array — a longer read is ineligible (kernel path).
pub const MAX_SEGMENTS: usize = 16;

pub const Segment = struct { off: u64, len: u32, queue: u16 };

pub const Plan = struct {
    segs: [MAX_SEGMENTS]Segment = undefined,
    count: u8 = 0,
    /// Per-queue total units, the ONE acquire per queue.
    units: [MAX_SEGMENTS]u64 = @splat(0),

    pub fn segments(self: *const Plan) []const Segment {
        return self.segs[0..self.count];
    }
};

/// Split `[off, off + len)` into `≤ cap` pieces assigned round-robin from
/// `rotor` over `queues`; null when the read needs more than `MAX_SEGMENTS`.
pub fn plan(off: u64, len: u64, cap: u64, queues: u16, rotor: u64) ?Plan {
    std.debug.assert(cap > 0 and queues > 0 and queues <= MAX_SEGMENTS);
    var p: Plan = .{};
    var pos: u64 = 0;
    var i: u64 = 0;
    while (pos < len) : (i += 1) {
        if (p.count == MAX_SEGMENTS) return null;
        const seg_len: u32 = @intCast(@min(cap, len - pos));
        const q: u16 = @intCast((rotor + i) % queues);
        p.segs[p.count] = .{ .off = off + pos, .len = seg_len, .queue = q };
        p.units[q] += unitsFor(seg_len);
        p.count += 1;
        pos += seg_len;
    }
    return p;
}

/// `len > 0 ∧ off, len LBA-aligned`.
pub fn rangeEligible(off: u64, len: u64, lba_shift: u6) bool {
    const mask = (@as(u64, 1) << lba_shift) - 1;
    return len > 0 and off & mask == 0 and len & mask == 0;
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "exact-length law: spans accumulate by datao, SUCCESS|LAST completes exactly, a short SUCCESS fails the op, a status ≠ 0 fails the op and releases the spans; refs are taken per span and returned after the gather" {
    // The completion cell parks on the Linux futex (`sync/completion.zig`).
    if (builtin.os.tag != .linux) return testkit.skip(@src(), .no_linux, "fill_table: Linux only");
    const gpa = testing.allocator;
    var ledger = try span_ledger.SpanLedger.init(gpa, 8);
    defer ledger.deinit(gpa);
    var t = try Table.init(gpa, 4, 8);
    defer t.deinit(gpa);
    var areab: [8 * 64]u8 = undefined;
    for (&areab, 0..) |*b, i| b.* = @truncate(i);
    // CID 2: a 100 B fill delivered as 60 + 40 in chunks 0 and 1 (the driver
    // holds the chunk grants).
    const s0 = ledger.grant().?;
    const s1 = ledger.grant().?;
    t.prepare(2, 100);
    t.activate(2);
    try testing.expectEqual(@as(u32, 1), t.pendingCount());
    try t.apply(.{ .c2h_span = .{ .cid = 2, .datao = 0, .span = .{ .slot = s0, .off = 4, .len = 60 } } }, &ledger);
    try testing.expectEqual(@as(u32, 2), ledger.refCount(s0));
    try t.apply(.{ .c2h_end = .{ .cid = 2, .last = false, .success = false } }, &ledger);
    try testing.expect(t.cells[2].done.poll() == null);
    try t.apply(.{ .c2h_span = .{ .cid = 2, .datao = 60, .span = .{ .slot = s1, .off = 64, .len = 40 } } }, &ledger);
    try t.apply(.{ .c2h_end = .{ .cid = 2, .last = true, .success = true } }, &ledger);
    try testing.expectEqual(Verdict.ok, t.cells[2].done.poll().?);
    try testing.expectEqual(@as(u32, 0), t.pendingCount());
    var dest: [100]u8 = undefined;
    t.gatherInto(2, &dest, &areab);
    try testing.expectEqualSlices(u8, areab[4..64], dest[0..60]);
    try testing.expectEqualSlices(u8, areab[64..104], dest[60..100]);
    t.releaseSpans(2, &ledger);
    try testing.expectEqual(@as(u32, 1), ledger.refCount(s0));
    _ = ledger.release(s0);
    _ = ledger.release(s1);
    try testing.expectEqual(@as(u32, 8), ledger.freeCount());
    // Short + SUCCESS: the op fails, never partial data.
    const s2 = ledger.grant().?;
    t.prepare(1, 100);
    t.activate(1);
    try t.apply(.{ .c2h_span = .{ .cid = 1, .datao = 0, .span = .{ .slot = s2, .off = 0, .len = 50 } } }, &ledger);
    try t.apply(.{ .c2h_end = .{ .cid = 1, .last = true, .success = true } }, &ledger);
    try testing.expectEqual(Verdict{ .short = .{ .received = 50, .len = 100 } }, t.cells[1].done.poll().?);
    try testing.expectEqual(@as(u32, 1), ledger.refCount(s2));
    _ = ledger.release(s2);
    // A CQE with a status fails the op; the session lives (no error).
    const s3 = ledger.grant().?;
    t.prepare(3, 64);
    t.activate(3);
    try t.apply(.{ .c2h_span = .{ .cid = 3, .datao = 0, .span = .{ .slot = s3, .off = 0, .len = 64 } } }, &ledger);
    try t.apply(.{ .cqe = .{ .dw0 = 0, .dw1 = 0, .sqhd = 0, .sqid = 0, .cid = 3, .status = 0x0281 } }, &ledger);
    try testing.expectEqual(Verdict{ .status = 0x0281 }, t.cells[3].done.poll().?);
    try testing.expectEqual(@as(u32, 1), ledger.refCount(s3));
    _ = ledger.release(s3);
    // A CQE completing an exact fill (SUCCESS elided) completes it.
    const s4 = ledger.grant().?;
    t.prepare(0, 64);
    t.activate(0);
    try t.apply(.{ .c2h_span = .{ .cid = 0, .datao = 0, .span = .{ .slot = s4, .off = 0, .len = 64 } } }, &ledger);
    try t.apply(.{ .c2h_end = .{ .cid = 0, .last = true, .success = false } }, &ledger);
    try t.apply(.{ .cqe = .{ .dw0 = 0, .dw1 = 0, .sqhd = 0, .sqid = 0, .cid = 0, .status = 0 } }, &ledger);
    try testing.expectEqual(Verdict.ok, t.cells[0].done.poll().?);
    t.releaseSpans(0, &ledger);
    _ = ledger.release(s4);
    try testing.expectEqual(@as(u32, 8), ledger.freeCount());
}

test "violations: an unknown CID, a span past the length, SUCCESS on a non-LAST PDU, a span for a CID with no pending fill" {
    // The completion cell parks on the Linux futex (`sync/completion.zig`).
    if (builtin.os.tag != .linux) return testkit.skip(@src(), .no_linux, "fill_table: Linux only");
    const gpa = testing.allocator;
    var ledger = try span_ledger.SpanLedger.init(gpa, 2);
    defer ledger.deinit(gpa);
    var t = try Table.init(gpa, 2, 4);
    defer t.deinit(gpa);
    const s = ledger.grant().?;
    try testing.expectError(error.Violation, t.apply(.{ .c2h_span = .{ .cid = 9, .datao = 0, .span = .{ .slot = s, .off = 0, .len = 1 } } }, &ledger));
    try testing.expectError(error.Violation, t.apply(.{ .c2h_span = .{ .cid = 1, .datao = 0, .span = .{ .slot = s, .off = 0, .len = 1 } } }, &ledger));
    t.prepare(1, 10);
    t.activate(1);
    try testing.expectError(error.Violation, t.apply(.{ .c2h_span = .{ .cid = 1, .datao = 8, .span = .{ .slot = s, .off = 0, .len = 3 } } }, &ledger));
    try testing.expectError(error.Violation, t.apply(.{ .c2h_end = .{ .cid = 1, .last = false, .success = true } }, &ledger));
    try testing.expectError(error.Violation, t.apply(.{ .cqe = .{ .dw0 = 0, .dw1 = 0, .sqhd = 0, .sqid = 0, .cid = 0, .status = 0 } }, &ledger));
    // Nothing leaked a ref.
    try testing.expectEqual(@as(u32, 1), ledger.refCount(s));
    // The drain fails the pending fill and recycles.
    t.failAll(&ledger);
    try testing.expectEqual(Verdict.drained, t.cells[1].done.poll().?);
    try testing.expectEqual(@as(u32, 0), t.pendingCount());
}

test "MEM-3: a requester that abandoned its cell leaves the driver owning the cleanup — spans released, the CID back in the pool (free == total)" {
    // The completion cell parks on the Linux futex (`sync/completion.zig`).
    if (builtin.os.tag != .linux) return testkit.skip(@src(), .no_linux, "fill_table: Linux only");
    const gpa = testing.allocator;
    var ledger = try span_ledger.SpanLedger.init(gpa, 2);
    defer ledger.deinit(gpa);
    var t = try Table.init(gpa, 2, 4);
    defer t.deinit(gpa);
    const s = ledger.grant().?;
    const cid = t.cids.take().?;
    try testing.expectEqual(@as(u32, 1), t.cids.slots().free);
    t.prepare(cid, 8);
    t.activate(cid);
    try t.apply(.{ .c2h_span = .{ .cid = cid, .datao = 0, .span = .{ .slot = s, .off = 0, .len = 8 } } }, &ledger);
    try testing.expectEqual(@as(u32, 2), ledger.refCount(s));
    try testing.expect(t.cells[cid].done.abandon());
    try t.apply(.{ .c2h_end = .{ .cid = cid, .last = true, .success = true } }, &ledger);
    try testing.expectEqual(@as(u32, 1), ledger.refCount(s));
    try testing.expectEqual(@as(u32, 2), t.cids.slots().free);
    _ = ledger.release(s);
}

test "CID custody: take/put round-trip, exhaustion is null, free == total at quiescence" {
    var pool = try CidPool.init(testing.allocator, 3);
    defer pool.deinit(testing.allocator);
    try testing.expectEqual(@as(u32, 3), pool.slots().free);
    const a = pool.take().?;
    const b = pool.take().?;
    const c = pool.take().?;
    try testing.expect(a != b and b != c and a != c);
    try testing.expect(pool.take() == null);
    try testing.expectEqual(@as(u32, 0), pool.slots().free);
    pool.put(b);
    pool.put(a);
    pool.put(c);
    try testing.expectEqual(@as(u32, 3), pool.slots().free);
    try testing.expectEqual(@as(u32, 3), pool.slots().total);
}

test "admission is try-only: a decline leaves the word untouched; releases restore exactly" {
    var adm = Admission.init(10);
    try testing.expect(adm.tryAcquire(6));
    try testing.expect(!adm.tryAcquire(5));
    try testing.expectEqual(@as(u64, 4), adm.available());
    try testing.expect(adm.tryAcquire(4));
    try testing.expect(!adm.tryAcquire(1));
    adm.release(6);
    adm.release(4);
    try testing.expectEqual(@as(u64, 10), adm.available());
    try testing.expectEqual(@as(u64, 1), unitsFor(1));
    try testing.expectEqual(@as(u64, 256), unitsFor(1 << 20));
    try testing.expectEqual(@as(u64, 2), unitsFor(4097));
}

test "the segment plan: cap-sized pieces round-robin from the rotor; per-queue units summed ONCE; a read past 16 segments is null; range eligibility by LBA" {
    const p = plan(1 << 20, 3 << 20, 1 << 20, 2, 1).?;
    try testing.expectEqual(@as(u8, 3), p.count);
    try testing.expectEqual(Segment{ .off = 1 << 20, .len = 1 << 20, .queue = 1 }, p.segs[0]);
    try testing.expectEqual(Segment{ .off = 2 << 20, .len = 1 << 20, .queue = 0 }, p.segs[1]);
    try testing.expectEqual(Segment{ .off = 3 << 20, .len = 1 << 20, .queue = 1 }, p.segs[2]);
    try testing.expectEqual(@as(u64, 256), p.units[0]);
    try testing.expectEqual(@as(u64, 512), p.units[1]);
    const tail = plan(0, (1 << 20) + 4096, 1 << 20, 1, 0).?;
    try testing.expectEqual(@as(u8, 2), tail.count);
    try testing.expectEqual(@as(u32, 4096), tail.segs[1].len);
    try testing.expect(plan(0, 17 << 20, 1 << 20, 4, 0) == null);
    try testing.expect(rangeEligible(0, 4096, 9));
    try testing.expect(!rangeEligible(0, 0, 9));
    try testing.expect(!rangeEligible(256, 512, 9));
    try testing.expect(!rangeEligible(512, 100, 9));
    try testing.expect(rangeEligible(4096, 4096, 12));
}
