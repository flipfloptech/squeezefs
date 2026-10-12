//! The data allocation bitmap (charter §5.5 "a data allocation bitmap (1
//! bit per block, A/B pages `KVDA`, 32 KiB/TiB, in extents of the holder's
//! grant on its HOME volume; mutations are `vol_tag`-prefixed deltas in the
//! holder's ring carrying the holder term, under one ordering lock, pages
//! written before barrier #1; the C6/C8 census is the oracle)";
//! `docs/porting/sym_data_planes.md` §2). PORTABLE: byte images stand in
//! for the device; the holder's ring is a `DeltaJournal` seam.
//!
//! The bitmap IS the allocator's `bits` (0002 row 237): `DataBitmap` owns
//! no second copy of the words — it owns the PAGE STATE over them (A/B
//! slot + generation + dirty per page), the O(1) population, and the ONE
//! ordering lock under which every bit moves TOGETHER with its delta
//! (`sym_data_planes.md` §2.4: "queue order IS RAM order … a CLEAR is never
//! journaled after a later re-grant's SET"). A carve drains the ring itself
//! and answers only once its SET is durable; a `finish_free` CLEAR is queued
//! and the single-flight drainer is kicked; a drain the ring refuses
//! (`ReserveExhausted`) leaves the deltas queued — the page write covers
//! the bits, the next kick or carve journals them (`deltas_deferred`).
//!
//! Deltas are RUNS (0002 row 238): kind 5, key `vol_tag ‖ start`, value
//! `{tag, len, term}`; the replay expands a run before its per-key LWW
//! fold, so the per-block law is unchanged while a floor grant costs one
//! record instead of 64.
//!
//! Pages ride the holder's checkpoint cycle BEFORE barrier #1 beside the
//! meta bitmap's own page write (§2.5): the record's tail passes every
//! delta journaled below the cycle's head, so the pages must already carry
//! them. The queue is NOT drained in the cycle ("no USER-class admission
//! ever decides the cycle's outcome": a queued SET only ever costs a leak,
//! a queued CLEAR reads a genuinely free block). Load (§2.6): per page the
//! newest VALID slot wins; both slots all-zero = fresh; a page with no
//! valid slot and a non-zero byte REFUSES ("an unreadable predecessor
//! region must never read as an all-free bitmap — this bitmap has no second
//! witness until fsck's census runs").

const std = @import("std");
const fuzzkit = @import("../../core/fuzzkit.zig");
const Atomic = std.atomic.Value;
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const checksum = @import("../../core/checksum.zig");
const counters = @import("../../core/counters.zig");
const log = @import("../../core/log.zig");
const derive = @import("../../core/derive.zig");
const knobs = @import("../../core/knobs.zig");
const jc = @import("../../meta/journal_core.zig");
const block_alloc = @import("../block_alloc.zig");
const records = @import("records.zig");

pub const BlockAllocator = block_alloc.BlockAllocator;

// ---------------------------------------------------------------------------
// §2.1–§2.2 Geometry and the `KVDA` page.
// ---------------------------------------------------------------------------

/// One page = one 4 KiB sector: the torn-write unit, so an A/B pair makes
/// a tear damage only the copy being replaced.
pub const page_len: usize = 4096;
/// `magic u32 ‖ page_index u32 ‖ generation u64 ‖ vol_tag u64 ‖ hash64 u64`:
/// the meta bitmap's 24-B `KVAB` header plus the volume word, so a page of
/// another volume's bitmap never reads as ours.
pub const page_hdr_len: usize = 32;
pub const page_data_len: usize = page_len - page_hdr_len;
/// 4064 × 8 = 32,512 bits per page.
pub const page_bits: u64 = page_data_len * 8;
/// 508 u64 words per page — page boundaries fall on word boundaries, so a
/// page serializes from the allocator's words with no shifting.
pub const words_per_page: usize = page_data_len / 8;
/// ASCII "KVDA" in disk order (0002 row 38); distinct from "KVAB" so a
/// misplaced meta page never verifies here.
pub const page_magic: u32 = std.mem.readInt(u32, "KVDA", .little);
const off_magic = 0;
const off_page_index = 4;
const off_generation = 8;
const off_vol_tag = 16;
const off_checksum = 24;
/// Fresh page state: the first write goes to slot A.
pub const slot_none: u8 = 2;

comptime {
    assert(words_per_page * 8 == page_data_len);
    assert(page_bits == @as(u64, words_per_page) * 64);
}

/// `ceil((cap / block) / 8)`: 32 KiB per TiB at 4 MiB blocks.
pub fn bitmapBytesFor(capacity_bytes: u64, block_size: u64) u64 {
    return derive.ceilDiv(capacity_bytes / block_size, 8);
}

pub fn pagesFor(blocks: u64) u64 {
    return derive.ceilDiv(blocks, page_bits);
}

/// Pages × 2 slots × 4 KiB.
pub fn regionLen(blocks: u64) u64 {
    return pagesFor(blocks) * 2 * page_len;
}

/// Slot A of page `i` at `i × 8 KiB`, slot B at `+ 4 KiB`.
pub fn pageSlotOffset(page: u64, slot: u8) u64 {
    return page * 2 * page_len + @as(u64, slot) * page_len;
}

pub const PageEncodeError = error{PayloadTooLarge};

/// `bits` (≤ 4064 B, zero-padded) under the 32-B header, checksummed with
/// the field zeroed (header-then-payload).
pub fn encodePage(page_index: u32, generation: u64, vol_tag: u64, bits: []const u8, out: *[page_len]u8) PageEncodeError!void {
    if (bits.len > page_data_len) return error.PayloadTooLarge;
    @memset(out, 0);
    std.mem.writeInt(u32, out[off_magic..][0..4], page_magic, .little);
    std.mem.writeInt(u32, out[off_page_index..][0..4], page_index, .little);
    std.mem.writeInt(u64, out[off_generation..][0..8], generation, .little);
    std.mem.writeInt(u64, out[off_vol_tag..][0..8], vol_tag, .little);
    @memcpy(out[page_hdr_len .. page_hdr_len + bits.len], bits);
    checksum.stamp(out, off_checksum);
}

pub const PageDecodeError = error{ Truncated, BadMagic, WrongIndex, WrongVolume, ChecksumMismatch };

pub const DecodedPage = struct { generation: u64, bits: *const [page_data_len]u8 };

/// length → magic → page_index → vol_tag → checksum; at LOAD every failure
/// means "this slot holds no valid page", never loud.
pub fn decodePage(buf: []const u8, page_index: u32, vol_tag: u64) PageDecodeError!DecodedPage {
    if (buf.len < page_len) return error.Truncated;
    const img = buf[0..page_len];
    if (std.mem.readInt(u32, img[off_magic..][0..4], .little) != page_magic) return error.BadMagic;
    if (std.mem.readInt(u32, img[off_page_index..][0..4], .little) != page_index) return error.WrongIndex;
    if (std.mem.readInt(u64, img[off_vol_tag..][0..8], .little) != vol_tag) return error.WrongVolume;
    if (!checksum.verify(img, off_checksum)) return error.ChecksumMismatch;
    return .{ .generation = std.mem.readInt(u64, img[off_generation..][0..8], .little), .bits = img[page_hdr_len..page_len] };
}

// ---------------------------------------------------------------------------
// §2.4 The run delta and the holder's ring.
// ---------------------------------------------------------------------------

pub const RunDelta = struct {
    set: bool,
    start: u64,
    len: u32,
    term: u64,

    pub fn encodeKey(self: RunDelta, vol_tag: u64) [records.DELTA_KEY_LEN]u8 {
        return records.deltaKey(vol_tag, self.start);
    }

    pub fn encodeValue(self: RunDelta) [records.DELTA_LEN]u8 {
        return records.encodeDelta(.{ .tag = if (self.set) records.TAG_SET else records.TAG_CLEAR, .len = self.len, .term = self.term });
    }
};

pub const JournalError = error{ ReserveExhausted, Io };

/// The holder's ring: ONE control entry per drain chunk, USER class (a verb
/// storm never eats the checkpoint reserve). `ReserveExhausted` is the
/// window-full park's class — the deltas stay queued.
pub const DeltaJournal = struct {
    ctx: *anyopaque,
    journal: *const fn (*anyopaque, vol_tag: u64, deltas: []const RunDelta) JournalError!void,
};

/// Deltas per control entry: the reference's `DELTAS_PER_ENTRY = 1024`
/// per-bit records; a run record is 16 + 16 B, so 1024 runs ≈ 48 KiB of
/// entry — well inside the 128 KiB whole-entry cap.
pub const DRAIN_CHUNK: usize = 1024;

