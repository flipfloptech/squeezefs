//! THE BUFFER-LEASE LAW (charter §7 "arena-backed 4 KiB-aligned buffers
//! under a buffer-lease law (`free` while SUBMITTED = EBUSY, never deferred;
//! poison ⇒ never recycled)"; spec `docs/porting/il_sdk.md` §2) and the
//! buddy allocator over one session arena. Portable: no syscalls, no `Io`;
//! `buffer_lease_model.zig` restates the lease word for the explorer.
//!
//! The app leases its own arena buffer to the daemon for the duration of an
//! op — the transport payload lease with the roles mirrored. States:
//!
//!     APP_OWNED ──submit (inflight += 1)──▶ SUBMITTED ──every CQE reaped──▶ APP_OWNED
//!         │ free ⇒ FREED, the range back to the allocator        │ free ⇒ -EBUSY, NEVER deferred
//!         └──────────────── session poison while SUBMITTED ───────┴──▶ POISONED (never recycled)
//!
//! SUBMITTED ≡ `inflight > 0` on ONE 32-bit word `{state u8 | inflight
//! u24}`, so "free iff APP_OWNED and inflight == 0" is a single CAS — a
//! load-then-store pair is the WEAKENING the model proves fatal (a submit
//! between the two frees a leased range the daemon may still be serving:
//! the shim's timed-out-slot never-reuse law, verbatim). A hidden deferred
//! free would recycle custody the same way, so the violation that could
//! hurt SOMEONE ELSE's op is the one made unrepresentable.
//!
//! Every record lives OUTSIDE the arena: the arena is daemon-writable for
//! the session's life (a late completion into a POISONED range, a daemon
//! bug), so no arena write can corrupt the lease bookkeeping.

const std = @import("std");
const Atomic = std.atomic.Value;
const assert = std.debug.assert;

/// The granule and alignment of every buffer: the O_DIRECT LBA / the
/// direct-drive DMA dest gate — a 4 KiB-aligned window keeps cold reads on
/// the zero-daemon-copy arm. Natural power-of-two placement makes every
/// buffer `GRANULE`-aligned regardless of its size.
pub const GRANULE: u64 = 4096;
/// Arena ≤ 2^40 (`layout.MAX_ARENA_BYTES`) ⇒ ≤ 2^28 granules ⇒ orders
/// 0..28; 32 lists is the next round number.
pub const MAX_ORDERS: usize = 32;

// ---------------------------------------------------------------------------
// The lease word.
// ---------------------------------------------------------------------------

pub const State = enum(u8) { app_owned = 0, freed = 1, poisoned = 2 };

/// ONE word: state in the top byte, the inflight count below.
pub const Word = packed struct(u32) {
    inflight: u24,
    state: State,

    pub fn pack(w: Word) u32 {
        return @bitCast(w);
    }
    pub fn unpack(v: u32) Word {
        return @bitCast(v);
    }
};

pub const FreeOutcome = enum {
    /// The range returns to the allocator.
    recycle,
    /// POISONED: the record is done, the range is quarantined forever.
    quarantine,
};

