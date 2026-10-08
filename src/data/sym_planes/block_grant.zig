//! Ranged block grants (charter §5.5 "`G = clamp(2 × ewma_blocks/s ×
//! T_renewal, 64, cap/(2 × writers))` (floor 64 = one write-pipeline BDP
//! window; carve = lowest clear run at/above the frontier, bits SET at the
//! carve; writer tops up at 50 % as a PULL; liveness = the death ledger,
//! never a TTL; a dead writer's grants revoked + quarantined under a dead
//! epoch; every live writer declares its held windows on each renewal so a
//! successor adopts/releases a crashed incarnation's bits)";
//! `docs/porting/sym_data_planes.md` §3). PORTABLE.
//!
//! The holder's `Ledger` is a fixed table of open grants keyed by appender
//! id (never a `String`-keyed map with a `Vec` per carve, 0002 row 236);
//! `grant_frontier` = the end of the highest open grant — "the zombie
//! floor a successor carves above": a fenced-but-live predecessor may be
//! DMA-ing into offsets it minted and never published, and derived state
//! cannot see them, so the recovery floor is the grant's END. The bits are
//! SET at the carve, under the bitmap's ordering lock, BEFORE any reply.
//!
//! The writer's `GrantWindow` mints lowest-first, gives a minted-never-
//! published block back (merged onto its neighbours), and PULLS a top-up
//! at 50 % (`remaining × 2 < reference`) — single-flight, off the mint
//! path: "a burst of mints costs one wire round trip, and the exhausted
//! mint's inline ask stays the belt". The WRITER measures its own block
//! rate and names `want` (decision 0003 §E: the reference never fed the
//! EWMA term, so every grant was the floor); the holder CAPS the ask by
//! `cap / (2 × writers)`. An unreachable or deferred holder is the
//! retryable class and never latches `StorageFull`.

const std = @import("std");
const Atomic = std.atomic.Value;
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const counters = @import("../../core/counters.zig");
const log = @import("../../core/log.zig");
const derive = @import("../../core/derive.zig");
const block_alloc = @import("../block_alloc.zig");
const data_bitmap = @import("data_bitmap.zig");

pub const BlockAllocator = block_alloc.BlockAllocator;
pub const DataBitmap = data_bitmap.DataBitmap;

// ---------------------------------------------------------------------------
// §3.1 `G` — the derivation and its terms.
// ---------------------------------------------------------------------------

/// ONE write-pipeline BDP window of 4 MiB blocks: 256 MiB in flight is
/// what a single streaming writer keeps between two beats
/// (`write_pipeline_depth_target`'s shipped floor posture), so a writer
/// never asks the holder more than once per window at the norm.
pub const BLOCK_GRANT_FLOOR: u64 = 64;
/// The shipped membership beat and owner TTL (`fuse_client.rs:L191-195`;
/// 0002 row 188): `T_renewal = min(T_self / 3, 10 s)` — three attempts
/// before the member's own deadline.
pub const T_OWNER_MS: u64 = 45_000;
pub const BEAT_MAX_MS: u64 = 10_000;

/// `renew_interval = max(1 ms, min(T_self / 3, 10 s))`.
pub fn renewIntervalMs(t_self_ms: u64) u64 {
    return @max(1, @min(t_self_ms / 3, BEAT_MAX_MS));
}

/// `want = ewma_mblk × 2 × t_ms / 1,000,000` (milli-blocks per second ×
/// ms); `cap_term = max(floor, cap / (2 × max(writers, 1)))`; `clamp(want,
/// floor, cap_term)`. The `2 ×` is one EWMA window of under-estimate; the
/// cap keeps half the volume for reuse; a cap below the floor is the floor
/// ("a tiny volume grants the floor and lets the carve truncate at what is
/// free").
pub fn deriveGrant(ewma_milli_blocks_per_s: u64, t_renewal_ms: u64, capacity_blocks: u64, writers: u64) u64 {
    const want = (@as(u128, ewma_milli_blocks_per_s) * 2 * t_renewal_ms) / 1_000_000;
    const cap_term = @max(BLOCK_GRANT_FLOOR, capacity_blocks / (2 * @max(writers, 1)));
    return derive.clamp(@intCast(@min(want, std.math.maxInt(u64))), BLOCK_GRANT_FLOOR, cap_term);
}

/// The holder's CAP of a wire `want` (its screen): the derivation at an
/// unbounded rate — `cap / (2 × writers)`, never below the floor.
pub fn capGrant(want: u64, capacity_blocks: u64, writers: u64) u64 {
    return @min(@max(want, BLOCK_GRANT_FLOOR), deriveGrant(std.math.maxInt(u64) / 4, 1, capacity_blocks, writers));
}

/// The writer's own block rate: an EWMA (α = ¼ per one-second window) in
/// milli-blocks per second, fed by every mint.
pub const RateMeter = struct {
    ewma_mblk_per_s: Atomic(u64) = .init(0),
    window_start_ns: Atomic(u64) = .init(0),
    window_blocks: Atomic(u64) = .init(0),

    pub const WINDOW_NS: u64 = std.time.ns_per_s;

    pub fn note(self: *RateMeter, blocks: u64, now_ns: u64) void {
        const start = self.window_start_ns.load(.acquire);
        if (start == 0) {
            self.window_start_ns.store(now_ns, .release);
            _ = self.window_blocks.fetchAdd(blocks, .monotonic);
            return;
        }
        if (now_ns -| start >= WINDOW_NS) {
            const n = self.window_blocks.swap(0, .acq_rel) + blocks;
            const elapsed_ns = @max(now_ns - start, 1);
            const rate: u64 = @intCast((@as(u128, n) * 1000 * std.time.ns_per_s) / elapsed_ns);
            const prev = self.ewma_mblk_per_s.load(.monotonic);
            self.ewma_mblk_per_s.store(prev - prev / 4 + rate / 4, .release);
            self.window_start_ns.store(now_ns, .release);
            return;
        }
        _ = self.window_blocks.fetchAdd(blocks, .monotonic);
    }

    pub fn ewmaMilliBlocksPerS(self: *const RateMeter) u64 {
        return self.ewma_mblk_per_s.load(.acquire);
    }
};

// ---------------------------------------------------------------------------
// Stats (§7.2).
// ---------------------------------------------------------------------------

pub const stats = struct {
    pub var block_grants: counters.ShardedCounter = .{};
    pub var block_grant_blocks: counters.ShardedCounter = .{};
    pub var block_grant_blocks_returned: counters.ShardedCounter = .{};
    pub var block_grants_revoked: counters.ShardedCounter = .{};
    pub var block_grant_replays: counters.ShardedCounter = .{};
    pub var block_grant_full: counters.ShardedCounter = .{};
    pub var block_grant_deferred: counters.ShardedCounter = .{};
    /// Deaths × holdings with grants quarantined.
    pub var dead_members_quarantined: counters.ShardedCounter = .{};
    // Writer side.
    pub var block_grant_topups: counters.ShardedCounter = .{};
    pub var block_grant_window_recycles: counters.ShardedCounter = .{};
    /// Mints refused on an empty window.
    pub var block_grant_exhausted: counters.ShardedCounter = .{};
    pub var block_grant_topup_unreachable: counters.ShardedCounter = .{};
    pub var block_grant_topup_deferred: counters.ShardedCounter = .{};
    pub var block_grant_topup_refused: counters.ShardedCounter = .{};
    // Declared windows and the deferred-leak protocol (closure `deferred
    // ≡ released + adopted + pending`).
    pub var data_alloc_bitmap_leaks_deferred: counters.ShardedCounter = .{};
    pub var data_alloc_bitmap_leaks_released: counters.ShardedCounter = .{};
    pub var data_alloc_bitmap_leaks_adopted: counters.ShardedCounter = .{};
    pub var data_alloc_bitmap_decls_screened: counters.ShardedCounter = .{};
    /// Must-stay-0: a poisoned window declaration.
    pub var data_alloc_bitmap_decls_rejected: counters.Tripwire = .init("data_alloc_bitmap_decls_rejected");
    /// Levels.
    pub var block_grant_window_remaining: counters.Gauge = .{};
    pub var data_alloc_bitmap_leaks_pending: counters.Gauge = .{};
};