/// A fixed ring (never grown under the lock, 0002 row 32): the drainer
/// PEEKS the head chunk and pops it only once journaled, so a refused entry
/// leaves everything in place and nothing behind it was journaled
/// meanwhile (the single drainer holds the latch).
pub const DeltaRing = struct {
    buf: []RunDelta,
    head: usize = 0,
    len: usize = 0,

    pub fn init(gpa: Allocator, capacity: usize) Allocator.Error!DeltaRing {
        assert(capacity > 0);
        return .{ .buf = try gpa.alloc(RunDelta, capacity) };
    }

    pub fn deinit(self: *DeltaRing, gpa: Allocator) void {
        gpa.free(self.buf);
        self.* = undefined;
    }

    pub fn isFull(self: *const DeltaRing) bool {
        return self.len == self.buf.len;
    }

    pub fn push(self: *DeltaRing, d: RunDelta) bool {
        if (self.isFull()) return false;
        self.buf[(self.head + self.len) % self.buf.len] = d;
        self.len += 1;
        return true;
    }

    /// Copy up to `out.len` head entries without popping.
    pub fn peek(self: *const DeltaRing, out: []RunDelta) usize {
        const n = @min(out.len, self.len);
        for (0..n) |i| out[i] = self.buf[(self.head + i) % self.buf.len];
        return n;
    }

    pub fn pop(self: *DeltaRing, n: usize) void {
        assert(n <= self.len);
        self.head = (self.head + n) % self.buf.len;
        self.len -= n;
    }
};

/// Ring capacity at the hold: `2 × (grant floor × writers) + the reclaim
/// batch` — two floor grants per writer in flight plus one reclaim batch of
/// CLEARs between two drains; never grown under the lock.
pub fn ringCapacity(grant_floor: u64, writers: u64, reclaim_batch: u64) usize {
    return @intCast(2 * grant_floor * @max(writers, 1) + reclaim_batch);
}

// ---------------------------------------------------------------------------
// Stats (names = the registry, §7.2).
// ---------------------------------------------------------------------------

pub const stats = struct {
    pub var data_alloc_bitmap_set_bits: counters.ShardedCounter = .{};
    pub var data_alloc_bitmap_clear_bits: counters.ShardedCounter = .{};
    pub var data_alloc_bitmap_deltas_journaled: counters.ShardedCounter = .{};
    /// Drains the ring refused: the deltas stayed queued (the pages cover
    /// the bits).
    pub var data_alloc_bitmap_deltas_deferred: counters.ShardedCounter = .{};
    /// A CLEAR the ring could not hold (leak-safe: the bit is clear in RAM
    /// and the page carries it; a stale SET in an older page costs a leak
    /// the census releases, never a loss).
    pub var data_alloc_bitmap_deltas_dropped: counters.ShardedCounter = .{};
    pub var data_alloc_bitmap_pages_written: counters.ShardedCounter = .{};
    pub var data_alloc_bitmap_page_generation_raises: counters.ShardedCounter = .{};
    pub var data_alloc_bitmap_replayed: counters.ShardedCounter = .{};
    /// The census LEAK half: SET ∧ ¬referenced ∧ ¬granted (released by the
    /// deferred-leak protocol, counted here).
    pub var data_alloc_bitmap_leak_candidates: counters.ShardedCounter = .{};
    /// Must-stay-0: referenced ∧ clear — a future carve would overwrite a
    /// live block (the S1 class, report-only).
    pub var data_alloc_bitmap_drift: counters.Tripwire = .init("data_alloc_bitmap_drift");
    /// Must-stay-0: the maintained population disagreed with the popcount.
    pub var data_alloc_population_drift: counters.Tripwire = .init("data_alloc_population_drift");
    /// A SET delta the ring could not hold at a carve — cannot happen: the
    /// carve drains before it carves.
    pub var data_alloc_bitmap_set_dropped: counters.Tripwire = .init("data_alloc_bitmap_set_dropped");
    /// Deltas a holder with no journal sink (the RAM posture) did not
    /// queue — the KVDA page sink is the next wave's (0002 row 292).
    pub var data_alloc_bitmap_deltas_unjournaled: counters.ShardedCounter = .{};
    /// Population scans the allocation funnel performed (0 by contract).
    pub var population_scans: counters.ShardedCounter = .{};
    /// Levels.
    pub var data_alloc_bitmap_population: counters.Gauge = .{};
    pub var data_alloc_bitmap_queued: counters.Gauge = .{};
};

// ---------------------------------------------------------------------------
// The bitmap over the allocator's words.
// ---------------------------------------------------------------------------

const PageState = struct {
    generation: Atomic(u64),
    /// 0 = A, 1 = B, `slot_none` = fresh.
    slot: Atomic(u8),
};

pub const Config = struct {
    vol_tag: u64,
    /// The holder term every delta carries.
    term: u64,
    /// Bits the pages cover (= capacity_bytes / block_size).
    blocks: u64,
    ring_capacity: usize,
};