pub const Lease = struct {
    word: Atomic(u32) = .init(Word.pack(.{ .inflight = 0, .state = .app_owned })),

    pub fn load(l: *const Lease) Word {
        return Word.unpack(l.word.load(.acquire));
    }

    /// `sqz_submit` referencing the buffer: APP_OWNED or SUBMITTED ⇒
    /// `inflight += 1` (several SQEs may reference one buffer); FREED and
    /// POISONED never take a new op (`-EINVAL` for that SQE). `.acq_rel`
    /// pairs with `complete`'s decrement and `free`'s CAS.
    pub fn submit(l: *Lease) error{InvalidArgument}!void {
        var cur = l.load();
        while (true) {
            if (cur.state != .app_owned) return error.InvalidArgument;
            const next: Word = .{ .inflight = cur.inflight + 1, .state = .app_owned };
            cur = Word.unpack(l.word.cmpxchgWeak(cur.pack(), next.pack(), .acq_rel, .acquire) orelse return);
        }
    }

    /// The CQE of one referencing op reaped (after the DONE Acquire that
    /// orders the daemon's arena write before this): `inflight -= 1`; 0 ⇒
    /// APP_OWNED again. A POISONED record keeps its count (the late CQE of
    /// a dead session is never delivered — the record stays POISONED).
    pub fn complete(l: *Lease) void {
        var cur = l.load();
        while (cur.state == .app_owned) {
            assert(cur.inflight > 0);
            const next: Word = .{ .inflight = cur.inflight - 1, .state = .app_owned };
            cur = Word.unpack(l.word.cmpxchgWeak(cur.pack(), next.pack(), .acq_rel, .acquire) orelse return);
        }
    }

    /// `sqz_buf_free`: FREED ⇒ `-EINVAL` (idempotent-refused); POISONED ⇒
    /// the record flips to FREED and the range is QUARANTINED; APP_OWNED ⇒
    /// ONE CAS `{app_owned, 0} → {freed, 0}` — any inflight makes it fail
    /// `-EBUSY` with the state unchanged, never a deferral.
    pub fn free(l: *Lease) error{ Busy, InvalidArgument }!FreeOutcome {
        var cur = l.load();
        while (true) {
            switch (cur.state) {
                .freed => return error.InvalidArgument,
                .poisoned => {
                    const next: Word = .{ .inflight = 0, .state = .freed };
                    cur = Word.unpack(l.word.cmpxchgWeak(cur.pack(), next.pack(), .acq_rel, .acquire) orelse return .quarantine);
                },
                .app_owned => {
                    if (cur.inflight != 0) return error.Busy;
                    const want: Word = .{ .inflight = 0, .state = .app_owned };
                    const next: Word = .{ .inflight = 0, .state = .freed };
                    // Strong: a spurious failure here must not read as Busy.
                    cur = Word.unpack(l.word.cmpxchgStrong(want.pack(), next.pack(), .acq_rel, .acquire) orelse return .recycle);
                },
            }
        }
    }

    /// Session poison: SUBMITTED ⇒ POISONED (the daemon may complete into
    /// the range arbitrarily late); APP_OWNED with nothing in flight stays
    /// valid memory. Returns whether this record flipped.
    pub fn poison(l: *Lease) bool {
        var cur = l.load();
        while (cur.state == .app_owned and cur.inflight != 0) {
            const next: Word = .{ .inflight = cur.inflight, .state = .poisoned };
            cur = Word.unpack(l.word.cmpxchgWeak(cur.pack(), next.pack(), .acq_rel, .acquire) orelse return true);
        }
        return false;
    }

    pub fn submitted(l: *const Lease) bool {
        const w = l.load();
        return w.state == .app_owned and w.inflight != 0;
    }
};

// ---------------------------------------------------------------------------
// The buddy allocator over the arena.
// ---------------------------------------------------------------------------

pub const Range = struct {
    off: u64,
    order: u8,

    pub fn bytes(r: Range) u64 {
        return GRANULE << @intCast(r.order);
    }
};

const NONE: u32 = std.math.maxInt(u32);