// ---------------------------------------------------------------------------
// §3.2 The holder's ledger.
// ---------------------------------------------------------------------------

pub const BlockGrant = struct {
    start: u64,
    len: u32,

    pub fn end(self: BlockGrant) u64 {
        return self.start + self.len;
    }

    pub fn contains(self: BlockGrant, idx: u64) bool {
        return idx >= self.start and idx < self.end();
    }
};

/// The most grants one reply carries / one window holds: a writer's
/// window holds at most two floor-sized grants plus their splits across
/// a few beats; 32 is a generous ceiling on that, not a knob.
pub const GRANTS_MAX: usize = 32;

pub const Grants = struct {
    buf: [GRANTS_MAX]BlockGrant = undefined,
    n: u8 = 0,

    pub fn slice(self: *const Grants) []const BlockGrant {
        return self.buf[0..self.n];
    }

    pub fn push(self: *Grants, g: BlockGrant) bool {
        if (self.n >= GRANTS_MAX) return false;
        self.buf[self.n] = g;
        self.n += 1;
        return true;
    }
};

const Slot = struct {
    used: bool = false,
    writer: u32 = 0,
    grant: BlockGrant = .{ .start = 0, .len = 0 },
};

pub const CarveResult = union(enum) {
    granted: BlockGrant,
    /// A replayed frame carves nothing: the writer's open grants.
    already: Grants,
    /// No clear bit remains.
    full,
    /// The table is full (a bounded belt, never growth under the lock).
    table_full,
};

pub const Ledger = struct {
    gpa: Allocator,
    slots: []Slot,

    pub fn init(gpa: Allocator, capacity: usize) Allocator.Error!Ledger {
        const slots = try gpa.alloc(Slot, capacity);
        @memset(slots, .{});
        return .{ .gpa = gpa, .slots = slots };
    }

    pub fn deinit(self: *Ledger) void {
        self.gpa.free(self.slots);
        self.* = undefined;
    }

    /// The highest granted block + 1 over every open grant.
    pub fn grantFrontier(self: *const Ledger) u64 {
        var f: u64 = 0;
        for (self.slots) |s| if (s.used) {
            f = @max(f, s.grant.end());
        };
        return f;
    }

    pub fn openCount(self: *const Ledger) usize {
        var n: usize = 0;
        for (self.slots) |s| if (s.used) {
            n += 1;
        };
        return n;
    }

    /// Distinct writers with open grants.
    pub fn writerCount(self: *const Ledger) u64 {
        var seen: [GRANTS_MAX * 4]u32 = undefined;
        var n: usize = 0;
        for (self.slots) |s| {
            if (!s.used) continue;
            var dup = false;
            for (seen[0..n]) |w| dup = dup or w == s.writer;
            if (!dup and n < seen.len) {
                seen[n] = s.writer;
                n += 1;
            }
        }
        return n;
    }

    pub fn writerHasGrants(self: *const Ledger, writer: u32) bool {
        for (self.slots) |s| if (s.used and s.writer == writer) return true;
        return false;
    }

    pub fn writerGrants(self: *const Ledger, writer: u32) Grants {
        var g: Grants = .{};
        for (self.slots) |s| if (s.used and s.writer == writer) {
            _ = g.push(s.grant);
        };
        return g;
    }

    /// Every open range, for the census oracle's `granted` bitset.
    pub fn openRangesInto(self: *const Ledger, granted_words: []u64) void {
        @memset(granted_words, 0);
        for (self.slots) |s| if (s.used) {
            var idx = s.grant.start;
            while (idx < s.grant.end()) : (idx += 1) granted_words[@intCast(idx / 64)] |= @as(u64, 1) << @intCast(idx % 64);
        };
    }

    pub fn isGranted(self: *const Ledger, idx: u64) bool {
        for (self.slots) |s| if (s.used and s.grant.contains(idx)) return true;
        return false;
    }

    fn pushSlot(self: *Ledger, writer: u32, g: BlockGrant) bool {
        for (self.slots) |*s| if (!s.used) {
            s.* = .{ .used = true, .writer = writer, .grant = g };
            return true;
        };
        return false;
    }

    /// Idempotency first (`held_unconsumed ≥ want ∧ open grants` ⇒
    /// `Already`), then the lowest clear run at/above the frontier (the
    /// bitmap falls back to 0), the bits SET under the bitmap's lock, the
    /// grant pushed under the writer's name.
    pub fn carve(self: *Ledger, bitmap: *DataBitmap, writer: u32, want: u64, held_unconsumed: u64) CarveResult {
        assert(want >= 1);
        if (held_unconsumed >= want and self.writerHasGrants(writer)) {
            stats.block_grant_replays.inc();
            return .{ .already = self.writerGrants(writer) };
        }
        var free_slot = false;
        for (self.slots) |s| free_slot = free_slot or !s.used;
        if (!free_slot) return .table_full;
        const run = bitmap.carveRun(self.grantFrontier(), want) orelse {
            stats.block_grant_full.inc();
            return .full;
        };
        const g: BlockGrant = .{ .start = run.start, .len = run.len };
        const pushed = self.pushSlot(writer, g);
        assert(pushed);
        stats.block_grants.inc();
        stats.block_grant_blocks.add(run.len);
        return .{ .granted = g };
    }

    /// Give a carved run back (a failed drain): the table entry removed,
    /// the bits CLEARED with their delta.
    pub fn ungrant(self: *Ledger, bitmap: *DataBitmap, writer: u32, g: BlockGrant) void {
        for (self.slots) |*s| if (s.used and s.writer == writer and s.grant.start == g.start and s.grant.len == g.len) {
            s.used = false;
            break;
        };
        _ = bitmap.clearRun(g.start, g.len);
    }

    /// `ReturnBlocks`: the ONE covering grant of that writer, split around
    /// the range, the bits CLEARED under the lock; null for a range the
    /// ledger does not grant that writer ("a wire integer is never an
    /// allocation authority").
    pub fn returnBlocks(self: *Ledger, bitmap: *DataBitmap, writer: u32, start: u64, len: u32) ?u32 {
        if (len == 0) return null;
        const end = std.math.add(u64, start, len) catch return null;
        const slot = for (self.slots) |*s| {
            if (s.used and s.writer == writer and start >= s.grant.start and end <= s.grant.end()) break s;
        } else return null;
        const g = slot.grant;
        slot.used = false;
        if (start > g.start) {
            const ok = self.pushSlot(writer, .{ .start = g.start, .len = @intCast(start - g.start) });
            assert(ok);
        }
        if (end < g.end()) {
            const ok = self.pushSlot(writer, .{ .start = end, .len = @intCast(g.end() - end) });
            assert(ok);
        }
        const cleared = bitmap.clearRun(start, len);
        stats.block_grant_blocks_returned.add(cleared);
        return cleared;
    }

    /// A dead writer's grants leave the table; the BITS STAY SET (the
    /// quarantine owns them now). The ranges are written to `out`.
    pub fn revokeDead(self: *Ledger, writer: u32, out: *Grants) usize {
        var n: usize = 0;
        for (self.slots) |*s| if (s.used and s.writer == writer) {
            if (out.push(s.grant)) n += 1;
            s.used = false;
            stats.block_grants_revoked.inc();
        };
        return n;
    }

    /// A declared range adopted under the writer's name (no bit moves: "a
    /// wire word sets no bit").
    pub fn adopt(self: *Ledger, writer: u32, g: BlockGrant) bool {
        return self.pushSlot(writer, g);
    }

    /// A clean leave returns every grant of the writer.
    pub fn releaseWriter(self: *Ledger, bitmap: *DataBitmap, writer: u32) u64 {
        var cleared: u64 = 0;
        for (self.slots) |*s| if (s.used and s.writer == writer) {
            s.used = false;
            cleared += bitmap.clearRun(s.grant.start, s.grant.len);
        };
        stats.block_grant_blocks_returned.add(cleared);
        return cleared;
    }
};