pub const DataBitmap = struct {
    gpa: Allocator,
    alloc: *BlockAllocator,
    cfg: Config,
    pages: u64,
    page_states: []PageState,
    dirty: []Atomic(u64),
    /// O(1) maintained (`DAB:L578-586`); re-synced by the drift check.
    population: Atomic(u64) = .init(0),
    /// THE ordering lock: every bit mutation moves the bit and queues its
    /// delta inside it. A spin lock: the critical section is O(1) and the
    /// finish-free sink runs on paths that carry no `Io`.
    mutations: jc.SpinLock = .{},
    ring: DeltaRing,
    /// The single-drainer latch (a CAS; the drainer releases it and
    /// re-checks the queue).
    draining: Atomic(bool) = .init(false),
    journal: ?DeltaJournal = null,
    /// `SQUEEZEFS_TEST_HOLDER_GRANT_EXHAUST_N`: the next N drains answer the
    /// window-full class.
    test_exhaust_drains: Atomic(u32) = .init(0),

    pub fn init(gpa: Allocator, alloc: *BlockAllocator, cfg: Config) Allocator.Error!*DataBitmap {
        assert(cfg.blocks == alloc.capacityBlocks());
        const pages = pagesFor(cfg.blocks);
        const self = try gpa.create(DataBitmap);
        errdefer gpa.destroy(self);
        const states = try gpa.alloc(PageState, @intCast(pages));
        errdefer gpa.free(states);
        for (states) |*s| s.* = .{ .generation = .init(0), .slot = .init(slot_none) };
        const dirty = try gpa.alloc(Atomic(u64), @intCast((pages + 63) / 64));
        errdefer gpa.free(dirty);
        @memset(dirty, .init(0));
        var ring = try DeltaRing.init(gpa, cfg.ring_capacity);
        errdefer ring.deinit(gpa);
        self.* = .{ .gpa = gpa, .alloc = alloc, .cfg = cfg, .pages = pages, .page_states = states, .dirty = dirty, .ring = ring };
        self.resyncPopulation();
        return self;
    }

    pub fn deinit(self: *DataBitmap) void {
        self.ring.deinit(self.gpa);
        self.gpa.free(self.dirty);
        self.gpa.free(self.page_states);
        self.gpa.destroy(self);
    }

    /// The holder's arm reads its test seam ONCE: `SQUEEZEFS_TEST_HOLDER_
    /// GRANT_EXHAUST_N` answers the next N drains with the park class.
    pub fn armFromEnv(self: *DataBitmap, env: *const std.process.Environ.Map) void {
        self.test_exhaust_drains.store(knobs.intKnob("SQUEEZEFS_TEST_HOLDER_GRANT_EXHAUST_N", u32, env), .release);
    }

    pub fn volTag(self: *const DataBitmap) u64 {
        return self.cfg.vol_tag;
    }

    pub fn term(self: *const DataBitmap) u64 {
        return self.cfg.term;
    }

    pub fn blocks(self: *const DataBitmap) u64 {
        return self.cfg.blocks;
    }

    pub fn populationNow(self: *const DataBitmap) u64 {
        return self.population.load(.acquire);
    }

    /// The highest set bit + 1: the holder's frontier floor and virgin tail.
    pub fn highestSetPlusOne(self: *const DataBitmap) u64 {
        return self.alloc.highestSetPlusOne();
    }

    fn resyncPopulation(self: *DataBitmap) void {
        stats.population_scans.inc();
        const n = self.alloc.bitPopulation() -| self.alloc.trim_windowed.load(.acquire);
        self.population.store(n, .release);
        stats.data_alloc_bitmap_population.set(n);
    }

    fn markDirty(self: *DataBitmap, idx: u64) void {
        const page = idx / page_bits;
        _ = self.dirty[@intCast(page / 64)].fetchOr(@as(u64, 1) << @intCast(page % 64), .acq_rel);
    }

    pub fn hasDirtyPages(self: *const DataBitmap) bool {
        for (self.dirty) |*d| if (d.load(.acquire) != 0) return true;
        return false;
    }

    pub fn queuedDeltas(self: *DataBitmap) usize {
        self.mutations.lock();
        defer self.mutations.unlock();
        return self.ring.len;
    }

    // ---- the mutations (each under the ONE lock) ----

    /// Queue a delta under the held lock; a SET that does not fit is the
    /// tripwire (the carve drained first), a CLEAR that does not fit is
    /// dropped leak-safe. With NO journal bound (this wave's RAM holder,
    /// decision 0002 row 292) the delta has no consumer: the RAM word is
    /// the truth and nothing is queued — a ring nothing drains is not a
    /// defect when it fills.
    fn queueLocked(self: *DataBitmap, d: RunDelta) void {
        if (self.journal == null) {
            stats.data_alloc_bitmap_deltas_unjournaled.inc();
            return;
        }
        if (self.ring.push(d)) {
            stats.data_alloc_bitmap_queued.set(self.ring.len);
            return;
        }
        if (d.set) {
            stats.data_alloc_bitmap_set_dropped.fire("a carve's SET delta found the ring full after its drain");
        } else {
            stats.data_alloc_bitmap_deltas_dropped.inc();
        }
    }

    /// The finish-free sink's body: bit 1→0 and the CLEAR queued in ONE
    /// critical section; `false` ⇒ already clear (a double free).
    pub fn noteFinishFree(self: *DataBitmap, idx: u64) bool {
        assert(idx < self.cfg.blocks);
        self.mutations.lock();
        defer self.mutations.unlock();
        if (!self.alloc.holderClearBitRaw(idx)) return false;
        _ = self.population.fetchSub(1, .acq_rel);
        stats.data_alloc_bitmap_population.set(self.population.load(.monotonic));
        stats.data_alloc_bitmap_clear_bits.inc();
        self.markDirty(idx);
        self.queueLocked(.{ .set = false, .start = idx, .len = 1, .term = self.cfg.term });
        return true;
    }

    /// `block_alloc.FinishFreeSink` over this bitmap.
    pub fn finishFreeSink(self: *DataBitmap) block_alloc.FinishFreeSink {
        const S = struct {
            fn note(ctx: ?*anyopaque, _: u64, idx: u64) bool {
                const b: *DataBitmap = @ptrCast(@alignCast(ctx.?));
                return b.noteFinishFree(idx);
            }
        };
        return .{ .ctx = self, .noteFinishFree = &S.note };
    }

    pub const Run = struct { start: u64, len: u32 };

    /// The carve's SET (§3.2): the lowest clear run of ≤ `want` blocks at or
    /// above `frontier`, falling back to 0 ("a hole below the floor is
    /// genuinely free, since every granted bit is SET"); the bits SET and
    /// the run delta queued under the lock, BEFORE any reply. Null ⇒ no
    /// clear bit remains (`Full`).
    pub fn carveRun(self: *DataBitmap, frontier: u64, want: u64) ?Run {
        assert(want >= 1);
        const cap: u64 = @min(want, std.math.maxInt(u32));
        self.mutations.lock();
        defer self.mutations.unlock();
        const run = self.alloc.lowestClearRun(frontier, cap) orelse self.alloc.lowestClearRun(0, cap) orelse return null;
        var taken: u64 = 0;
        while (taken < run.len) : (taken += 1) {
            // Every bit of the run read clear under the lock and only this
            // lock sets bits on an armed allocator.
            const ok = self.alloc.holderTakeBit(run.start + taken);
            assert(ok);
        }
        _ = self.population.fetchAdd(run.len, .acq_rel);
        stats.data_alloc_bitmap_population.set(self.population.load(.monotonic));
        stats.data_alloc_bitmap_set_bits.add(run.len);
        self.markDirty(run.start);
        self.markDirty(run.start + run.len - 1);
        self.queueLocked(.{ .set = true, .start = run.start, .len = @intCast(run.len), .term = self.cfg.term });
        return .{ .start = run.start, .len = @intCast(run.len) };
    }

    /// A `ReturnBlocks` / adopt-release CLEAR of a granted, never-minted
    /// run: the bits 1→0 and the CLEAR queued under the lock. Returns the
    /// bits actually cleared.
    pub fn clearRun(self: *DataBitmap, start: u64, len: u32) u32 {
        assert(len >= 1 and start + len <= self.cfg.blocks);
        self.mutations.lock();
        defer self.mutations.unlock();
        var cleared: u32 = 0;
        var i: u64 = 0;
        while (i < len) : (i += 1) {
            if (self.alloc.holderReturnBit(start + i)) cleared += 1;
        }
        if (cleared == 0) return 0;
        _ = self.population.fetchSub(cleared, .acq_rel);
        stats.data_alloc_bitmap_population.set(self.population.load(.monotonic));
        stats.data_alloc_bitmap_clear_bits.add(cleared);
        self.markDirty(start);
        self.markDirty(start + len - 1);
        self.queueLocked(.{ .set = false, .start = start, .len = len, .term = self.cfg.term });
        return cleared;
    }

    /// A SET that journals (a replayed window's survivor applied at the
    /// re-hold, a seed's bits): bits set, the run queued.
    pub fn setRun(self: *DataBitmap, start: u64, len: u32) u32 {
        assert(len >= 1 and start + len <= self.cfg.blocks);
        self.mutations.lock();
        defer self.mutations.unlock();
        var set: u32 = 0;
        var i: u64 = 0;
        while (i < len) : (i += 1) {
            if (self.alloc.holderTakeBit(start + i)) set += 1;
        }
        if (set == 0) return 0;
        _ = self.population.fetchAdd(set, .acq_rel);
        stats.data_alloc_bitmap_population.set(self.population.load(.monotonic));
        stats.data_alloc_bitmap_set_bits.add(set);
        self.markDirty(start);
        self.markDirty(start + len - 1);
        self.queueLocked(.{ .set = true, .start = start, .len = len, .term = self.cfg.term });
        return set;
    }

    pub fn isSet(self: *const DataBitmap, idx: u64) bool {
        const words = self.alloc.bitmapWords();
        return words[@intCast(idx / 64)].load(.acquire) & (@as(u64, 1) << @intCast(idx % 64)) != 0;
    }

    // ---- the drainer (§2.4) ----

    pub const DrainOutcome = enum { drained, nothing, deferred, busy, no_journal };

    /// Journal the queue head-first in chunks; single-flight (a second
    /// caller returns `busy`); a refused entry leaves the deltas queued
    /// (`deferred`) — nothing behind it was journaled meanwhile.
    pub fn drain(self: *DataBitmap) DrainOutcome {
        const j = self.journal orelse return .no_journal;
        if (self.draining.cmpxchgStrong(false, true, .acq_rel, .acquire) != null) return .busy;
        defer self.draining.store(false, .release);
        var chunk: [DRAIN_CHUNK]RunDelta = undefined;
        var any = false;
        while (true) {
            self.mutations.lock();
            const n = self.ring.peek(&chunk);
            self.mutations.unlock();
            if (n == 0) return if (any) .drained else .nothing;
            if (self.test_exhaust_drains.load(.monotonic) > 0) {
                _ = self.test_exhaust_drains.fetchSub(1, .monotonic);
                stats.data_alloc_bitmap_deltas_deferred.inc();
                return .deferred;
            }
            j.journal(j.ctx, self.cfg.vol_tag, chunk[0..n]) catch |e| switch (e) {
                error.ReserveExhausted => {
                    stats.data_alloc_bitmap_deltas_deferred.inc();
                    return .deferred;
                },
                error.Io => {
                    stats.data_alloc_bitmap_deltas_deferred.inc();
                    return .deferred;
                },
            };
            self.mutations.lock();
            self.ring.pop(n);
            stats.data_alloc_bitmap_queued.set(self.ring.len);
            self.mutations.unlock();
            stats.data_alloc_bitmap_deltas_journaled.add(n);
            any = true;
        }
    }

    // ---- §2.5 pages before barrier #1 ----

    pub const PageWrite = struct { page: u32, offset: u64, target_slot: u8, generation: u64, image: [page_len]u8 };

    /// The planned write of every dirty page into the ALTERNATE slot at
    /// `gen = max(page.generation, generation − 1) + 1`; the dirty set is
    /// snapshot-and-cleared (a mutation racing this re-dirties its page and
    /// rides the next cycle), the state advanced at `commit`; `abort`
    /// re-dirties every page.
    pub const PageWriteSet = struct {
        gpa: Allocator,
        writes: []PageWrite,
        settled: bool = false,

        pub fn deinit(self: *PageWriteSet) void {
            assert(self.settled or self.writes.len == 0);
            self.gpa.free(self.writes);
            self.* = undefined;
        }

        pub fn commit(self: *PageWriteSet, bm: *DataBitmap) void {
            assert(!self.settled);
            for (self.writes) |*w| {
                const st = &bm.page_states[w.page];
                st.generation.store(w.generation, .release);
                st.slot.store(w.target_slot, .release);
            }
            stats.data_alloc_bitmap_pages_written.add(self.writes.len);
            self.settled = true;
        }

        pub fn abort(self: *PageWriteSet, bm: *DataBitmap) void {
            assert(!self.settled);
            for (self.writes) |*w| _ = bm.dirty[w.page / 64].fetchOr(@as(u64, 1) << @intCast(w.page % 64), .acq_rel);
            self.settled = true;
        }

        /// Apply to a byte image of the region (tests, the offline copy).
        pub fn applyToRegion(self: *PageWriteSet, bm: *DataBitmap, region: []u8) void {
            for (self.writes) |*w| @memcpy(region[@intCast(w.offset)..][0..page_len], &w.image);
            self.commit(bm);
        }
    };

    pub fn takeDirtyPlan(self: *DataBitmap, generation: u64) Allocator.Error!PageWriteSet {
        const page_buf = try self.gpa.alloc(u32, @intCast(self.pages));
        defer self.gpa.free(page_buf);
        var n_pages: usize = 0;
        for (self.dirty, 0..) |*word, wi| {
            var bits = word.swap(0, .acq_rel);
            while (bits != 0) : (bits &= bits - 1) {
                const page = @as(u64, wi) * 64 + @ctz(bits);
                if (page < self.pages) {
                    page_buf[n_pages] = @intCast(page);
                    n_pages += 1;
                }
            }
        }
        const pages = page_buf[0..n_pages];
        if (pages.len == 0) return .{ .gpa = self.gpa, .writes = &.{}, .settled = true };
        const writes = self.gpa.alloc(PageWrite, pages.len) catch |err| {
            for (pages) |p| _ = self.dirty[p / 64].fetchOr(@as(u64, 1) << @intCast(p % 64), .acq_rel);
            return err;
        };
        const words = self.alloc.bitmapWords();
        for (pages, writes) |page, *w| {
            const st = &self.page_states[page];
            const cur_gen = st.generation.load(.acquire);
            const cur_slot = st.slot.load(.acquire);
            var page_gen = generation;
            if (cur_slot != slot_none and generation <= cur_gen) {
                page_gen = cur_gen + 1;
                stats.data_alloc_bitmap_page_generation_raises.inc();
            }
            const target: u8 = if (cur_slot == slot_none) 0 else 1 - cur_slot;
            w.* = .{ .page = page, .offset = pageSlotOffset(page, target), .target_slot = target, .generation = page_gen, .image = undefined };
            var bits: [page_data_len]u8 = @splat(0);
            const start = @as(usize, page) * words_per_page;
            const end = @min(start + words_per_page, words.len);
            for (words[start..end], 0..) |*word, i| std.mem.writeInt(u64, bits[i * 8 ..][0..8], word.load(.acquire), .little);
            encodePage(page, page_gen, self.cfg.vol_tag, &bits, &w.image) catch unreachable;
        }
        return .{ .gpa = self.gpa, .writes = writes };
    }

    /// The checkpoint step for holdings HOMED ON THIS backend only: another
    /// volume's checkpoint touches neither the pages nor the dirty bits.
    pub fn pagesForCheckpoint(self: *DataBitmap, holder_home_vol: u16, checkpointing_vol: u16, generation: u64) Allocator.Error!?PageWriteSet {
        if (holder_home_vol != checkpointing_vol) return null;
        return try self.takeDirtyPlan(generation);
    }

    /// Once per cycle: the maintained population against the popcount
    /// (minus the trim windows' transient bits); a disagreement is the
    /// tripwire and a re-sync.
    pub fn populationDriftCheck(self: *DataBitmap) bool {
        const maintained = self.population.load(.acquire);
        const counted = self.alloc.bitPopulation() -| self.alloc.trim_windowed.load(.acquire);
        if (maintained == counted) return true;
        stats.data_alloc_population_drift.fire("the maintained population disagreed with the popcount");
        log.err("data bitmap vol {x}: population drift — maintained {d}, counted {d}; re-synced", .{ self.cfg.vol_tag, maintained, counted });
        self.population.store(counted, .release);
        stats.data_alloc_bitmap_population.set(counted);
        return false;
    }

    /// Newest page generation on disk (the mount's checkpoint seq floor).
    pub fn resumeGeneration(self: *const DataBitmap) u64 {
        var max: u64 = 0;
        for (self.page_states) |*s| max = @max(max, s.generation.load(.acquire));
        return max;
    }

    /// A successor's copy source: every page in slot A at one generation,
    /// slot B zeroed. `out.len == regionLen(blocks)`.
    pub fn regionImage(self: *const DataBitmap, generation: u64, out: []u8) void {
        assert(out.len == regionLen(self.cfg.blocks));
        @memset(out, 0);
        const words = self.alloc.bitmapWords();
        var page: u64 = 0;
        while (page < self.pages) : (page += 1) {
            var bits: [page_data_len]u8 = @splat(0);
            const start: usize = @intCast(page * words_per_page);
            const end = @min(start + words_per_page, words.len);
            for (words[start..end], 0..) |*word, i| std.mem.writeInt(u64, bits[i * 8 ..][0..8], word.load(.acquire), .little);
            encodePage(@intCast(page), generation, self.cfg.vol_tag, &bits, out[@intCast(pageSlotOffset(page, 0))..][0..page_len]) catch unreachable;
        }
    }

    // ---- §2.6 load and replay ----

    pub const LoadError = error{ TornPage, OutOfMemory };

    pub const Loaded = struct { pages_valid: u64, bits_set: u64 };

    /// Per page the newest VALID slot wins; both slots all-zero = fresh; a
    /// page with NO valid slot and a non-zero byte REFUSES. A short image
    /// zero-extends. Every set bit of the image that is clear in RAM is SET
    /// (no delta — the pages are the durable truth being loaded).
    pub fn loadRegionImage(self: *DataBitmap, region: []const u8) LoadError!Loaded {
        var out: Loaded = .{ .pages_valid = 0, .bits_set = 0 };
        var page: u64 = 0;
        while (page < self.pages) : (page += 1) {
            var newest: ?struct { gen: u64, slot: u8, bits: *const [page_data_len]u8 } = null;
            var any_nonzero = false;
            var slot: u8 = 0;
            while (slot < 2) : (slot += 1) {
                const off = pageSlotOffset(page, slot);
                if (off + page_len > region.len) continue;
                const img = region[@intCast(off)..][0..page_len];
                if (!std.mem.allEqual(u8, img, 0)) any_nonzero = true;
                const decoded = decodePage(img, @intCast(page), self.cfg.vol_tag) catch continue;
                if (newest == null or decoded.generation > newest.?.gen) newest = .{ .gen = decoded.generation, .slot = slot, .bits = decoded.bits };
            }
            if (newest) |n| {
                out.pages_valid += 1;
                const first = page * page_bits;
                const last = @min(first + page_bits, self.cfg.blocks);
                var idx = first;
                while (idx < last) : (idx += 1) {
                    const o: usize = @intCast(idx - first);
                    if (n.bits[o / 8] & (@as(u8, 1) << @intCast(o % 8)) != 0) {
                        if (self.alloc.holderTakeBit(idx)) out.bits_set += 1;
                    }
                }
                self.page_states[@intCast(page)] = .{ .generation = .init(n.gen), .slot = .init(n.slot) };
            } else if (any_nonzero) {
                log.err("data bitmap vol {x}: page {d} has no valid slot but non-zero bytes — bogus refs, a torn pair or an unreadable predecessor region; refusing to read it as all-free (fsck's census is the remedy)", .{ self.cfg.vol_tag, page });
                return error.TornPage;
            }
        }
        self.resyncPopulation();
        return out;
    }

    pub const ReplayRecord = struct { seq: u64, key: []const u8, value: []const u8 };

    /// Over the ring's window: only kind-5 records of THIS vol_tag and THIS
    /// `term` (the one being recovered); runs expanded to per-block pairs,
    /// per-key LWW by seq FIRST, then the survivors applied through the
    /// holder's own set/clear (journaled again under the recovering term —
    /// a re-hold's deltas). Returns bits changed.
    pub fn replay(self: *DataBitmap, replay_records: []const ReplayRecord, recovered_term: u64) Allocator.Error!u64 {
        const Folded = struct { seq: u64, set: bool };
        var folded: std.AutoHashMapUnmanaged(u64, Folded) = .empty;
        defer folded.deinit(self.gpa);
        for (replay_records) |rec| {
            const k = records.decodeDeltaKey(rec.key) catch continue;
            if (k.vol_tag != self.cfg.vol_tag) continue;
            const d = records.decodeDelta(rec.value) catch continue;
            if (d.term != recovered_term) continue;
            var i: u64 = 0;
            while (i < d.len) : (i += 1) {
                const idx = k.start + i;
                if (idx >= self.cfg.blocks) break;
                const gop = try folded.getOrPut(self.gpa, idx);
                if (!gop.found_existing or gop.value_ptr.seq <= rec.seq) gop.value_ptr.* = .{ .seq = rec.seq, .set = d.isSet() };
            }
        }
        var changed: u64 = 0;
        var it = folded.iterator();
        while (it.next()) |kv| {
            const idx = kv.key_ptr.*;
            if (kv.value_ptr.set) {
                if (self.setRun(idx, 1) == 1) changed += 1;
            } else {
                if (self.isSet(idx) and self.clearRun(idx, 1) == 1) changed += 1;
            }
        }
        stats.data_alloc_bitmap_replayed.add(changed);
        return changed;
    }

    // ---- §2.7 the census oracle ----

    pub const Drift = struct {
        /// `referenced ∧ clear` — must-stay-0 (a loss).
        loss: u64,
        /// `set ∧ ¬referenced ∧ ¬granted` — released by the leak protocol.
        leak: u64,
        pub fn clean(self: Drift) bool {
            return self.loss == 0 and self.leak == 0;
        }
    };

    /// `referenced` and `granted` are bitsets over the volume (`ceil(blocks
    /// / 64)` words; `granted` = the ledger's open ranges ∪ the in-flight
    /// registry — the live-owner shield). LOSS fires the tripwire (report-
    /// only); LEAK is counted. `leaks` (optional) receives the leak indices.
    pub fn verifyBitmap(self: *const DataBitmap, referenced: []const u64, granted: []const u64, leaks: ?*std.ArrayList(u64)) Allocator.Error!Drift {
        const words = self.alloc.bitmapWords();
        assert(referenced.len == words.len and granted.len == words.len);
        var out: Drift = .{ .loss = 0, .leak = 0 };
        for (words, 0..) |*w, wi| {
            const set = w.load(.acquire);
            const refd = referenced[wi];
            const gr = granted[wi];
            const loss_bits = refd & ~set;
            var leak_bits = set & ~refd & ~gr;
            if (wi == words.len - 1 and self.cfg.blocks % 64 != 0) leak_bits &= (@as(u64, 1) << @intCast(self.cfg.blocks % 64)) - 1;
            out.loss += @popCount(loss_bits);
            out.leak += @popCount(leak_bits);
            if (leaks) |l| {
                var bits = leak_bits;
                while (bits != 0) : (bits &= bits - 1) try l.append(self.gpa, @as(u64, wi) * 64 + @ctz(bits));
            }
        }
        if (out.loss != 0) {
            stats.data_alloc_bitmap_drift.fire("a referenced block reads CLEAR in the holder's bitmap (the LOSS direction)");
            log.err("data bitmap vol {x}: {d} referenced block(s) read CLEAR — a future carve would overwrite live data; remedy: fsck C6 (report-only here)", .{ self.cfg.vol_tag, out.loss });
        }
        stats.data_alloc_bitmap_leak_candidates.add(out.leak);
        return out;
    }
};