/// Granule-indexed bookkeeping (never in the arena): per granule the order
/// of the block starting there and whether it is free; per order a doubly
/// linked free list through granule indices. A non-power-of-two arena is
/// seeded as its binary decomposition (96 MiB = 64 + 32), and a merge is
/// admitted only when the buddy block lies inside the arena — the one rule
/// that keeps every block naturally aligned and inside its seed.
pub const Buddy = struct {
    gpa: std.mem.Allocator,
    granules: u32,
    order: []u8,
    is_free: []bool,
    next: []u32,
    prev: []u32,
    heads: [MAX_ORDERS]u32,
    /// Granules currently free (the census).
    free_granules: u64,

    pub const InitError = error{ OutOfMemory, ArenaNotGranuleMultiple, ArenaEmpty };

    pub fn init(gpa: std.mem.Allocator, arena_bytes: u64) InitError!Buddy {
        if (arena_bytes == 0) return error.ArenaEmpty;
        if (arena_bytes % GRANULE != 0) return error.ArenaNotGranuleMultiple;
        const n: u32 = @intCast(arena_bytes / GRANULE);
        var self: Buddy = .{
            .gpa = gpa,
            .granules = n,
            .order = try gpa.alloc(u8, n),
            .is_free = undefined,
            .next = undefined,
            .prev = undefined,
            .heads = @splat(NONE),
            .free_granules = 0,
        };
        errdefer gpa.free(self.order);
        self.is_free = try gpa.alloc(bool, n);
        errdefer gpa.free(self.is_free);
        self.next = try gpa.alloc(u32, n);
        errdefer gpa.free(self.next);
        self.prev = try gpa.alloc(u32, n);
        @memset(self.order, 0);
        @memset(self.is_free, false);
        // The binary decomposition, largest block first at offset 0.
        var idx: u32 = 0;
        var remaining = n;
        while (remaining > 0) {
            const ord: u8 = @intCast(std.math.log2_int(u32, remaining));
            self.pushFree(idx, ord);
            idx += @as(u32, 1) << @intCast(ord);
            remaining -= @as(u32, 1) << @intCast(ord);
        }
        return self;
    }

    pub fn deinit(self: *Buddy) void {
        self.gpa.free(self.order);
        self.gpa.free(self.is_free);
        self.gpa.free(self.next);
        self.gpa.free(self.prev);
        self.* = undefined;
    }

    fn pushFree(self: *Buddy, idx: u32, ord: u8) void {
        self.is_free[idx] = true;
        self.order[idx] = ord;
        self.prev[idx] = NONE;
        self.next[idx] = self.heads[ord];
        if (self.heads[ord] != NONE) self.prev[self.heads[ord]] = idx;
        self.heads[ord] = idx;
        self.free_granules += @as(u64, 1) << @intCast(ord);
    }

    fn removeFree(self: *Buddy, idx: u32) void {
        const ord = self.order[idx];
        if (self.prev[idx] != NONE) self.next[self.prev[idx]] = self.next[idx] else self.heads[ord] = self.next[idx];
        if (self.next[idx] != NONE) self.prev[self.next[idx]] = self.prev[idx];
        self.is_free[idx] = false;
        self.free_granules -= @as(u64, 1) << @intCast(ord);
    }

    /// The order a byte length needs: `ceil(log2(ceil(len / GRANULE)))`.
    pub fn orderFor(len: u64) ?u8 {
        if (len == 0) return null;
        const g = std.math.divCeil(u64, len, GRANULE) catch unreachable;
        const ord = std.math.log2_int_ceil(u64, g);
        if (ord >= MAX_ORDERS) return null;
        return @intCast(ord);
    }

    /// Null = no free block of a sufficient order (fragmentation or
    /// exhaustion — the caller answers `-ENOMEM`, explicit, never a hidden
    /// bounce buffer).
    pub fn alloc(self: *Buddy, len: u64) ?Range {
        const want = orderFor(len) orelse return null;
        var o: usize = want;
        while (o < MAX_ORDERS and self.heads[o] == NONE) o += 1;
        if (o >= MAX_ORDERS) return null;
        const idx = self.heads[o];
        self.removeFree(idx);
        // Split down to the wanted order, the upper half of each split freed.
        while (o > want) {
            o -= 1;
            self.pushFree(idx + (@as(u32, 1) << @intCast(o)), @intCast(o));
        }
        self.order[idx] = want;
        return .{ .off = @as(u64, idx) * GRANULE, .order = want };
    }

    /// Return a range; coalesce upward while the buddy is free, of the same
    /// order and inside the arena.
    pub fn free(self: *Buddy, r: Range) void {
        var idx: u32 = @intCast(r.off / GRANULE);
        var ord = r.order;
        assert(!self.is_free[idx]);
        while (ord + 1 < MAX_ORDERS) {
            const size = @as(u32, 1) << @intCast(ord);
            const buddy = idx ^ size;
            if (buddy >= self.granules or @max(idx, buddy) + size > self.granules) break;
            if (!self.is_free[buddy] or self.order[buddy] != ord) break;
            self.removeFree(buddy);
            idx = @min(idx, buddy);
            ord += 1;
        }
        self.pushFree(idx, ord);
    }

    pub fn freeBytes(self: *const Buddy) u64 {
        return self.free_granules * GRANULE;
    }

    pub fn totalBytes(self: *const Buddy) u64 {
        return @as(u64, self.granules) * GRANULE;
    }
};