// ---------------------------------------------------------------------------
// §3.3 The writer's window.
// ---------------------------------------------------------------------------

/// Sorted by `start`, non-overlapping; `install` refuses an equal or
/// overlapping grant (idempotent); `mint` consumes the LOWEST grant's
/// first block; `giveBack` merges onto an adjacent range. The lock is a
/// spin lock: every section is O(GRANTS_MAX) and the mint path carries no
/// `Io`.
pub const GrantWindow = struct {
    lock: SpinLock = .{},
    grants: [GRANTS_MAX]BlockGrant = undefined,
    n: u8 = 0,
    /// Blocks remaining (published lock-free for the supply arithmetic).
    remaining_blocks: Atomic(u64) = .init(0),
    installed: Atomic(u64) = .init(0),
    consumed: Atomic(u64) = .init(0),
    /// The top-up reference: `max(len installed)`.
    reference: Atomic(u64) = .init(0),
    exhausted: Atomic(u64) = .init(0),
    /// The single-flight top-up latch.
    topup_inflight: Atomic(bool) = .init(false),

    const SpinLock = @import("../../meta/journal_core.zig").SpinLock;

    fn recount(self: *GrantWindow) void {
        var r: u64 = 0;
        for (self.grants[0..self.n]) |g| r += g.len;
        self.remaining_blocks.store(r, .release);
        stats.block_grant_window_remaining.set(r);
    }

    /// False ⇔ equal/overlapping (already installed) or the window is full.
    pub fn install(self: *GrantWindow, g: BlockGrant) bool {
        if (g.len == 0) return false;
        self.lock.lock();
        defer self.lock.unlock();
        if (self.n >= GRANTS_MAX) return false;
        var pos: usize = 0;
        for (self.grants[0..self.n]) |cur| {
            if (g.start < cur.end() and cur.start < g.end()) return false;
            if (cur.start < g.start) pos += 1;
        }
        var i: usize = self.n;
        while (i > pos) : (i -= 1) self.grants[i] = self.grants[i - 1];
        self.grants[pos] = g;
        self.n += 1;
        _ = self.installed.fetchAdd(g.len, .monotonic);
        _ = self.reference.fetchMax(g.len, .monotonic);
        self.recount();
        return true;
    }

    /// The lowest grant's first block, or null on an empty window (a
    /// refused mint moves nothing).
    pub fn mint(self: *GrantWindow) ?u64 {
        self.lock.lock();
        defer self.lock.unlock();
        if (self.n == 0) return null;
        const idx = self.grants[0].start;
        self.grants[0].start += 1;
        self.grants[0].len -= 1;
        if (self.grants[0].len == 0) {
            var i: usize = 0;
            while (i + 1 < self.n) : (i += 1) self.grants[i] = self.grants[i + 1];
            self.n -= 1;
        }
        _ = self.consumed.fetchAdd(1, .monotonic);
        self.recount();
        return idx;
    }

    /// A minted-never-published block back into the window, merged onto an
    /// adjacent range; `false` ⇔ already unconsumed ("the double-handout
    /// lineage") or no room.
    pub fn giveBack(self: *GrantWindow, idx: u64) bool {
        self.lock.lock();
        defer self.lock.unlock();
        var pos: usize = 0;
        for (self.grants[0..self.n], 0..) |*g, i| {
            if (g.contains(idx)) return false;
            if (g.end() == idx) {
                // Merge onto the left neighbour; absorb a right neighbour too.
                g.len += 1;
                if (i + 1 < self.n and self.grants[i + 1].start == g.end()) {
                    g.len += self.grants[i + 1].len;
                    var j: usize = i + 1;
                    while (j + 1 < self.n) : (j += 1) self.grants[j] = self.grants[j + 1];
                    self.n -= 1;
                }
                return self.gaveBack();
            }
            if (g.start == idx + 1) {
                g.start -= 1;
                g.len += 1;
                return self.gaveBack();
            }
            if (g.start < idx) pos = i + 1;
        }
        if (self.n >= GRANTS_MAX) return false;
        var i: usize = self.n;
        while (i > pos) : (i -= 1) self.grants[i] = self.grants[i - 1];
        self.grants[pos] = .{ .start = idx, .len = 1 };
        self.n += 1;
        return self.gaveBack();
    }

    fn gaveBack(self: *GrantWindow) bool {
        _ = self.consumed.fetchSub(1, .monotonic);
        stats.block_grant_window_recycles.inc();
        self.recount();
        return true;
    }

    pub fn remaining(self: *const GrantWindow) u64 {
        return self.remaining_blocks.load(.acquire);
    }

    /// `remaining × 2 < reference ∨ reference == 0` — the 50 % law.
    pub fn wantsTopup(self: *const GrantWindow) bool {
        const r = self.reference.load(.acquire);
        return r == 0 or self.remaining() * 2 < r;
    }

    pub fn noteExhausted(self: *GrantWindow) void {
        _ = self.exhausted.fetchAdd(1, .monotonic);
        stats.block_grant_exhausted.inc();
    }

    /// The leave: every range out, into `out`.
    pub fn drain(self: *GrantWindow, out: *Grants) void {
        self.lock.lock();
        defer self.lock.unlock();
        for (self.grants[0..self.n]) |g| _ = out.push(g);
        self.n = 0;
        self.recount();
    }

    /// The unconsumed ranges, ascending (the renewal's declaration).
    pub fn ranges(self: *GrantWindow) Grants {
        self.lock.lock();
        defer self.lock.unlock();
        var g: Grants = .{};
        for (self.grants[0..self.n]) |r| _ = g.push(r);
        return g;
    }

    // ---- the allocator's `GrantSource` seam ----

    fn srcMint(ctx: *anyopaque) ?u64 {
        const w: *GrantWindow = @ptrCast(@alignCast(ctx));
        return w.mint();
    }
    fn srcGiveBack(ctx: *anyopaque, idx: u64) bool {
        const w: *GrantWindow = @ptrCast(@alignCast(ctx));
        return w.giveBack(idx);
    }
    fn srcRemaining(ctx: *anyopaque) u64 {
        const w: *GrantWindow = @ptrCast(@alignCast(ctx));
        return w.remaining();
    }
    fn srcWantsTopup(ctx: *anyopaque) bool {
        const w: *GrantWindow = @ptrCast(@alignCast(ctx));
        return w.wantsTopup();
    }
    fn srcNoteExhausted(ctx: *anyopaque) void {
        const w: *GrantWindow = @ptrCast(@alignCast(ctx));
        w.noteExhausted();
    }
    const src_vtable: block_alloc.GrantSource.VTable = .{ .mint = &srcMint, .giveBack = &srcGiveBack, .remaining = &srcRemaining, .wantsTopup = &srcWantsTopup, .noteExhausted = &srcNoteExhausted };

    pub fn grantSource(self: *GrantWindow) block_alloc.GrantSource {
        return .{ .ctx = self, .vtable = &src_vtable };
    }

    /// The lease arm's `window_remaining` seam.
    pub fn remainingFn(ctx: ?*anyopaque) u64 {
        const w: *GrantWindow = @ptrCast(@alignCast(ctx.?));
        return w.remaining();
    }
};