// ---------------------------------------------------------------------------
// §2.3 Placement — extents of the holder's grant on its HOME volume.
// ---------------------------------------------------------------------------

/// One claimed extent (bytes on the home volume's heap).
pub const Extent = struct { start: u64, len: u64 };

/// The extent-grant POOL the holder claims its pages from (the appender's
/// grant when the sym agent's `extent_grant.zig` binds it; a RAM pool in
/// tests): `claim` hands out one node-sized extent, `release` returns an
/// unpublished claim.
pub const ExtentPool = struct {
    ctx: *anyopaque,
    vtable: *const VTable,
    pub const VTable = struct {
        claim: *const fn (*anyopaque) error{GrantExhausted}!Extent,
        release: *const fn (*anyopaque, Extent) void,
        node_size: *const fn (*anyopaque) u64,
    };
};

/// The home volume's device face for the pages (RAM in tests).
pub const RegionIo = struct {
    ctx: *anyopaque,
    vtable: *const VTable,
    pub const VTable = struct {
        write: *const fn (*anyopaque, off: u64, bytes: []const u8) error{Io}!void,
        read: *const fn (*anyopaque, off: u64, out: []u8) error{Io}!void,
        barrier: *const fn (*anyopaque) error{Io}!void,
    };
};

pub const HoldError = error{ GrantExhausted, Io, OutOfMemory, TooManyRefs };