// ---------------------------------------------------------------------------
// The buffer records and the arena they lease from.
// ---------------------------------------------------------------------------

/// `sqz_buf_t`: one arena range + its lease. The record outlives its range
/// in the POISONED case (quarantined) and is recycled only once FREED.
pub const Buf = struct {
    lease: Lease = .{},
    range: Range = .{ .off = 0, .order = 0 },
    /// The requested length (`sqz_buf_len`), ≤ `range.bytes()`.
    len: u64 = 0,
    /// The arena the record belongs to (set at alloc, when the arena is at
    /// its final address); records never leave their arena.
    arena: ?*Arena = null,
    /// Pool link while the record is free.
    next_free: u32 = NONE,
    live: bool = false,
};

pub const Census = struct {
    allocs: u64,
    frees: u64,
    busy_refusals: u64,
    /// Records flipped SUBMITTED → POISONED.
    poisoned: u64,
    /// Granules quarantined forever (never back to the allocator).
    quarantined_granules: u64,
    /// Live APP_OWNED/SUBMITTED records.
    live: u64,
    /// POISONED records the app has not freed yet.
    poisoned_live: u64,
    free_bytes: u64,

    /// `allocs ≡ frees + live + poisoned_live` — the lease identity.
    pub fn holds(c: Census) bool {
        return c.allocs == c.frees + c.live + c.poisoned_live;
    }
};

/// The allocator's lock: a test-and-test-and-set spinlock, portable (no
/// `Io`, no libc). `sqz_buf_alloc`/`sqz_buf_free` are not per-op paths —
/// a buffer is leased across many ops — and the lease CAS precedes the
/// lock, so a `-EBUSY` refusal never waits here.
pub const SpinLock = struct {
    locked: Atomic(bool) = .init(false),

    pub fn lock(self: *SpinLock) void {
        while (true) {
            // Acquire pairs with `unlock`'s Release.
            if (!self.locked.swap(true, .acquire)) return;
            while (self.locked.load(.monotonic)) std.atomic.spinLoopHint();
        }
    }

    pub fn unlock(self: *SpinLock) void {
        self.locked.store(false, .release);
    }
};