/// The declared windows: the unconsumed ranges PLUS the in-flight
/// (minted, unpublished) blocks the allocator registers, coalesced
/// ascending — "a write between its mint and its layout commit holds a
/// block that is neither in the window nor referenced, and a failover
/// inside that window must not read it as nobody's".
pub fn heldBlockRanges(window: *GrantWindow, alloc: *BlockAllocator, gpa: Allocator, out: *std.ArrayList(BlockGrant)) Allocator.Error!void {
    const w = window.ranges();
    var idx: u64 = 0;
    var wi: usize = 0;
    const cap = alloc.capacityBlocks();
    while (idx < cap) : (idx += 1) {
        var held = alloc.inflightContains(idx);
        while (wi < w.n and w.buf[wi].end() <= idx) wi += 1;
        if (wi < w.n and w.buf[wi].contains(idx)) held = true;
        if (!held) continue;
        if (out.items.len > 0 and out.items[out.items.len - 1].end() == idx) {
            out.items[out.items.len - 1].len += 1;
        } else try out.append(gpa, .{ .start = idx, .len = 1 });
    }
}

// ---------------------------------------------------------------------------
// §3.4 The verbs: the holder's handlers and the writer's sink.
// ---------------------------------------------------------------------------

/// The holder's word carried on every reply (0002 row 241: a joiner's `df`).
pub const HolderWord = struct { blocks: u64, population: u64, frontier: u64 };

pub const GrantReply = union(enum) {
    granted: struct { grants: Grants, already: bool, word: HolderWord },
    full: HolderWord,
    /// The holder's ring-0 user window full for the beat, its lease in
    /// motion: retry.
    deferred,
    /// This mount does not hold the lease (or is homed elsewhere).
    busy,
};

/// The holder's grant plane for one data volume.
pub const HolderGrants = struct {
    ledger: Ledger,
    bitmap: *DataBitmap,
    /// The ledger's writers + 1 (the holder itself).
    pub fn writers(self: *const HolderGrants) u64 {
        return self.ledger.writerCount() + 1;
    }

    pub fn word(self: *const HolderGrants) HolderWord {
        return .{ .blocks = self.bitmap.blocks(), .population = self.bitmap.populationNow(), .frontier = self.bitmap.highestSetPlusOne() };
    }

    /// `BlockGrant{writer, want, held_unconsumed}`: `want == 0` ⇒ the
    /// derivation at the floor; else the holder CAPS the writer's named
    /// want; carve → drain → the reply. A refused drain gives the bits back
    /// and answers `deferred`.
    pub fn blockGrant(self: *HolderGrants, writer: u32, want: u64, held_unconsumed: u64) GrantReply {
        const w = if (want == 0) deriveGrant(0, BEAT_MAX_MS, self.bitmap.blocks(), self.writers()) else capGrant(want, self.bitmap.blocks(), self.writers());
        switch (self.ledger.carve(self.bitmap, writer, w, held_unconsumed)) {
            .already => |g| return .{ .granted = .{ .grants = g, .already = true, .word = self.word() } },
            .full => return .{ .full = self.word() },
            .table_full => {
                stats.block_grant_deferred.inc();
                return .deferred;
            },
            .granted => |g| {
                switch (self.bitmap.drain()) {
                    .drained, .nothing, .busy => {},
                    .no_journal => {},
                    .deferred => {
                        // The SET is not durable: no reply may name the run.
                        self.ledger.ungrant(self.bitmap, writer, g);
                        _ = self.bitmap.drain();
                        stats.block_grant_deferred.inc();
                        return .deferred;
                    },
                }
                var gs: Grants = .{};
                _ = gs.push(g);
                return .{ .granted = .{ .grants = gs, .already = false, .word = self.word() } };
            },
        }
    }

    pub const ReturnReply = union(enum) { returned: u32, not_granted, deferred };

    /// `ReturnBlocks{writer, start, len}` → the CLEAR deltas drained in order.
    pub fn returnBlocks(self: *HolderGrants, writer: u32, start: u64, len: u32) ReturnReply {
        const cleared = self.ledger.returnBlocks(self.bitmap, writer, start, len) orelse return .not_granted;
        if (self.bitmap.drain() == .deferred) {
            // The bits are clear in RAM and the page covers them; the
            // CLEAR stays queued for the next kick (leak-safe).
            return .{ .returned = cleared };
        }
        return .{ .returned = cleared };
    }

    /// §3.6: at the death record — the dead writer's grants leave the table
    /// (bits SET) and every block of them is quarantined under `dead_epoch`
    /// on the allocator. Returns the blocks quarantined.
    pub fn revokeDeadWriter(self: *HolderGrants, writer: u32, dead_epoch: u64) u64 {
        var revoked: Grants = .{};
        _ = self.ledger.revokeDead(writer, &revoked);
        var n: u64 = 0;
        for (revoked.slice()) |g| {
            var idx = g.start;
            while (idx < g.end()) : (idx += 1) {
                if (self.bitmap.alloc.quarantineBlock(idx, dead_epoch)) n += 1;
            }
        }
        if (revoked.n > 0) stats.dead_members_quarantined.inc();
        return n;
    }
};

/// The writer's face of the holder: in-process (the holder in this
/// process) or the wire's client — both answer a `GrantAnswer`.
pub const GrantAnswer = union(enum) {
    granted: struct { grants: Grants, already: bool, word: HolderWord },
    full: HolderWord,
    deferred,
    holder_unreachable,
    refused,
};

pub const GrantSink = struct {
    ctx: *anyopaque,
    ask: *const fn (*anyopaque, want: u64, held_unconsumed: u64) GrantAnswer,
};

pub const TopupOutcome = enum { not_asked, landed, full, holder_unreachable, deferred, refused };

/// `Unreachable` / `Deferred` are the retryable classes (NEVER a latched
/// `StorageFull`: "a manager failover on a joined writer with a drained
/// window"); `Refused` is loud; `Full` / `Landed` leave the verdict to the
/// window.
pub fn classifyTopupAnswer(a: GrantAnswer) TopupOutcome {
    return switch (a) {
        .granted => .landed,
        .full => .full,
        .deferred => .deferred,
        .holder_unreachable => .holder_unreachable,
        .refused => .refused,
    };
}

/// The writer's arm: window + meter + sink + the lease arm's holder word.
pub const WriterArm = struct {
    window: GrantWindow = .{},
    meter: RateMeter = .{},
    sink: ?GrantSink = null,
    /// The volume's block count (the cap term) and the writers the last
    /// word named (the holder caps anyway; the writer's own clamp keeps
    /// the ask honest).
    capacity_blocks: u64,
    t_renewal_ms: u64 = BEAT_MAX_MS,
    last_word: ?HolderWord = null,

    /// The writer measures and names `want` (decision 0003 §E).
    pub fn namedWant(self: *const WriterArm) u64 {
        return deriveGrant(self.meter.ewmaMilliBlocksPerS(), self.t_renewal_ms, self.capacity_blocks, 1);
    }

    /// The PULL: single-flight; asks `(want, held = remaining)` when the
    /// window wants a top-up; installs every answered grant.
    pub fn topup(self: *WriterArm) TopupOutcome {
        if (!self.window.wantsTopup()) return .not_asked;
        const sink = self.sink orelse return .holder_unreachable;
        if (self.window.topup_inflight.cmpxchgStrong(false, true, .acq_rel, .acquire) != null) return .not_asked;
        defer self.window.topup_inflight.store(false, .release);
        stats.block_grant_topups.inc();
        const answer = sink.ask(sink.ctx, self.namedWant(), self.window.remaining());
        switch (answer) {
            .granted => |g| {
                for (g.grants.slice()) |grant| _ = self.window.install(grant);
                self.last_word = g.word;
            },
            .full => |w| self.last_word = w,
            .deferred => stats.block_grant_topup_deferred.inc(),
            .holder_unreachable => stats.block_grant_topup_unreachable.inc(),
            .refused => {
                stats.block_grant_topup_refused.inc();
                log.err("block grant top-up REFUSED by the holder — a screened word this writer sent is malformed (InvalidOperation class); the window is left as it is", .{});
            },
        }
        return classifyTopupAnswer(answer);
    }
};