/// `extents_needed = max(1, ceil(region_len / node_size))` claimed from
/// the pool; the image written across the claims in claim order and
/// barriered; a failed write releases every claim. The published refs are
/// the claims coalesced into runs on `home_vol`.
pub fn holdPages(gpa: Allocator, pool: ExtentPool, io: RegionIo, home_vol: u16, image: []const u8, out_refs: *std.ArrayList(records.BitmapRef)) HoldError!void {
    const node = pool.vtable.node_size(pool.ctx);
    assert(node >= page_len and node % page_len == 0);
    const needed: usize = @intCast(@max(1, derive.ceilDiv(image.len, node)));
    var claims: std.ArrayList(Extent) = .empty;
    defer claims.deinit(gpa);
    errdefer for (claims.items) |c| pool.vtable.release(pool.ctx, c);
    for (0..needed) |_| try claims.append(gpa, try pool.vtable.claim(pool.ctx));
    var off: usize = 0;
    for (claims.items) |c| {
        if (off >= image.len) break;
        const n = @min(image.len - off, c.len);
        try io.vtable.write(io.ctx, c.start, image[off .. off + n]);
        off += n;
    }
    try io.vtable.barrier(io.ctx);
    // Coalesce adjacent claims into runs.
    for (claims.items) |c| {
        if (out_refs.items.len > 0) {
            const last = &out_refs.items[out_refs.items.len - 1];
            if (last.vol == home_vol and last.start + last.len == c.start) {
                last.len += c.len;
                continue;
            }
        }
        if (out_refs.items.len >= records.BITMAP_REFS_MAX) return error.TooManyRefs;
        try out_refs.append(gpa, .{ .vol = home_vol, .start = c.start, .len = c.len });
    }
}

/// `screen_bitmap_refs`: every ref names a volume the set has, lies inside
/// that volume's heap, starts a whole number of nodes from the heap start,
/// is a whole number of nodes long, non-empty, no overflow; `Σ len ≥
/// region_len(blocks)` and `≤ ceil(region / node) × node`.
pub const HeapGeometry = struct { vol: u16, heap_start: u64, heap_len: u64, node_size: u64 };

pub fn screenBitmapRefs(refs: []const records.BitmapRef, blocks: u64, heaps: []const HeapGeometry) bool {
    if (refs.len == 0) return false;
    var total: u64 = 0;
    var node: u64 = 0;
    for (refs) |r| {
        const h = for (heaps) |h| {
            if (h.vol == r.vol) break h;
        } else return false;
        if (r.len == 0) return false;
        const end = std.math.add(u64, r.start, r.len) catch return false;
        if (r.start < h.heap_start or end > h.heap_start + h.heap_len) return false;
        if ((r.start - h.heap_start) % h.node_size != 0 or r.len % h.node_size != 0) return false;
        node = h.node_size;
        total = std.math.add(u64, total, r.len) catch return false;
    }
    const region = regionLen(blocks);
    return total >= region and total <= derive.ceilDiv(region, node) * node;
}

/// Read the predecessor's pages through screened refs into one image
/// (`regionLen(blocks)` bytes, zero-extended); nothing proportional is
/// allocated before the screen passed (the caller's duty).
pub fn readRegionImage(gpa: Allocator, io: RegionIo, refs: []const records.BitmapRef, blocks: u64) HoldError![]u8 {
    const region = regionLen(blocks);
    const image = try gpa.alloc(u8, @intCast(region));
    errdefer gpa.free(image);
    @memset(image, 0);
    var off: usize = 0;
    for (refs) |r| {
        if (off >= image.len) break;
        const n = @min(image.len - off, r.len);
        try io.vtable.read(io.ctx, r.start, image[off .. off + n]);
        off += n;
    }
    return image;
}

/// A RAM pool + RAM region for the portable contracts.
pub const RamHome = struct {
    gpa: Allocator,
    node_size: u64,
    bytes: []u8,
    next_free: u64 = 0,
    claimed: u32 = 0,
    released: u32 = 0,
    fail_writes: bool = false,

    pub fn init(gpa: Allocator, node_size: u64, nodes: u64) Allocator.Error!RamHome {
        const bytes = try gpa.alloc(u8, @intCast(node_size * nodes));
        @memset(bytes, 0);
        return .{ .gpa = gpa, .node_size = node_size, .bytes = bytes };
    }

    pub fn deinit(self: *RamHome) void {
        self.gpa.free(self.bytes);
    }

    fn claim(ctx: *anyopaque) error{GrantExhausted}!Extent {
        const h: *RamHome = @ptrCast(@alignCast(ctx));
        if (h.next_free + h.node_size > h.bytes.len) return error.GrantExhausted;
        const e: Extent = .{ .start = h.next_free, .len = h.node_size };
        h.next_free += h.node_size;
        h.claimed += 1;
        return e;
    }
    fn release(ctx: *anyopaque, _: Extent) void {
        const h: *RamHome = @ptrCast(@alignCast(ctx));
        h.released += 1;
    }
    fn nodeSize(ctx: *anyopaque) u64 {
        const h: *RamHome = @ptrCast(@alignCast(ctx));
        return h.node_size;
    }
    fn write(ctx: *anyopaque, off: u64, bytes: []const u8) error{Io}!void {
        const h: *RamHome = @ptrCast(@alignCast(ctx));
        if (h.fail_writes) return error.Io;
        @memcpy(h.bytes[@intCast(off)..][0..bytes.len], bytes);
    }
    fn read(ctx: *anyopaque, off: u64, out: []u8) error{Io}!void {
        const h: *RamHome = @ptrCast(@alignCast(ctx));
        if (off + out.len > h.bytes.len) return error.Io;
        @memcpy(out, h.bytes[@intCast(off)..][0..out.len]);
    }
    fn barrier(_: *anyopaque) error{Io}!void {}

    const pool_vtable: ExtentPool.VTable = .{ .claim = &claim, .release = &release, .node_size = &nodeSize };
    const io_vtable: RegionIo.VTable = .{ .write = &write, .read = &read, .barrier = &barrier };

    pub fn pool(self: *RamHome) ExtentPool {
        return .{ .ctx = self, .vtable = &pool_vtable };
    }
    pub fn io(self: *RamHome) RegionIo {
        return .{ .ctx = self, .vtable = &io_vtable };
    }
};