/// One session's arena: the buddy allocator + a record pool sized one
/// record per granule (the worst case; 64 MiB ⇒ 16 384 records). The lease
/// CAS is lock-free and precedes the lock (a refusal is never queued behind
/// an allocation); the allocator and the pool mutate under `lock`.
/// `poisonAll` takes no lock (the atfork child calls it).
pub const Arena = struct {
    gpa: std.mem.Allocator,
    buddy: Buddy,
    recs: []Buf,
    free_head: u32,
    lock: SpinLock = .{},
    // ---- the `sdk_buf_*` census (display-only, plain atomics) ----
    allocs: Atomic(u64) = .init(0),
    frees: Atomic(u64) = .init(0),
    busy_refusals: Atomic(u64) = .init(0),
    poisoned: Atomic(u64) = .init(0),
    quarantined_granules: Atomic(u64) = .init(0),

    pub fn init(gpa: std.mem.Allocator, arena_bytes: u64) Buddy.InitError!Arena {
        var buddy = try Buddy.init(gpa, arena_bytes);
        errdefer buddy.deinit();
        const recs = try gpa.alloc(Buf, buddy.granules);
        for (recs, 0..) |*r, i| r.* = .{ .next_free = if (i + 1 < recs.len) @intCast(i + 1) else NONE };
        return .{ .gpa = gpa, .buddy = buddy, .recs = recs, .free_head = 0 };
    }

    pub fn deinit(self: *Arena) void {
        self.gpa.free(self.recs);
        self.buddy.deinit();
        self.* = undefined;
    }

    pub fn arenaBytes(self: *const Arena) u64 {
        return self.buddy.totalBytes();
    }

    /// `len == 0` ⇒ `InvalidArgument`; no range ⇒ `OutOfMemory`.
    pub fn alloc(self: *Arena, len: u64) error{ InvalidArgument, OutOfMemory }!*Buf {
        if (len == 0) return error.InvalidArgument;
        self.lock.lock();
        defer self.lock.unlock();
        if (self.free_head == NONE) return error.OutOfMemory;
        const range = self.buddy.alloc(len) orelse return error.OutOfMemory;
        const idx = self.free_head;
        const rec = &self.recs[idx];
        self.free_head = rec.next_free;
        rec.* = .{ .range = range, .len = len, .arena = self, .live = true };
        _ = self.allocs.fetchAdd(1, .monotonic);
        return rec;
    }

    /// The matrix's `sqz_buf_free` column: `-EBUSY` while SUBMITTED (the
    /// CAS, before any lock); `-EINVAL` on a FREED record; POISONED ⇒ the
    /// record returns to the pool, the RANGE never does.
    pub fn free(self: *Arena, b: *Buf) error{ Busy, InvalidArgument }!void {
        const outcome = b.lease.free() catch |e| {
            if (e == error.Busy) _ = self.busy_refusals.fetchAdd(1, .monotonic);
            return e;
        };
        self.lock.lock();
        defer self.lock.unlock();
        switch (outcome) {
            .recycle => self.buddy.free(b.range),
            .quarantine => _ = self.quarantined_granules.fetchAdd(@as(u64, 1) << @intCast(b.range.order), .monotonic),
        }
        b.live = false;
        b.next_free = self.free_head;
        self.free_head = @intCast((@intFromPtr(b) - @intFromPtr(self.recs.ptr)) / @sizeOf(Buf));
        _ = self.frees.fetchAdd(1, .monotonic);
    }

    /// Session poison: every SUBMITTED record ⇒ POISONED. Lock-free (the
    /// atfork child runs it); returns how many flipped.
    pub fn poisonAll(self: *Arena) u64 {
        var n: u64 = 0;
        for (self.recs) |*r| {
            if (r.lease.poison()) n += 1;
        }
        _ = self.poisoned.fetchAdd(n, .monotonic);
        return n;
    }

    /// Any record SUBMITTED right now (`sqz_session_close` refuses `-EBUSY`).
    pub fn anySubmitted(self: *const Arena) bool {
        for (self.recs) |*r| if (r.lease.submitted()) return true;
        return false;
    }

    /// Is `b` a record of this arena (the handle screen)?
    pub fn owns(self: *const Arena, b: *const Buf) bool {
        const base = @intFromPtr(self.recs.ptr);
        const p = @intFromPtr(b);
        if (p < base or p >= base + self.recs.len * @sizeOf(Buf)) return false;
        return (p - base) % @sizeOf(Buf) == 0;
    }

    pub fn census(self: *Arena) Census {
        var live: u64 = 0;
        var poisoned_live: u64 = 0;
        for (self.recs) |*r| {
            if (!r.live) continue;
            switch (r.lease.load().state) {
                .app_owned => live += 1,
                .poisoned => poisoned_live += 1,
                .freed => {},
            }
        }
        self.lock.lock();
        defer self.lock.unlock();
        return .{
            .allocs = self.allocs.load(.monotonic),
            .frees = self.frees.load(.monotonic),
            .busy_refusals = self.busy_refusals.load(.monotonic),
            .poisoned = self.poisoned.load(.monotonic),
            .quarantined_granules = self.quarantined_granules.load(.monotonic),
            .live = live,
            .poisoned_live = poisoned_live,
            .free_bytes = self.buddy.freeBytes(),
        };
    }
};