/// The in-process holder as a writer's sink (the same process holds the
/// lease, or the test's loopback).
pub const LocalSink = struct {
    holder: *HolderGrants,
    writer: u32,
    /// Test seams: the next N asks answer the named class.
    unreachable_next: u32 = 0,

    fn ask(ctx: *anyopaque, want: u64, held: u64) GrantAnswer {
        const s: *LocalSink = @ptrCast(@alignCast(ctx));
        if (s.unreachable_next > 0) {
            s.unreachable_next -= 1;
            return .holder_unreachable;
        }
        return switch (s.holder.blockGrant(s.writer, want, held)) {
            .granted => |g| .{ .granted = .{ .grants = g.grants, .already = g.already, .word = g.word } },
            .full => |w| .{ .full = w },
            .deferred => .deferred,
            .busy => .holder_unreachable,
        };
    }

    pub fn sink(self: *LocalSink) GrantSink {
        return .{ .ctx = self, .ask = &ask };
    }
};

// ---------------------------------------------------------------------------
// §3.7 Declared windows and the deferred-leak release.
// ---------------------------------------------------------------------------

/// `screen_window_decl`: every range non-empty, inside `[0, blocks)`
/// judged without wrap, ascending, disjoint; one bad range refuses the
/// whole word (the must-stay-0 tripwire).
pub fn screenWindowDecl(ranges: []const BlockGrant, blocks: u64) bool {
    stats.data_alloc_bitmap_decls_screened.inc();
    var prev_end: u64 = 0;
    for (ranges, 0..) |r, i| {
        if (r.len == 0) return rejectDecl();
        const end = std.math.add(u64, r.start, r.len) catch return rejectDecl();
        if (end > blocks) return rejectDecl();
        if (i > 0 and r.start < prev_end) return rejectDecl();
        prev_end = end;
    }
    return true;
}

fn rejectDecl() bool {
    stats.data_alloc_bitmap_decls_rejected.fire("a poisoned window declaration");
    return false;
}