/// A RAM ring for the portable contracts: records in journal order, a
/// refusal budget for the park class.
pub const RamJournal = struct {
    gpa: Allocator,
    entries: std.ArrayList(Entry) = .empty,
    /// The next N `journal` calls answer `ReserveExhausted`.
    refuse_next: u32 = 0,
    seq: u64 = 1,

    pub const Entry = struct { seq: u64, vol_tag: u64, delta: RunDelta };

    pub fn deinit(self: *RamJournal) void {
        self.entries.deinit(self.gpa);
    }

    fn journal(ctx: *anyopaque, vol_tag: u64, deltas: []const RunDelta) JournalError!void {
        const j: *RamJournal = @ptrCast(@alignCast(ctx));
        if (j.refuse_next > 0) {
            j.refuse_next -= 1;
            return error.ReserveExhausted;
        }
        for (deltas) |d| {
            j.entries.append(j.gpa, .{ .seq = j.seq, .vol_tag = vol_tag, .delta = d }) catch return error.Io;
            j.seq += 1;
        }
    }

    pub fn sink(self: *RamJournal) DeltaJournal {
        return .{ .ctx = self, .journal = &journal };
    }

    /// The ring's window as replay records (encoded; caller frees via `freeReplay`).
    pub fn replayRecords(self: *const RamJournal, gpa: Allocator) Allocator.Error![]DataBitmap.ReplayRecord {
        const out = try gpa.alloc(DataBitmap.ReplayRecord, self.entries.items.len);
        errdefer gpa.free(out);
        var i: usize = 0;
        errdefer for (out[0..i]) |r| {
            gpa.free(r.key);
            gpa.free(r.value);
        };
        while (i < out.len) : (i += 1) {
            const e = self.entries.items[i];
            const k = try gpa.dupe(u8, &e.delta.encodeKey(e.vol_tag));
            errdefer gpa.free(k);
            const v = try gpa.dupe(u8, &e.delta.encodeValue());
            out[i] = .{ .seq = e.seq, .key = k, .value = v };
        }
        return out;
    }

    pub fn freeReplay(gpa: Allocator, recs: []DataBitmap.ReplayRecord) void {
        for (recs) |r| {
            gpa.free(r.key);
            gpa.free(r.value);
        }
        gpa.free(recs);
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

fn testAlloc(blocks: u64) !*BlockAllocator {
    return BlockAllocator.init(testing.allocator, .{ .vol_tag = 0xDA7A, .vol = 0, .block_size = 4096, .capacity_bytes = blocks * 4096 });
}

fn testBitmap(alloc: *BlockAllocator, term: u64) !*DataBitmap {
    return DataBitmap.init(testing.allocator, alloc, .{ .vol_tag = 0xDA7A, .term = term, .blocks = alloc.capacityBlocks(), .ring_capacity = ringCapacity(64, 2, 64) });
}

test "the_data_bitmap_is_thirty_two_kib_per_tib; the page geometry ties (32-B header, 32,512 bits, 508 words, A/B at 8 KiB)" {
    try testing.expectEqual(@as(u64, 32 * derive.KiB), bitmapBytesFor(derive.TiB, 4 * derive.MiB));
    try testing.expectEqual(@as(usize, 32), page_hdr_len);
    try testing.expectEqual(@as(u64, 32_512), page_bits);
    try testing.expectEqual(@as(usize, 508), words_per_page);
    try testing.expectEqual(@as(u64, 1), pagesFor(32_512));
    try testing.expectEqual(@as(u64, 2), pagesFor(32_513));
    try testing.expectEqual(@as(u64, 2 * 2 * 4096), regionLen(32_513));
    try testing.expectEqual(@as(u64, 8192), pageSlotOffset(1, 0));
    try testing.expectEqual(@as(u64, 8192 + 4096), pageSlotOffset(1, 1));
    try testing.expectEqualSlices(u8, "KVDA", std.mem.asBytes(&page_magic));
    try testing.expectEqual(ringCapacity(64, 2, 64), @as(usize, 2 * 64 * 2 + 64));
}

test "page_round_trips_and_refuses_the_wrong_volume_and_index; every header word is where the spec puts it" {
    var bits: [page_data_len]u8 = @splat(0);
    bits[0] = 0b101;
    bits[page_data_len - 1] = 0x80;
    var img: [page_len]u8 = undefined;
    try encodePage(3, 77, 0xBEEF, &bits, &img);
    try testing.expectEqualSlices(u8, "KVDA", img[0..4]);
    try testing.expectEqual(@as(u32, 3), std.mem.readInt(u32, img[4..8], .little));
    try testing.expectEqual(@as(u64, 77), std.mem.readInt(u64, img[8..16], .little));
    try testing.expectEqual(@as(u64, 0xBEEF), std.mem.readInt(u64, img[16..24], .little));
    const d = try decodePage(&img, 3, 0xBEEF);
    try testing.expectEqual(@as(u64, 77), d.generation);
    try testing.expectEqualSlices(u8, &bits, d.bits);
    try testing.expectError(error.WrongIndex, decodePage(&img, 4, 0xBEEF));
    try testing.expectError(error.WrongVolume, decodePage(&img, 3, 0xBEEE));
    try testing.expectError(error.Truncated, decodePage(img[0..4095], 3, 0xBEEF));
    var bad = img;
    bad[0] = 'X';
    try testing.expectError(error.BadMagic, decodePage(&bad, 3, 0xBEEF));
    var flipped = img;
    flipped[100] ^= 1;
    try testing.expectError(error.ChecksumMismatch, decodePage(&flipped, 3, 0xBEEF));
    // A meta `KVAB` page never verifies here.
    var kvab = img;
    @memcpy(kvab[0..4], "KVAB");
    try testing.expectError(error.BadMagic, decodePage(&kvab, 3, 0xBEEF));
    var big: [page_data_len + 1]u8 = @splat(0);
    try testing.expectError(error.PayloadTooLarge, encodePage(0, 0, 0, &big, &img));
    // Zero-padded payload.
    try encodePage(0, 1, 2, &.{0xFF}, &img);
    try testing.expectEqual(@as(u8, 0xFF), img[32]);
    try testing.expectEqual(@as(u8, 0), img[33]);
}

test "the ring: push to capacity, peek without pop, pop after the journal; never grown" {
    var r = try DeltaRing.init(testing.allocator, 3);
    defer r.deinit(testing.allocator);
    try testing.expect(r.push(.{ .set = true, .start = 1, .len = 1, .term = 1 }));
    try testing.expect(r.push(.{ .set = false, .start = 2, .len = 1, .term = 1 }));
    try testing.expect(r.push(.{ .set = true, .start = 3, .len = 1, .term = 1 }));
    try testing.expect(!r.push(.{ .set = true, .start = 4, .len = 1, .term = 1 }));
    var out: [2]RunDelta = undefined;
    try testing.expectEqual(@as(usize, 2), r.peek(&out));
    try testing.expectEqual(@as(u64, 1), out[0].start);
    try testing.expectEqual(@as(usize, 3), r.len);
    r.pop(2);
    try testing.expect(r.push(.{ .set = true, .start = 5, .len = 1, .term = 1 }));
    try testing.expectEqual(@as(usize, 2), r.peek(&out));
    try testing.expectEqual(@as(u64, 3), out[0].start);
    try testing.expectEqual(@as(u64, 5), out[1].start);
}

test "deltas_fold_lww_and_the_run_carve_skips_set_bits: a carve SETs under the lock and journals ONE run; a finish-free CLEAR is one record; the replay folds by seq and ignores another term" {
    const a = try testAlloc(256);
    defer a.deinit();
    var i: u64 = 0;
    while (i < 10) : (i += 1) _ = try a.allocateBlock();
    const bm = try testBitmap(a, 3);
    defer bm.deinit();
    var j: RamJournal = .{ .gpa = testing.allocator };
    defer j.deinit();
    bm.journal = j.sink();
    a.finish_free_sink = bm.finishFreeSink();
    try testing.expectEqual(@as(u64, 10), bm.populationNow());
    // The carve at the frontier: 64 blocks from 10, one record.
    const run = bm.carveRun(bm.highestSetPlusOne(), 64).?;
    try testing.expectEqual(@as(u64, 10), run.start);
    try testing.expectEqual(@as(u32, 64), run.len);
    try testing.expectEqual(@as(u64, 74), bm.populationNow());
    try testing.expectEqual(@as(usize, 1), bm.queuedDeltas());
    try testing.expectEqual(DataBitmap.DrainOutcome.drained, bm.drain());
    try testing.expectEqual(@as(usize, 1), j.entries.items.len);
    try testing.expect(j.entries.items[0].delta.set and j.entries.items[0].delta.len == 64);
    // A terminal free of block 4 CLEARS the bit through the sink and queues len = 1.
    try testing.expectEqual(block_alloc.BeginFree.terminal, try a.freeBlock(4));
    try testing.expect(!bm.isSet(4));
    try testing.expectEqual(@as(u64, 73), bm.populationNow());
    try testing.expectEqual(DataBitmap.DrainOutcome.drained, bm.drain());
    try testing.expectEqual(@as(usize, 2), j.entries.items.len);
    try testing.expect(!j.entries.items[1].delta.set and j.entries.items[1].delta.start == 4 and j.entries.items[1].delta.len == 1);
    // A later carve skips the set bits: the next run starts at 74 (the
    // frontier), and the hole at 4 is found only when the tail is gone.
    const run2 = bm.carveRun(74, 8).?;
    try testing.expectEqual(@as(u64, 74), run2.start);
    _ = bm.drain();
    const hole = bm.carveRun(1000, 8).?; // above the cap ⇒ fallback to 0 ⇒ the hole at 4
    try testing.expectEqual(@as(u64, 4), hole.start);
    try testing.expectEqual(@as(u32, 1), hole.len);
    _ = bm.drain();
    // Replay onto a fresh allocator: CLEAR(4, seq 2) then SET(4, seq 4) fold to SET.
    const b = try testAlloc(256);
    defer b.deinit();
    const bm2 = try testBitmap(b, 3);
    defer bm2.deinit();
    const recs = try j.replayRecords(testing.allocator);
    defer RamJournal.freeReplay(testing.allocator, recs);
    const changed = try bm2.replay(recs, 3);
    try testing.expectEqual(@as(u64, 64 + 8 + 1), changed);
    try testing.expect(bm2.isSet(4));
    try testing.expect(bm2.isSet(10) and bm2.isSet(73) and bm2.isSet(81));
    try testing.expect(!bm2.isSet(82));
    // Another term's deltas never fold in.
    const c = try testAlloc(256);
    defer c.deinit();
    const bm3 = try testBitmap(c, 4);
    defer bm3.deinit();
    try testing.expectEqual(@as(u64, 0), try bm3.replay(recs, 4));
}

test "the single drainer: a refused entry leaves the deltas queued in order (deferred); the next drain lands them; a second drainer is busy; the test seam answers the park class" {
    const a = try testAlloc(128);
    defer a.deinit();
    const bm = try testBitmap(a, 1);
    defer bm.deinit();
    var j: RamJournal = .{ .gpa = testing.allocator };
    defer j.deinit();
    bm.journal = j.sink();
    _ = bm.carveRun(0, 4).?;
    _ = bm.clearRun(0, 2);
    const deferred0 = stats.data_alloc_bitmap_deltas_deferred.load();
    j.refuse_next = 1;
    try testing.expectEqual(DataBitmap.DrainOutcome.deferred, bm.drain());
    try testing.expectEqual(deferred0 + 1, stats.data_alloc_bitmap_deltas_deferred.load());
    try testing.expectEqual(@as(usize, 2), bm.queuedDeltas());
    try testing.expectEqual(DataBitmap.DrainOutcome.drained, bm.drain());
    try testing.expectEqual(@as(usize, 2), j.entries.items.len);
    try testing.expect(j.entries.items[0].delta.set and !j.entries.items[1].delta.set);
    try testing.expectEqual(DataBitmap.DrainOutcome.nothing, bm.drain());
    bm.draining.store(true, .release);
    try testing.expectEqual(DataBitmap.DrainOutcome.busy, bm.drain());
    bm.draining.store(false, .release);
    var env: std.process.Environ.Map = .init(testing.allocator);
    defer env.deinit();
    bm.armFromEnv(&env);
    try testing.expectEqual(@as(u32, 0), bm.test_exhaust_drains.load(.monotonic));
    try env.put("SQUEEZEFS_TEST_HOLDER_GRANT_EXHAUST_N", "1");
    bm.armFromEnv(&env);
    _ = bm.carveRun(10, 1).?;
    try testing.expectEqual(DataBitmap.DrainOutcome.deferred, bm.drain());
    try testing.expectEqual(DataBitmap.DrainOutcome.drained, bm.drain());
    bm.journal = null;
    try testing.expectEqual(DataBitmap.DrainOutcome.no_journal, bm.drain());
}

test "a full ring drops a CLEAR leak-safe and counts it; a SET never meets a full ring at a carve (the tripwire belt); with no journal bound nothing is queued" {
    const a = try testAlloc(64);
    defer a.deinit();
    const bm = try DataBitmap.init(testing.allocator, a, .{ .vol_tag = 0xDA7A, .term = 1, .blocks = 64, .ring_capacity = 1 });
    defer bm.deinit();
    // No journal: the deltas have no consumer and never fill the ring.
    const unjournaled0 = stats.data_alloc_bitmap_deltas_unjournaled.load();
    _ = bm.carveRun(0, 2).?;
    _ = bm.carveRun(8, 2).?;
    try testing.expectEqual(@as(usize, 0), bm.queuedDeltas());
    try testing.expectEqual(unjournaled0 + 2, stats.data_alloc_bitmap_deltas_unjournaled.load());
    try testing.expectEqual(@as(u32, 2), bm.clearRun(8, 2));
    // A journal bound: the ring is the sink's queue and its capacity binds.
    var j: RamJournal = .{ .gpa = testing.allocator };
    defer j.deinit();
    bm.journal = j.sink();
    _ = bm.carveRun(16, 2).?;
    const dropped0 = stats.data_alloc_bitmap_deltas_dropped.load();
    try testing.expectEqual(@as(u32, 1), bm.clearRun(0, 1));
    try testing.expectEqual(dropped0 + 1, stats.data_alloc_bitmap_deltas_dropped.load());
    try testing.expect(!bm.isSet(0));
    const trip0 = counters.tripwireCount("data_alloc_bitmap_set_dropped");
    stats.data_alloc_bitmap_set_dropped.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    _ = bm.carveRun(4, 1).?;
    try log.testing.expectAllFired();
    try testing.expectEqual(trip0 + 1, counters.tripwireCount("data_alloc_bitmap_set_dropped"));
}

test "pages before barrier #1: the dirty plan writes the alternate slot at a raised generation, commit advances the state, abort re-dirties; a second volume's checkpoint never writes the holder's pages" {
    const a = try testAlloc(70_000); // three pages
    defer a.deinit();
    const bm = try testBitmap(a, 1);
    defer bm.deinit();
    const region = try testing.allocator.alloc(u8, @intCast(regionLen(70_000)));
    defer testing.allocator.free(region);
    @memset(region, 0);
    try testing.expect(!bm.hasDirtyPages());
    _ = bm.carveRun(0, 3).?; // page 0
    _ = bm.carveRun(65_100, 2).?; // page 2
    try testing.expect(bm.hasDirtyPages());
    try testing.expect((try bm.pagesForCheckpoint(1, 2, 10)) == null);
    try testing.expect(bm.hasDirtyPages());
    var set = (try bm.pagesForCheckpoint(1, 1, 10)).?;
    try testing.expectEqual(@as(usize, 2), set.writes.len);
    try testing.expectEqual(@as(u32, 0), set.writes[0].page);
    try testing.expectEqual(@as(u8, 0), set.writes[0].target_slot);
    try testing.expectEqual(@as(u64, 10), set.writes[0].generation);
    try testing.expectEqual(@as(u32, 2), set.writes[1].page);
    set.applyToRegion(bm, region);
    set.deinit();
    try testing.expect(!bm.hasDirtyPages());
    try testing.expectEqual(@as(u64, 10), bm.resumeGeneration());
    // The second write of page 0 goes to slot B; a stale generation is raised.
    _ = bm.carveRun(5, 1).?;
    const raises0 = stats.data_alloc_bitmap_page_generation_raises.load();
    var set2 = try bm.takeDirtyPlan(10);
    try testing.expectEqual(@as(u8, 1), set2.writes[0].target_slot);
    try testing.expectEqual(@as(u64, 11), set2.writes[0].generation);
    try testing.expectEqual(raises0 + 1, stats.data_alloc_bitmap_page_generation_raises.load());
    set2.abort(bm);
    set2.deinit();
    try testing.expect(bm.hasDirtyPages());
    var set3 = try bm.takeDirtyPlan(12);
    set3.applyToRegion(bm, region);
    set3.deinit();
    // Load onto a fresh allocator: newest valid wins per page; the bits match.
    const b = try testAlloc(70_000);
    defer b.deinit();
    const bm2 = try testBitmap(b, 1);
    defer bm2.deinit();
    const loaded = try bm2.loadRegionImage(region);
    try testing.expectEqual(@as(u64, 2), loaded.pages_valid);
    try testing.expectEqual(@as(u64, 6), loaded.bits_set);
    try testing.expect(bm2.isSet(0) and bm2.isSet(2) and bm2.isSet(5) and bm2.isSet(65_101));
    try testing.expect(!bm2.isSet(3));
    try testing.expectEqual(@as(u64, 6), bm2.populationNow());
    try testing.expectEqual(@as(u64, 12), bm2.resumeGeneration());
    try testing.expect(bm.populationDriftCheck());
}

test "from_region_image refusals: both slots all-zero is fresh, a short image zero-extends, a non-zero page with no valid slot REFUSES" {
    const a = try testAlloc(100);
    defer a.deinit();
    const bm = try testBitmap(a, 1);
    defer bm.deinit();
    const l = try bm.loadRegionImage(&.{});
    try testing.expectEqual(@as(u64, 0), l.pages_valid);
    var region: [2 * page_len]u8 = @splat(0);
    _ = try bm.loadRegionImage(&region);
    region[page_len + 100] = 1; // slot B garbage, slot A zero
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.TornPage, bm.loadRegionImage(&region));
    try log.testing.expectAllFired();
    // A page of ANOTHER volume is "no valid slot" + non-zero ⇒ refused too.
    var other: [page_len]u8 = undefined;
    try encodePage(0, 1, 0x0BAD, &.{0xFF}, &other);
    @memcpy(region[0..page_len], &other);
    @memset(region[page_len..], 0);
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.TornPage, bm.loadRegionImage(&region));
    try log.testing.expectAllFired();
}