// ---------------------------------------------------------------------------
// Tests (spec §8: the matrix every cell, the allocator property test, the
// quarantine, the census identity).
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the lease word is 32 bits: state in the top byte, inflight below; the initial word is APP_OWNED with nothing in flight" {
    try testing.expectEqual(@as(usize, 4), @sizeOf(Word));
    const w: Word = .{ .inflight = 5, .state = .poisoned };
    try testing.expectEqual(@as(u32, (2 << 24) | 5), w.pack());
    try testing.expectEqual(w, Word.unpack(w.pack()));
    var l: Lease = .{};
    try testing.expectEqual(State.app_owned, l.load().state);
    try testing.expectEqual(@as(u24, 0), l.load().inflight);
    try testing.expect(!l.submitted());
}

test "the (state, call) matrix, every cell: submit / complete / free / poison on APP_OWNED, SUBMITTED, POISONED and FREED" {
    // APP_OWNED row.
    var l: Lease = .{};
    try l.submit(); // → SUBMITTED
    try testing.expect(l.submitted());
    try testing.expectEqual(@as(u24, 1), l.load().inflight);
    // SUBMITTED row: a second SQE references it; free is -EBUSY and the
    // state is unchanged (never deferred); the two CQEs bring it back.
    try l.submit();
    try testing.expectEqual(@as(u24, 2), l.load().inflight);
    try testing.expectError(error.Busy, l.free());
    try testing.expect(l.submitted());
    try testing.expectEqual(@as(u24, 2), l.load().inflight);
    l.complete();
    try testing.expectError(error.Busy, l.free());
    l.complete();
    try testing.expect(!l.submitted());
    try testing.expectEqual(State.app_owned, l.load().state);
    // APP_OWNED: poison leaves it alone (valid memory); free recycles.
    try testing.expect(!l.poison());
    try testing.expectEqual(State.app_owned, l.load().state);
    try testing.expectEqual(FreeOutcome.recycle, try l.free());
    // FREED row: everything refuses -EINVAL / is a no-op.
    try testing.expectEqual(State.freed, l.load().state);
    try testing.expectError(error.InvalidArgument, l.submit());
    try testing.expectError(error.InvalidArgument, l.free());
    try testing.expect(!l.poison());
    l.complete();
    try testing.expectEqual(State.freed, l.load().state);
    // POISONED row: SUBMITTED → POISONED keeps the count; a late CQE
    // changes nothing; a new SQE is -EINVAL; free is 0 with quarantine.
    var p: Lease = .{};
    try p.submit();
    try p.submit();
    try testing.expect(p.poison());
    try testing.expectEqual(State.poisoned, p.load().state);
    try testing.expectEqual(@as(u24, 2), p.load().inflight);
    try testing.expect(!p.poison());
    p.complete();
    try testing.expectEqual(@as(u24, 2), p.load().inflight);
    try testing.expectError(error.InvalidArgument, p.submit());
    try testing.expect(!p.submitted());
    try testing.expectEqual(FreeOutcome.quarantine, try p.free());
    try testing.expectEqual(State.freed, p.load().state);
    try testing.expectError(error.InvalidArgument, p.free());
}