/// A SET bit nothing references and no grant names is either a dead
/// incarnation's remainder or a LIVE writer's window the predecessor's RAM
/// ledger granted and died with — indistinguishable from the bitmap alone.
/// Every candidate enters `pending`; once every live peer has declared (or
/// is dead), what is still pending is released (bit CLEAR + delta, in
/// order). Closure: `deferred ≡ released + adopted + pending`.
pub const DeferredLeaks = struct {
    gpa: Allocator,
    pending: []u64,
    blocks: u64,
    deferred: u64 = 0,
    released: u64 = 0,
    adopted: u64 = 0,

    pub fn init(gpa: Allocator, blocks: u64) Allocator.Error!DeferredLeaks {
        const words = try gpa.alloc(u64, @intCast((blocks + 63) / 64));
        @memset(words, 0);
        return .{ .gpa = gpa, .pending = words, .blocks = blocks };
    }

    pub fn deinit(self: *DeferredLeaks) void {
        self.gpa.free(self.pending);
    }

    pub fn pendingCount(self: *const DeferredLeaks) u64 {
        var n: u64 = 0;
        for (self.pending) |w| n += @popCount(w);
        return n;
    }

    fn isPending(self: *const DeferredLeaks, idx: u64) bool {
        return self.pending[@intCast(idx / 64)] & (@as(u64, 1) << @intCast(idx % 64)) != 0;
    }

    fn clearPending(self: *DeferredLeaks, idx: u64) void {
        self.pending[@intCast(idx / 64)] &= ~(@as(u64, 1) << @intCast(idx % 64));
    }

    /// The census LEAK half at the (re-)hold: every candidate is deferred.
    pub fn deferLeaks(self: *DeferredLeaks, candidates: []const u64) void {
        for (candidates) |idx| {
            assert(idx < self.blocks);
            if (self.isPending(idx)) continue;
            self.pending[@intCast(idx / 64)] |= @as(u64, 1) << @intCast(idx % 64);
            self.deferred += 1;
            stats.data_alloc_bitmap_leaks_deferred.inc();
        }
        stats.data_alloc_bitmap_leaks_pending.set(self.pendingCount());
    }

    /// A live writer's screened declaration: the PENDING ∩ range blocks
    /// whose bits are SET are ADOPTED into the ledger under the writer's
    /// name as runs (its death revokes them, its leave returns them).
    pub fn adoptDeclared(self: *DeferredLeaks, ledger: *Ledger, bitmap: *const DataBitmap, writer: u32, ranges: []const BlockGrant) u64 {
        var adopted: u64 = 0;
        for (ranges) |r| {
            var run_start: ?u64 = null;
            var idx = r.start;
            while (idx <= r.end()) : (idx += 1) {
                const take = idx < r.end() and self.isPending(idx) and bitmap.isSet(idx) and !ledger.isGranted(idx);
                if (take) {
                    if (run_start == null) run_start = idx;
                    self.clearPending(idx);
                    adopted += 1;
                } else if (run_start) |s| {
                    _ = ledger.adopt(writer, .{ .start = s, .len = @intCast(idx - s) });
                    run_start = null;
                }
            }
        }
        self.adopted += adopted;
        stats.data_alloc_bitmap_leaks_adopted.add(adopted);
        stats.data_alloc_bitmap_leaks_pending.set(self.pendingCount());
        return adopted;
    }

    /// Once every live peer declared (`all_declared`), the remainder is
    /// released: bit CLEAR + delta, ascending.
    pub fn convergeDeferred(self: *DeferredLeaks, bitmap: *DataBitmap, all_declared: bool) u64 {
        if (!all_declared) return 0;
        var released: u64 = 0;
        for (self.pending, 0..) |*w, wi| {
            var bits = w.*;
            while (bits != 0) : (bits &= bits - 1) {
                const idx = @as(u64, wi) * 64 + @ctz(bits);
                if (bitmap.isSet(idx)) released += bitmap.clearRun(idx, 1);
            }
            w.* = 0;
        }
        self.released += released;
        stats.data_alloc_bitmap_leaks_released.add(released);
        stats.data_alloc_bitmap_leaks_pending.set(0);
        return released;
    }

    /// `deferred ≡ released + adopted + pending`.
    pub fn closureHolds(self: *const DeferredLeaks) bool {
        return self.deferred == self.released + self.adopted + self.pendingCount();
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

fn testAlloc(blocks: u64) !*BlockAllocator {
    return BlockAllocator.init(testing.allocator, .{ .vol_tag = 0x61, .vol = 0, .block_size = 4096, .capacity_bytes = blocks * 4096 });
}

fn testBitmap(alloc: *BlockAllocator) !*DataBitmap {
    return DataBitmap.init(testing.allocator, alloc, .{ .vol_tag = 0x61, .term = 1, .blocks = alloc.capacityBlocks(), .ring_capacity = data_bitmap.ringCapacity(BLOCK_GRANT_FLOOR, 4, 64) });
}

test "the_block_grant_derivation_is_clamped_between_its_floor_and_cap (BG:L634-648): 100 blocks/s × 10 s × 2 = 2,000; 1,024/(2×4) = 128; a 100-block volume grants the floor; the beat derives min(T_self/3, 10 s)" {
    try testing.expectEqual(@as(u64, 64), BLOCK_GRANT_FLOOR);
    try testing.expectEqual(@as(u64, 128), deriveGrant(100_000, 10_000, 1024, 4));
    try testing.expectEqual(@as(u64, 2000), deriveGrant(100_000, 10_000, 1 << 20, 4));
    try testing.expectEqual(@as(u64, 64), deriveGrant(0, 10_000, 1 << 20, 4));
    try testing.expectEqual(@as(u64, 64), deriveGrant(100_000, 10_000, 100, 1));
    try testing.expectEqual(@as(u64, 64), deriveGrant(1_000, 10_000, 1 << 20, 1)); // 20 < floor
    // The cap screen: a wire want above the cap is the cap; below the floor is the floor.
    try testing.expectEqual(@as(u64, 128), capGrant(10_000, 1024, 4));
    try testing.expectEqual(@as(u64, 64), capGrant(3, 1 << 20, 1));
    try testing.expectEqual(@as(u64, 500), capGrant(500, 1 << 20, 1));
    try testing.expectEqual(@as(u64, 10_000), renewIntervalMs(T_OWNER_MS));
    try testing.expectEqual(@as(u64, 3_000), renewIntervalMs(9_000));
    try testing.expectEqual(@as(u64, 1), renewIntervalMs(2));
    // The meter: a window of 1 s at 8 k blocks ⇒ 8 M milli-blocks/s × ¼ into the EWMA.
    var m: RateMeter = .{};
    m.note(4000, 1_000_000_000);
    m.note(4000, 1_500_000_000);
    try testing.expectEqual(@as(u64, 0), m.ewmaMilliBlocksPerS());
    m.note(0, 2_000_000_000);
    try testing.expectEqual(@as(u64, 2_000_000), m.ewmaMilliBlocksPerS());
    try testing.expectEqual(@as(u64, 40_000), deriveGrant(2_000_000, 10_000, 1 << 30, 1)); // 2000 blocks/s × 2 × 10 s
}

test "two_writers_grants_are_disjoint_and_the_zombie_floor_is_the_grants_end; the carve SETs the bits before the reply; a replayed frame carves nothing; Full when no clear bit remains" {
    const a = try testAlloc(256);
    defer a.deinit();
    var i: u64 = 0;
    while (i < 10) : (i += 1) _ = try a.allocateBlock();
    const bm = try testBitmap(a);
    defer bm.deinit();
    var j: data_bitmap.RamJournal = .{ .gpa = testing.allocator };
    defer j.deinit();
    bm.journal = j.sink();
    var h: HolderGrants = .{ .ledger = try Ledger.init(testing.allocator, 16), .bitmap = bm };
    defer h.ledger.deinit();
    try testing.expectEqual(@as(u64, 0), h.ledger.grantFrontier());
    const g1 = h.blockGrant(1, 0, 0).granted;
    try testing.expect(!g1.already);
    try testing.expectEqual(@as(u64, 10), g1.grants.buf[0].start);
    try testing.expectEqual(@as(u32, 64), g1.grants.buf[0].len);
    try testing.expectEqual(@as(u64, 74), h.ledger.grantFrontier());
    try testing.expectEqual(@as(u64, 74), g1.word.population);
    try testing.expectEqual(@as(usize, 1), j.entries.items.len); // the SET journaled before the reply
    const g2 = h.blockGrant(2, 0, 0).granted;
    try testing.expectEqual(@as(u64, 74), g2.grants.buf[0].start);
    try testing.expectEqual(@as(u64, 138), h.ledger.grantFrontier());
    try testing.expectEqual(@as(u64, 3), h.writers());
    for (g1.grants.slice()) |x| for (g2.grants.slice()) |y| try testing.expect(x.end() <= y.start or y.end() <= x.start);
    // A replayed frame: held_unconsumed ≥ want and open grants ⇒ already.
    const replay = h.blockGrant(1, 64, 64).granted;
    try testing.expect(replay.already);
    try testing.expectEqual(@as(u8, 1), replay.grants.n);
    try testing.expectEqual(@as(usize, 2), j.entries.items.len);
    // Exhaust: 256 − 138 = 118 blocks left; the floor carve truncates.
    _ = h.blockGrant(3, 0, 0).granted; // 138..202
    const last = h.blockGrant(4, 0, 0).granted; // 202..256 (54 blocks)
    try testing.expectEqual(@as(u32, 54), last.grants.buf[0].len);
    try testing.expectEqual(GrantReply{ .full = h.word() }, h.blockGrant(5, 0, 0));
    try testing.expectEqual(@as(u64, 256), h.word().population);
    // A terminal free below the floor is found by the fallback carve.
    a.finish_free_sink = bm.finishFreeSink();
    _ = try a.freeBlock(3);
    const hole = h.blockGrant(5, 0, 0).granted;
    try testing.expectEqual(@as(u64, 3), hole.grants.buf[0].start);
    try testing.expectEqual(@as(u32, 1), hole.grants.buf[0].len);
}

test "ReturnBlocks: the ONE covering grant is split around the range, the bits CLEAR under the lock and journal; a range the ledger does not grant that writer is refused; a leave returns everything" {
    const a = try testAlloc(64);
    defer a.deinit();
    const bm = try testBitmap(a);
    defer bm.deinit();
    var j: data_bitmap.RamJournal = .{ .gpa = testing.allocator };
    defer j.deinit();
    bm.journal = j.sink();
    var h: HolderGrants = .{ .ledger = try Ledger.init(testing.allocator, 16), .bitmap = bm };
    defer h.ledger.deinit();
    _ = h.blockGrant(1, 0, 0).granted; // 0..64: the whole volume
    try testing.expectEqual(GrantReply{ .full = h.word() }, h.blockGrant(2, 0, 0));
    try testing.expectEqual(HolderGrants.ReturnReply.not_granted, h.returnBlocks(2, 0, 8));
    try testing.expectEqual(HolderGrants.ReturnReply.not_granted, h.returnBlocks(1, 60, 8)); // past the grant
    try testing.expectEqual(HolderGrants.ReturnReply.not_granted, h.returnBlocks(1, 0, 0));
    try testing.expectEqual(HolderGrants.ReturnReply{ .returned = 8 }, h.returnBlocks(1, 8, 8));
    try testing.expect(!bm.isSet(8) and !bm.isSet(15) and bm.isSet(7) and bm.isSet(16));
    const gs = h.ledger.writerGrants(1);
    try testing.expectEqual(@as(u8, 2), gs.n);
    try testing.expectEqual(@as(u64, 56), bm.populationNow());
    try testing.expect(!j.entries.items[j.entries.items.len - 1].delta.set);
    // Returned bits are carvable again: the only clear run is the hole.
    const again = h.blockGrant(2, 4, 0).granted;
    try testing.expectEqual(@as(u64, 8), again.grants.buf[0].start);
    try testing.expectEqual(@as(u32, 8), again.grants.buf[0].len);
    // The leave clears every remaining grant of writer 1 (8 + 48 blocks);
    // writer 2's 8 stay set.
    try testing.expectEqual(@as(u64, 56), h.ledger.releaseWriter(bm, 1));
    try testing.expectEqual(@as(usize, 1), h.ledger.openCount());
    try testing.expectEqual(@as(u64, 8), bm.populationNow());
}

test "a_death_record_revokes_the_dead_writers_grants_into_the_quarantine: the ranges leave the table, the bits STAY SET, every block is quarantined under the dead epoch; a drain proof releases them through finish_free" {
    const a = try testAlloc(256);
    defer a.deinit();
    const bm = try testBitmap(a);
    defer bm.deinit();
    var h: HolderGrants = .{ .ledger = try Ledger.init(testing.allocator, 16), .bitmap = bm };
    defer h.ledger.deinit();
    const g = h.blockGrant(7, 0, 0).granted.grants.buf[0];
    const q0 = stats.dead_members_quarantined.load();
    try testing.expectEqual(@as(u64, 64), h.revokeDeadWriter(7, 99));
    try testing.expectEqual(q0 + 1, stats.dead_members_quarantined.load());
    try testing.expect(!h.ledger.writerHasGrants(7));
    try testing.expectEqual(@as(u64, 0), h.ledger.grantFrontier()); // the frontier is the table's, the bits stay
    try testing.expect(bm.isSet(g.start) and bm.isSet(g.end() - 1));
    try testing.expect(a.isQuarantined(g.start));
    // A new carve lands ABOVE the quarantined bits (they are set).
    const next = h.blockGrant(8, 0, 0).granted.grants.buf[0];
    try testing.expectEqual(@as(u64, 64), next.start);
    // The release: every owed publish goes back through finishFree → the sink clears the bit.
    a.finish_free_sink = bm.finishFreeSink();
    var out: [64]u64 = undefined;
    const n = a.releaseQuarantine(99, &.{}, &out);
    try testing.expectEqual(@as(usize, 64), n);
    for (out[0..n]) |idx| a.finishFree(idx);
    try testing.expect(!bm.isSet(g.start));
    try testing.expectEqual(@as(u64, 64), bm.populationNow());
    try testing.expectEqual(@as(u64, 0), h.revokeDeadWriter(7, 100)); // nothing left to revoke
}

test "grants_are_disjoint_and_the_window_mints_lowest_first; a_given_back_block_is_the_next_mint_and_merges_onto_its_neighbours; install refuses overlap; wants_topup is the 50 % law" {
    var w: GrantWindow = .{};
    try testing.expect(w.wantsTopup()); // reference 0
    try testing.expect(w.install(.{ .start = 100, .len = 4 }));
    try testing.expect(w.install(.{ .start = 10, .len = 4 }));
    try testing.expect(!w.install(.{ .start = 10, .len = 4 }));
    try testing.expect(!w.install(.{ .start = 102, .len = 10 }));
    try testing.expect(!w.install(.{ .start = 5, .len = 0 }));
    try testing.expectEqual(@as(u64, 8), w.remaining());
    try testing.expect(!w.wantsTopup());
    try testing.expectEqual(@as(?u64, 10), w.mint());
    try testing.expectEqual(@as(?u64, 11), w.mint());
    try testing.expect(w.giveBack(11));
    try testing.expect(!w.giveBack(11)); // already unconsumed
    try testing.expectEqual(@as(?u64, 11), w.mint());
    try testing.expectEqual(@as(?u64, 12), w.mint());
    try testing.expectEqual(@as(?u64, 13), w.mint());
    try testing.expect(!w.wantsTopup()); // 4 remaining × 2 = 8 ≥ reference 4
    try testing.expectEqual(@as(?u64, 100), w.mint());
    try testing.expect(!w.wantsTopup()); // 3 × 2 = 6 ≥ 4
    // Give back 13 then 12: a fresh run 12..14 ahead of 101..104.
    try testing.expect(w.giveBack(13));
    try testing.expect(w.giveBack(12));
    const r = w.ranges();
    try testing.expectEqual(@as(u8, 2), r.n);
    try testing.expectEqual(@as(u64, 12), r.buf[0].start);
    try testing.expectEqual(@as(u32, 2), r.buf[0].len);
    try testing.expectEqual(@as(u64, 101), r.buf[1].start);
    // Giving back 100 merges onto the right neighbour's start.
    try testing.expect(w.giveBack(100));
    try testing.expectEqual(@as(u64, 100), w.ranges().buf[1].start);
    try testing.expectEqual(@as(u32, 4), w.ranges().buf[1].len);
    // Giving back 14 merges onto the left run (12..15).
    try testing.expect(w.giveBack(14));
    try testing.expectEqual(@as(u32, 3), w.ranges().buf[0].len);
    // Giving back 11 then 10 rebuilds 10..15 as ONE run.
    try testing.expect(w.giveBack(11));
    try testing.expect(w.giveBack(10));
    try testing.expectEqual(@as(u8, 2), w.ranges().n);
    try testing.expectEqual(@as(u64, 10), w.ranges().buf[0].start);
    try testing.expectEqual(@as(u32, 5), w.ranges().buf[0].len);
    try testing.expectEqual(@as(?u64, 10), w.mint()); // the given-back block is the next mint
    var drained: Grants = .{};
    w.drain(&drained);
    try testing.expectEqual(@as(u8, 2), drained.n);
    try testing.expectEqual(@as(u64, 0), w.remaining());
    try testing.expectEqual(@as(?u64, null), w.mint());
    w.noteExhausted();
    try testing.expectEqual(@as(u64, 1), w.exhausted.load(.monotonic));
}

test "wants_topup: exactly remaining × 2 < reference" {
    var w: GrantWindow = .{};
    try testing.expect(w.install(.{ .start = 0, .len = 10 }));
    var i: usize = 0;
    while (i < 5) : (i += 1) _ = w.mint();
    try testing.expectEqual(@as(u64, 5), w.remaining());
    try testing.expect(!w.wantsTopup()); // 10 < 10 false
    _ = w.mint();
    try testing.expect(w.wantsTopup()); // 8 < 10
}

test "an_armed_two_writer_set_allocates_disjoint_ranges_from_grants; the PULL top-up is single-flight, names the measured want, installs the answer and carries the holder's word; an unreachable holder is the retryable class never exhaustion" {
    const a = try testAlloc(4096);
    defer a.deinit();
    const bm = try testBitmap(a);
    defer bm.deinit();
    var h: HolderGrants = .{ .ledger = try Ledger.init(testing.allocator, 16), .bitmap = bm };
    defer h.ledger.deinit();
    // Two writers (appender ids 1 and 2), each with its own allocator view
    // of the same volume; the holder is writer 1's process.
    const b = try testAlloc(4096);
    defer b.deinit();
    var sink_a: LocalSink = .{ .holder = &h, .writer = 1 };
    var sink_b: LocalSink = .{ .holder = &h, .writer = 2 };
    var arm_a: WriterArm = .{ .capacity_blocks = 4096, .sink = sink_a.sink() };
    var arm_b: WriterArm = .{ .capacity_blocks = 4096, .sink = sink_b.sink() };
    a.grant_source = arm_a.window.grantSource();
    b.grant_source = arm_b.window.grantSource();
    try testing.expectEqual(TopupOutcome.landed, arm_a.topup());
    try testing.expectEqual(TopupOutcome.landed, arm_b.topup());
    try testing.expectEqual(TopupOutcome.not_asked, arm_a.topup()); // above 50 %
    try testing.expectEqual(@as(u64, 64), arm_a.window.remaining());
    try testing.expectEqual(@as(u64, 64), arm_a.last_word.?.population); // A's reply saw its own 64
    try testing.expectEqual(@as(u64, 128), arm_b.last_word.?.population); // B's saw both
    var minted_a: [96]u64 = undefined;
    var minted_b: [96]u64 = undefined;
    for (&minted_a) |*m| {
        m.* = (try a.allocateBlock()).idx;
        if (arm_a.window.wantsTopup()) _ = arm_a.topup();
    }
    for (&minted_b) |*m| {
        m.* = (try b.allocateBlock()).idx;
        if (arm_b.window.wantsTopup()) _ = arm_b.topup();
    }
    for (minted_a) |x| for (minted_b) |y| try testing.expect(x != y);
    try testing.expectEqual(@as(u64, 4), h.ledger.writerCount() + 1 + 1); // two writers + the holder
    // An unreachable holder: retryable, never a latched StorageFull.
    var drained: Grants = .{};
    arm_a.window.drain(&drained);
    sink_a.unreachable_next = 1;
    try testing.expectEqual(TopupOutcome.holder_unreachable, arm_a.topup());
    try testing.expectError(error.StorageFull, a.allocateBlock()); // the window is empty: the terminal verdict of THIS mint …
    try testing.expect(a.freshSupplyExhausted()); // … latched, but
    try testing.expectEqual(TopupOutcome.landed, arm_a.topup()); // the next pull lands and
    _ = try a.allocateBlock(); // the latch clears at the first landed mint.
    try testing.expect(!a.freshSupplyExhausted());
    // The single-flight latch: a top-up in flight answers not_asked.
    arm_b.window.topup_inflight.store(true, .release);
    var d2: Grants = .{};
    arm_b.window.drain(&d2);
    try testing.expectEqual(TopupOutcome.not_asked, arm_b.topup());
    arm_b.window.topup_inflight.store(false, .release);
    // The deferred class rides a refused drain at the holder.
    var j: data_bitmap.RamJournal = .{ .gpa = testing.allocator };
    defer j.deinit();
    bm.journal = j.sink();
    j.refuse_next = 1;
    const pop_before = bm.populationNow();
    try testing.expectEqual(TopupOutcome.deferred, arm_b.topup());
    try testing.expectEqual(pop_before, bm.populationNow()); // the bits were given back
    try testing.expectEqual(TopupOutcome.landed, arm_b.topup());
    try testing.expectEqual(classifyTopupAnswer(.refused), TopupOutcome.refused);
}

test "a_poisoned_window_declaration_is_refused_before_any_walk; a crashed incarnation's remainder is released at the re-hold and a live writer's declared window is adopted: deferred ≡ released + adopted + pending" {
    const a = try testAlloc(512);
    defer a.deinit();
    var i: u64 = 0;
    while (i < 8) : (i += 1) _ = try a.allocateBlock();
    const bm = try testBitmap(a);
    defer bm.deinit();
    var h: HolderGrants = .{ .ledger = try Ledger.init(testing.allocator, 16), .bitmap = bm };
    defer h.ledger.deinit();
    // The predecessor's RAM ledger granted 8..72 to writer 2 and 72..136 to
    // a writer that died with it; the successor's ledger is empty, so the
    // bitmap shows 128 SET bits nothing references and no grant names.
    try testing.expect(bm.setRun(8, 128) == 128);
    var referenced = [_]u64{ 0xFF, 0, 0, 0, 0, 0, 0, 0 };
    var granted = [_]u64{ 0, 0, 0, 0, 0, 0, 0, 0 };
    h.ledger.openRangesInto(&granted);
    var leaks: std.ArrayList(u64) = .empty;
    defer leaks.deinit(testing.allocator);
    const d = try bm.verifyBitmap(&referenced, &granted, &leaks);
    try testing.expectEqual(@as(u64, 128), d.leak);
    var dl = try DeferredLeaks.init(testing.allocator, 512);
    defer dl.deinit();
    dl.deferLeaks(leaks.items);
    try testing.expectEqual(@as(u64, 128), dl.pendingCount());
    try testing.expect(dl.closureHolds());
    // Not yet converged: a live peer has not declared.
    try testing.expectEqual(@as(u64, 0), dl.convergeDeferred(bm, false));
    // Writer 2's declaration (its window 8..72, minus 4 minted-unpublished
    // in flight at 8..12 that it also declares): screened, then adopted.
    const decl = [_]BlockGrant{ .{ .start = 8, .len = 4 }, .{ .start = 12, .len = 60 } };
    try testing.expect(screenWindowDecl(&decl, 512));
    const rej0 = counters.tripwireCount("data_alloc_bitmap_decls_rejected");
    stats.data_alloc_bitmap_decls_rejected.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    try testing.expect(!screenWindowDecl(&.{ .{ .start = 12, .len = 60 }, .{ .start = 8, .len = 4 } }, 512)); // not ascending
    try log.testing.expectAllFired();
    try testing.expectEqual(rej0 + 1, counters.tripwireCount("data_alloc_bitmap_decls_rejected"));
    const bad = [_][]const BlockGrant{
        &.{.{ .start = 500, .len = 20 }}, // past the end
        &.{.{ .start = std.math.maxInt(u64), .len = 2 }}, // wrap
        &.{ .{ .start = 8, .len = 10 }, .{ .start = 12, .len = 4 } }, // overlap
        &.{.{ .start = 8, .len = 0 }}, // empty
    };
    for (bad) |b| {
        stats.data_alloc_bitmap_decls_rejected.resetLogBudgetForTest();
        log.testing.expectErrorLogs(1);
        try testing.expect(!screenWindowDecl(b, 512));
        try log.testing.expectAllFired();
    }
    try testing.expectEqual(rej0 + 5, counters.tripwireCount("data_alloc_bitmap_decls_rejected"));
    try testing.expectEqual(@as(u64, 64), dl.adoptDeclared(&h.ledger, bm, 2, &decl));
    try testing.expect(h.ledger.isGranted(8) and h.ledger.isGranted(71) and !h.ledger.isGranted(72));
    try testing.expectEqual(@as(u64, 72), h.ledger.grantFrontier());
    try testing.expectEqual(@as(u64, 64), dl.pendingCount());
    try testing.expect(dl.closureHolds());
    // Every live peer declared: the remainder (the dead writer's 72..136) is released.
    try testing.expectEqual(@as(u64, 64), dl.convergeDeferred(bm, true));
    try testing.expect(!bm.isSet(72) and !bm.isSet(135) and bm.isSet(71));
    try testing.expectEqual(@as(u64, 0), dl.pendingCount());
    try testing.expect(dl.closureHolds());
    try testing.expectEqual(@as(u64, 128), dl.deferred);
    try testing.expectEqual(@as(u64, 64 + 64), dl.released + dl.adopted);
    // The successor's first carve lands above the adopted frontier (72),
    // never inside the dead grant's window below it … and the dead writer's
    // death revokes nothing (its grants were never adopted).
    const first = h.blockGrant(9, 0, 0).granted.grants.buf[0];
    try testing.expectEqual(@as(u64, 72), first.start);
    try testing.expectEqual(@as(u64, 0), h.revokeDeadWriter(3, 1));
    // The adopted writer's leave returns the adopted blocks.
    try testing.expectEqual(@as(u64, 64), h.ledger.releaseWriter(bm, 2));
}

test "held_block_ranges: the declaration is the unconsumed window plus the in-flight registrations, coalesced ascending" {
    const a = try testAlloc(128);
    defer a.deinit();
    var w: GrantWindow = .{};
    try testing.expect(w.install(.{ .start = 10, .len = 4 }));
    try testing.expect(w.install(.{ .start = 20, .len = 2 }));
    _ = w.mint(); // 10 minted
    var g1 = block_alloc.InflightAllocGuard.register(a, 10); // in flight
    defer g1.deinit();
    var g2 = block_alloc.InflightAllocGuard.register(a, 50);
    defer g2.deinit();
    var out: std.ArrayList(BlockGrant) = .empty;
    defer out.deinit(testing.allocator);
    try heldBlockRanges(&w, a, testing.allocator, &out);
    try testing.expectEqual(@as(usize, 3), out.items.len);
    try testing.expectEqual(BlockGrant{ .start = 10, .len = 4 }, out.items[0]); // 10 in flight + 11..14 window
    try testing.expectEqual(BlockGrant{ .start = 20, .len = 2 }, out.items[1]);
    try testing.expectEqual(BlockGrant{ .start = 50, .len = 1 }, out.items[2]);
    try testing.expect(screenWindowDecl(out.items, 128));
}