test "the_bitmap_agrees_with_the_census_oracle: a synthetic LOSS fires data_alloc_bitmap_drift; a LEAK is counted and named; the population drift check re-syncs" {
    const a = try testAlloc(128);
    defer a.deinit();
    var i: u64 = 0;
    while (i < 5) : (i += 1) _ = try a.allocateBlock();
    const bm = try testBitmap(a, 1);
    defer bm.deinit();
    _ = bm.carveRun(5, 3).?; // granted 5..7
    var referenced = [_]u64{ 0b11111, 0 };
    var granted = [_]u64{ 0b111 << 5, 0 };
    var leaks: std.ArrayList(u64) = .empty;
    defer leaks.deinit(testing.allocator);
    const clean = try bm.verifyBitmap(&referenced, &granted, &leaks);
    try testing.expect(clean.clean());
    // A leak: block 9 set, nothing references or grants it.
    try testing.expect(a.holderTakeBit(9));
    const leak_before = stats.data_alloc_bitmap_leak_candidates.load();
    const d = try bm.verifyBitmap(&referenced, &granted, &leaks);
    try testing.expectEqual(@as(u64, 1), d.leak);
    try testing.expectEqual(@as(u64, 0), d.loss);
    try testing.expectEqualSlices(u64, &.{9}, leaks.items);
    try testing.expectEqual(leak_before + 1, stats.data_alloc_bitmap_leak_candidates.load());
    // A loss: a referenced block reads clear.
    try testing.expect(a.holderReturnBit(2));
    const trip0 = counters.tripwireCount("data_alloc_bitmap_drift");
    stats.data_alloc_bitmap_drift.resetLogBudgetForTest();
    log.testing.expectErrorLogs(2);
    const d2 = try bm.verifyBitmap(&referenced, &granted, null);
    try log.testing.expectAllFired();
    try testing.expectEqual(@as(u64, 1), d2.loss);
    try testing.expectEqual(trip0 + 1, counters.tripwireCount("data_alloc_bitmap_drift"));
    // The population drift check: the bit moves above bypassed the holder
    // (one more here, so the net is non-zero); maintained ≠ counted, the
    // check fires once and re-syncs.
    try testing.expect(a.holderTakeBit(11));
    const pd0 = counters.tripwireCount("data_alloc_population_drift");
    stats.data_alloc_population_drift.resetLogBudgetForTest();
    log.testing.expectErrorLogs(2);
    try testing.expect(!bm.populationDriftCheck());
    try log.testing.expectAllFired();
    try testing.expectEqual(pd0 + 1, counters.tripwireCount("data_alloc_population_drift"));
    try testing.expect(bm.populationDriftCheck());
    try testing.expectEqual(a.bitPopulation(), bm.populationNow());
}