test "buddy: every alloc GRANULE-aligned and inside the arena, no two live ranges overlap, free coalesces back to the seed — a property test over random alloc/free sequences on pow2 and non-pow2 arenas" {
    var prng = std.Random.DefaultPrng.init(0xb0dd);
    const r = prng.random();
    for ([_]u64{ 64 * GRANULE, 96 * GRANULE, 3 * GRANULE, 1000 * GRANULE }) |arena_bytes| {
        var b = try Buddy.init(testing.allocator, arena_bytes);
        defer b.deinit();
        try testing.expectEqual(arena_bytes, b.freeBytes());
        var live: std.ArrayList(Range) = .empty;
        defer live.deinit(testing.allocator);
        var round: usize = 0;
        while (round < 3000) : (round += 1) {
            if (live.items.len == 0 or r.boolean()) {
                const len = r.uintLessThan(u64, arena_bytes / 4 + 1) + 1;
                if (b.alloc(len)) |rg| {
                    try testing.expectEqual(@as(u64, 0), rg.off % GRANULE);
                    try testing.expectEqual(@as(u64, 0), rg.off % rg.bytes());
                    try testing.expect(rg.bytes() >= len);
                    try testing.expect(rg.off + rg.bytes() <= arena_bytes);
                    for (live.items) |o| try testing.expect(rg.off + rg.bytes() <= o.off or o.off + o.bytes() <= rg.off);
                    try live.append(testing.allocator, rg);
                }
            } else {
                const i = r.uintLessThan(usize, live.items.len);
                b.free(live.swapRemove(i));
            }
            var used: u64 = 0;
            for (live.items) |o| used += o.bytes();
            try testing.expectEqual(arena_bytes - used, b.freeBytes());
        }
        for (live.items) |o| b.free(o);
        try testing.expectEqual(arena_bytes, b.freeBytes());
        // Back to the seed: the largest block of the decomposition is free again.
        const top_ord: u8 = @intCast(std.math.log2_int(u64, arena_bytes / GRANULE));
        try testing.expect(b.heads[top_ord] != NONE);
        try testing.expectEqual(@as(u32, 0), b.heads[top_ord]);
    }
}

test "buddy: the whole arena is one allocation; a non-pow2 arena never merges across its decomposition; orderFor's ties; exhaustion is null, never a panic" {
    var b = try Buddy.init(testing.allocator, 96 * GRANULE);
    defer b.deinit();
    // 96 = 64 + 32: the largest single buffer is 64 granules.
    try testing.expectEqual(@as(?Range, null), b.alloc(65 * GRANULE));
    const big = b.alloc(64 * GRANULE).?;
    try testing.expectEqual(@as(u64, 0), big.off);
    const rest = b.alloc(32 * GRANULE).?;
    try testing.expectEqual(64 * GRANULE, rest.off);
    try testing.expectEqual(@as(u64, 0), b.freeBytes());
    try testing.expectEqual(@as(?Range, null), b.alloc(1));
    b.free(rest);
    b.free(big);
    try testing.expectEqual(96 * GRANULE, b.freeBytes());
    // Never a 128-granule block.
    try testing.expectEqual(NONE, b.heads[7]);
    try testing.expectEqual(@as(?u8, 0), Buddy.orderFor(1));
    try testing.expectEqual(@as(?u8, 0), Buddy.orderFor(GRANULE));
    try testing.expectEqual(@as(?u8, 1), Buddy.orderFor(GRANULE + 1));
    try testing.expectEqual(@as(?u8, 4), Buddy.orderFor(16 * GRANULE));
    try testing.expectEqual(@as(?u8, 5), Buddy.orderFor(16 * GRANULE + 1));
    try testing.expectEqual(@as(?u8, null), Buddy.orderFor(0));
    // A whole default arena as ONE buffer (the 64 MiB round-trip's shape).
    var whole = try Buddy.init(testing.allocator, 64 << 20);
    defer whole.deinit();
    try testing.expectEqual(@as(u64, 64 << 20), whole.alloc(64 << 20).?.bytes());
    try testing.expectError(error.ArenaNotGranuleMultiple, Buddy.init(testing.allocator, GRANULE + 1));
    try testing.expectError(error.ArenaEmpty, Buddy.init(testing.allocator, 0));
}

test "arena: free while SUBMITTED is -EBUSY and the range stays allocated; the census identity holds at every step; a freed record is refused -EINVAL" {
    var a = try Arena.init(testing.allocator, 16 * GRANULE);
    defer a.deinit();
    try testing.expectError(error.InvalidArgument, a.alloc(0));
    const b1 = try a.alloc(3 * GRANULE);
    try testing.expectEqual(@as(u64, 3 * GRANULE), b1.len);
    try testing.expectEqual(@as(u64, 4 * GRANULE), b1.range.bytes());
    try testing.expect(a.owns(b1));
    try testing.expectEqual(&a, b1.arena.?);
    try b1.lease.submit();
    const before = a.census();
    try testing.expectError(error.Busy, a.free(b1));
    const after = a.census();
    try testing.expectEqual(before.free_bytes, after.free_bytes);
    try testing.expectEqual(@as(u64, 1), after.busy_refusals);
    try testing.expect(after.holds());
    try testing.expect(a.anySubmitted());
    b1.lease.complete();
    try testing.expect(!a.anySubmitted());
    try a.free(b1);
    try testing.expectError(error.InvalidArgument, a.free(b1));
    const c = a.census();
    try testing.expectEqual(@as(u64, 1), c.allocs);
    try testing.expectEqual(@as(u64, 1), c.frees);
    try testing.expectEqual(@as(u64, 0), c.live);
    try testing.expectEqual(16 * GRANULE, c.free_bytes);
    try testing.expect(c.holds());
    // Exhaustion is explicit.
    const whole = try a.alloc(16 * GRANULE);
    try testing.expectError(error.OutOfMemory, a.alloc(1));
    try a.free(whole);
    var dummy: Buf = .{};
    try testing.expect(!a.owns(&dummy));
}

test "arena: poisoned ranges never return to the allocator — the census after a poison equals before minus the quarantined set, for the session's life" {
    var a = try Arena.init(testing.allocator, 32 * GRANULE);
    defer a.deinit();
    const idle = try a.alloc(GRANULE);
    const leased = try a.alloc(8 * GRANULE);
    const leased2 = try a.alloc(2 * GRANULE);
    try leased.lease.submit();
    try leased2.lease.submit();
    try leased2.lease.submit();
    const free_before = a.census().free_bytes;
    try testing.expectEqual(@as(u64, 2), a.poisonAll());
    try testing.expectEqual(@as(u64, 0), a.poisonAll());
    try testing.expectEqual(State.poisoned, leased.lease.load().state);
    try testing.expectEqual(State.app_owned, idle.lease.load().state);
    var c = a.census();
    try testing.expectEqual(@as(u64, 2), c.poisoned);
    try testing.expectEqual(@as(u64, 2), c.poisoned_live);
    try testing.expectEqual(@as(u64, 1), c.live);
    try testing.expect(c.holds());
    // Freeing a poisoned buffer is 0 for the app and quarantines its range.
    try a.free(leased);
    try a.free(leased2);
    try a.free(idle);
    c = a.census();
    try testing.expectEqual(@as(u64, 10), c.quarantined_granules);
    try testing.expectEqual(free_before + GRANULE, c.free_bytes);
    try testing.expectEqual(32 * GRANULE - 10 * GRANULE, c.free_bytes);
    try testing.expectEqual(@as(u64, 3), c.frees);
    try testing.expectEqual(@as(u64, 0), c.live + c.poisoned_live);
    try testing.expect(c.holds());
    // The quarantine only grows: the lower half holds the quarantined 8 and
    // 2 granule blocks, so the whole arena never re-forms and only the
    // upper 16-granule half is a 16-granule buffer.
    try testing.expectError(error.OutOfMemory, a.alloc(32 * GRANULE));
    const upper = try a.alloc(16 * GRANULE);
    try testing.expectEqual(16 * GRANULE, upper.range.off);
    try testing.expectError(error.OutOfMemory, a.alloc(16 * GRANULE));
    try testing.expectError(error.OutOfMemory, a.alloc(8 * GRANULE));
    try a.free(upper);
}

test "arena: the record pool is one record per granule and recycles FREED records; a stale record is not owned by another arena" {
    var a = try Arena.init(testing.allocator, 4 * GRANULE);
    defer a.deinit();
    try testing.expectEqual(@as(usize, 4), a.recs.len);
    var recs: [4]*Buf = undefined;
    for (&recs) |*r| r.* = try a.alloc(GRANULE);
    try testing.expectError(error.OutOfMemory, a.alloc(GRANULE));
    for (recs) |r| try a.free(r);
    const again = try a.alloc(4 * GRANULE);
    try testing.expect(a.owns(again));
    var other = try Arena.init(testing.allocator, 4 * GRANULE);
    defer other.deinit();
    try testing.expect(!other.owns(again));
    try a.free(again);
}