test "placement: the pages claim max(1, ceil(region / node)) extents of the home grant, write in claim order, publish coalesced refs; a failed write releases every claim; the ref screen" {
    const a = try testAlloc(40_000); // two pages ⇒ 16 KiB region
    defer a.deinit();
    const bm = try testBitmap(a, 1);
    defer bm.deinit();
    _ = bm.carveRun(0, 10).?;
    var home = try RamHome.init(testing.allocator, 8192, 8);
    defer home.deinit();
    const image = try testing.allocator.alloc(u8, @intCast(regionLen(40_000)));
    defer testing.allocator.free(image);
    bm.regionImage(5, image);
    var refs: std.ArrayList(records.BitmapRef) = .empty;
    defer refs.deinit(testing.allocator);
    try holdPages(testing.allocator, home.pool(), home.io(), 2, image, &refs);
    try testing.expectEqual(@as(u32, 2), home.claimed);
    try testing.expectEqual(@as(usize, 1), refs.items.len); // two adjacent nodes coalesce
    try testing.expectEqual(@as(u64, 16_384), refs.items[0].len);
    try testing.expectEqual(@as(u16, 2), refs.items[0].vol);
    // Read back through the refs and load: the same bits.
    const back = try readRegionImage(testing.allocator, home.io(), refs.items, 40_000);
    defer testing.allocator.free(back);
    try testing.expectEqualSlices(u8, image, back);
    const b = try testAlloc(40_000);
    defer b.deinit();
    const bm2 = try testBitmap(b, 2);
    defer bm2.deinit();
    const l = try bm2.loadRegionImage(back);
    try testing.expectEqual(@as(u64, 10), l.bits_set);
    try testing.expectEqual(@as(u64, 2), l.pages_valid);
    // A failed write releases the claims.
    home.fail_writes = true;
    var refs2: std.ArrayList(records.BitmapRef) = .empty;
    defer refs2.deinit(testing.allocator);
    try testing.expectError(error.Io, holdPages(testing.allocator, home.pool(), home.io(), 2, image, &refs2));
    try testing.expectEqual(@as(u32, 2), home.released);
    // The screen: inside the heap, node-aligned, Σ len within [region, ceil(region/node) × node].
    const heaps = [_]HeapGeometry{.{ .vol = 2, .heap_start = 0, .heap_len = 1 << 20, .node_size = 8192 }};
    try testing.expect(screenBitmapRefs(refs.items, 40_000, &heaps));
    try testing.expect(!screenBitmapRefs(&.{}, 40_000, &heaps));
    try testing.expect(!screenBitmapRefs(&.{.{ .vol = 3, .start = 0, .len = 16_384 }}, 40_000, &heaps));
    try testing.expect(!screenBitmapRefs(&.{.{ .vol = 2, .start = 4096, .len = 16_384 }}, 40_000, &heaps)); // not node-aligned
    try testing.expect(!screenBitmapRefs(&.{.{ .vol = 2, .start = 0, .len = 8192 }}, 40_000, &heaps)); // too short
    try testing.expect(!screenBitmapRefs(&.{.{ .vol = 2, .start = 0, .len = 32_768 }}, 40_000, &heaps)); // too long
    try testing.expect(!screenBitmapRefs(&.{.{ .vol = 2, .start = (1 << 20) - 8192, .len = 16_384 }}, 40_000, &heaps)); // past the heap
}

fn dataBitmapPageFuzz(_: void, smith: *std.testing.Smith) !void {
    var page: [page_len + 8]u8 = undefined;
    const plen = smith.slice(&page);
    _ = decodePage(page[0..plen], 0, 0) catch {};
    if (plen >= page_len) {
        const img = page[0..page_len];
        const index = std.mem.readInt(u32, img[off_page_index..][0..4], .little);
        const tag = std.mem.readInt(u64, img[off_vol_tag..][0..8], .little);
        checksum.stamp(img, off_checksum);
        if (decodePage(img, index, tag)) |d| {
            var re: [page_len]u8 = undefined;
            try encodePage(index, d.generation, tag, d.bits, &re);
            try std.testing.expectEqualSlices(u8, img, &re);
        } else |_| {}
    }
}

test "fuzz: the data bitmap page decoder is total on arbitrary and restamped pages; a decoded page re-encodes byte-identically" {
    try fuzzkit.fuzz({}, dataBitmapPageFuzz, .{});
}
