//! The extent allocator's on-disk half (charter §5.1 "allocator bitmap",
//! §5.3 "Space"; `docs/porting/kv_alloc_superblock.md` §4.1, §4.2, §4.6):
//! A/B bitmap page images, the journaled alloc/free delta payloads, and the
//! `ExtentAllocator` wrapper over `alloc_core.ExtCore` — page states, dirty
//! tracking, mount-time load + replay, the DUR-4 write laws — all as BYTE
//! IMAGES. Nothing here touches a device: the io task executes the
//! `PageWriteSet` this module plans and reports back `commit` or `abort`.
//!
//! The bitmap is a checkpoint ACCELERATOR, not the sole truth: every
//! alloc/free also rides the journal as a delta record (kind
//! `tree_alloc_reserved = 4`), and a mount replays the window's deltas over
//! the loaded pages; the replay-window tail rule (K6b) keeps those records
//! inside `[tail, head]` until their effects are durable in pages.

const std = @import("std");
const fuzzkit = @import("../core/fuzzkit.zig");
const checksum = @import("../core/checksum.zig");
const counters = @import("../core/counters.zig");
const log = @import("../core/log.zig");
const alloc_core = @import("alloc_core.zig");

pub const ExtCore = alloc_core.ExtCore;
pub const AllocClass = alloc_core.AllocClass;

/// One bitmap page = one 4 KiB sector: the torn-write unit, so an A/B pair
/// makes a tear damage only the copy being replaced.
pub const page_len: usize = 4096;
/// `magic u32 ‖ page_index u32 ‖ generation u64 ‖ hash64 u64`.
pub const page_hdr_len: usize = 24;
pub const page_data_len: usize = page_len - page_hdr_len;
/// Extents per page: `4072 × 8`. A 1 TiB heap of 256 KiB extents is 4 M bits
/// ⇒ 512 KiB × 2 of bitmap.
pub const page_bits: u64 = page_data_len * 8;
/// u64 words per page (`4072 / 8`); 32,576 = 509 × 64, so page boundaries
/// fall on word boundaries and the mirror serializes word-wise.
pub const words_per_page: usize = page_data_len / 8;
/// ASCII "KVAB" read as a little-endian u32: ASCII order on disk.
pub const page_magic: u32 = std.mem.readInt(u32, "KVAB", .little);
const off_magic = 0;
const off_page_index = 4;
const off_generation = 8;
const off_checksum = 16;

/// Heap-allocator deltas ride the journal under this tree id (the DATA
/// allocation bitmap's deltas of rung 6 take `key.TREE_DATA_ALLOC_RESERVED`
/// = 5 — a distinct kind, never a key-length arm under this one; decision
/// 0002 row 44).
pub const tree_alloc_reserved: u8 = 4;
pub const rec_allocated: u8 = 1;
pub const rec_freed: u8 = 2;
/// Big-endian u64 — memcmp-ordered like every key.
pub const extent_key_len: usize = 8;

/// The pressure valve's cap: far above steady state (any parked retirement
/// drains within 2 barriered cycles); `builder_pending_cap` because the
/// image builder never frees.
pub const pending_free_cap: usize = 65_536;
pub const builder_pending_cap: usize = 64;

/// Fresh page state: the first write goes to slot A.
pub const slot_none: u8 = 2;

// ---- observability (names per the spec) ----
pub var meta_kv_pending_free_parked: counters.ShardedCounter = .{};
pub var meta_kv_pending_free_released: counters.ShardedCounter = .{};
/// ≈ 0 in steady state.
pub var meta_kv_pending_free_overflow: counters.ShardedCounter = .{};
pub var meta_kv_replay_root_frees_dropped: counters.ShardedCounter = .{};
/// Extents a named root reaches that the recovered allocator held free,
/// adopted at mount (`adoptLive`); 0 unless a kill fell between a root's
/// naming and its claim's bitmap page.
pub var meta_kv_replay_root_claims_adopted: counters.ShardedCounter = .{};
pub var meta_kv_bitmap_generation_raises: counters.ShardedCounter = .{};
/// The wedge detector: every parked retirement, wherever parked.
pub var meta_kv_pending_free: counters.Gauge = .{};
pub var meta_kv_heap_full_cycles: counters.ShardedCounter = .{};
/// The §4.7 heap-full posture word (0/1).
pub var meta_kv_heap_full: counters.Gauge = .{};
/// Extents promised to pending SMOs by admitted commits; 0 at quiesce.
pub var meta_kv_heap_promised: counters.Gauge = .{};

pub fn bitmapPagesFor(total_extents: u64) u64 {
    return (total_extents + page_bits - 1) / page_bits;
}

/// Pages × 2 slots × 4 KiB.
pub fn bitmapRegionLen(total_extents: u64) u64 {
    return bitmapPagesFor(total_extents) * 2 * page_len;
}

// ---------------------------------------------------------------------------
// Journaled deltas.
// ---------------------------------------------------------------------------

pub fn extentKey(extent: u64) [extent_key_len]u8 {
    var k: [extent_key_len]u8 = undefined;
    std.mem.writeInt(u64, &k, extent, .big);
    return k;
}

pub const AllocDelta = union(enum) {
    allocated: u64,
    /// `retire_seq` is the HISTORICAL tag kept byte-for-byte: 0 = never
    /// referenced by any checkpoint ⇒ immediately reusable; non-zero ⇒
    /// parks — gated on the record's journal POSITION, not this value.
    freed: struct { extent: u64, retire_seq: u64 },

    pub fn extent(self: AllocDelta) u64 {
        return switch (self) {
            .allocated => |e| e,
            .freed => |f| f.extent,
        };
    }
};

/// `[1]`.
pub fn allocRecordValue() [1]u8 {
    return .{rec_allocated};
}

/// `[2, retire_seq LE]` (9 bytes).
pub fn freeRecordValue(retire_seq: u64) [9]u8 {
    var v: [9]u8 = undefined;
    v[0] = rec_freed;
    std.mem.writeInt(u64, v[1..9], retire_seq, .little);
    return v;
}

pub const DeltaError = error{ BadExtentKey, MalformedDeltaValue };

/// The verbatim refusal texts of the delta decoder.
pub const DeltaRefusal = union(enum) {
    bad_extent_key: usize,
    malformed_value: struct { len: usize, tag: ?u8, extent: u64 },

    pub fn write(self: DeltaRefusal, w: *std.Io.Writer) std.Io.Writer.Error!void {
        switch (self) {
            .bad_extent_key => |n| try w.print("extent key must be {d} bytes, got {d}", .{ extent_key_len, n }),
            .malformed_value => |m| {
                try w.print("malformed allocator delta value ({d} bytes, tag ", .{m.len});
                if (m.tag) |t| try w.print("Some({d})", .{t}) else try w.writeAll("None");
                try w.print(") for extent {d}", .{m.extent});
            },
        }
    }
};

pub fn decodeExtentKey(key: []const u8, diag: ?*DeltaRefusal) DeltaError!u64 {
    if (key.len != extent_key_len) {
        if (diag) |d| d.* = .{ .bad_extent_key = key.len };
        return error.BadExtentKey;
    }
    return std.mem.readInt(u64, key[0..extent_key_len], .big);
}

/// Total: exactly `[1]` or `[2] ++ 8 bytes` decodes; anything else is
/// Corrupt — LOUD at mount replay (the entry checksum already verified, so
/// garbage here is a writer bug).
pub fn decodeAllocRecord(key: []const u8, value: []const u8, diag: ?*DeltaRefusal) DeltaError!AllocDelta {
    const extent = try decodeExtentKey(key, diag);
    if (value.len == 1 and value[0] == rec_allocated) return .{ .allocated = extent };
    if (value.len == 9 and value[0] == rec_freed) {
        return .{ .freed = .{ .extent = extent, .retire_seq = std.mem.readInt(u64, value[1..9], .little) } };
    }
    if (diag) |d| d.* = .{ .malformed_value = .{ .len = value.len, .tag = if (value.len > 0) value[0] else null, .extent = extent } };
    return error.MalformedDeltaValue;
}

// ---------------------------------------------------------------------------
// The page image.
// ---------------------------------------------------------------------------

pub const PageEncodeError = error{PayloadTooLarge};

/// `bits` (≤ 4072 bytes, zero-padded) stamped with `page_index` and
/// `generation`, checksummed with the field zeroed.
pub fn encodeBitmapPage(page_index: u32, generation: u64, bits: []const u8, out: *[page_len]u8) PageEncodeError!void {
    if (bits.len > page_data_len) return error.PayloadTooLarge;
    @memset(out, 0);
    std.mem.writeInt(u32, out[off_magic..][0..4], page_magic, .little);
    std.mem.writeInt(u32, out[off_page_index..][0..4], page_index, .little);
    std.mem.writeInt(u64, out[off_generation..][0..8], generation, .little);
    @memcpy(out[page_hdr_len .. page_hdr_len + bits.len], bits);
    checksum.stamp(out, off_checksum);
}

pub const PageDecodeError = error{ Truncated, BadMagic, WrongIndex, ChecksumMismatch };

pub const DecodedPage = struct { generation: u64, bits: *const [page_data_len]u8 };

/// length → magic → page_index (misdirected-write guard) → checksum. At
/// LOAD every failure means "slot absent" — never loud.
pub fn decodeBitmapPage(buf: []const u8, page_index: u32) PageDecodeError!DecodedPage {
    if (buf.len < page_len) return error.Truncated;
    const img = buf[0..page_len];
    if (std.mem.readInt(u32, img[off_magic..][0..4], .little) != page_magic) return error.BadMagic;
    if (std.mem.readInt(u32, img[off_page_index..][0..4], .little) != page_index) return error.WrongIndex;
    if (!checksum.verify(img, off_checksum)) return error.ChecksumMismatch;
    return .{ .generation = std.mem.readInt(u64, img[off_generation..][0..8], .little), .bits = img[page_hdr_len..page_len] };
}

/// Offset of page `page`'s slot `slot` relative to the region base.
pub fn pageSlotOffset(page: u64, slot: u8) u64 {
    return page * 2 * page_len + @as(u64, slot) * page_len;
}

// ---------------------------------------------------------------------------
// The wrapper.
// ---------------------------------------------------------------------------

const PageState = struct {
    generation: std.atomic.Value(u64),
    /// 0 = A, 1 = B, `slot_none` = fresh.
    slot: std.atomic.Value(u8),
};

pub const ReplayRecord = struct { tree_id: u8, key: []const u8, value: []const u8 };

/// One recovered journal entry: records in position order, so record `i`'s
/// position (the park gate) is `seq + i`.
pub const ReplayEntry = struct { seq: u64, records: []const ReplayRecord };

pub const NoSpace = struct {
    free: u64,
    reserve: u64,

    pub fn format(self: NoSpace, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.print("no space: {d} free extents with the {d}-extent compaction reserve intact — allocation refused (ENOSPC)", .{ self.free, self.reserve });
    }
};

pub const PendingFreeFull = struct {
    pending: u64,

    pub fn format(self: PendingFreeFull, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.print("pending-free list full ({d} extents awaiting durable checkpoints); checkpoint required before further frees", .{self.pending});
    }
};

pub const LoadError = error{ MalformedDelta, OutOfMemory };

pub const LoadRefusal = union(enum) {
    delta: DeltaRefusal,

    pub fn write(self: LoadRefusal, w: *std.Io.Writer) std.Io.Writer.Error!void {
        switch (self) {
            .delta => |d| try d.write(w),
        }
    }
};

const ReplayedPark = struct { seq: u64, extent: u64 };

fn parkLess(_: void, a: ReplayedPark, b: ReplayedPark) bool {
    if (a.seq != b.seq) return a.seq < b.seq;
    return a.extent < b.extent;
}

pub const ExtentAllocator = struct {
    gpa: std.mem.Allocator,
    core: ExtCore,
    pages: u64,
    page_states: []PageState,
    /// One bit per page: diverged from the newest on-disk copy since the last
    /// `beginDirtyPageWrite`.
    dirty: []std.atomic.Value(u64),
    /// Replayed in-window frees `load` deferred until `parkReplayedFrees`
    /// knows the mounted roots. Mount-time only (single-threaded).
    replayed_parks: std.ArrayList(ReplayedPark),
    /// A grant mirror's refill mark and hook (`installLowWater`); 0 and
    /// null on the manager's own allocator.
    low_water: std.atomic.Value(u64) = .init(0),
    on_low: ?LowWaterHook = null,

    /// All-free, EVERY page dirty (the first checkpoint persists the whole
    /// bitmap).
    pub fn format(gpa: std.mem.Allocator, total: u64, reserve: u64, pending_cap: usize) !ExtentAllocator {
        var self = try initEmpty(gpa, total, reserve, pending_cap);
        errdefer self.deinit();
        var page: u64 = 0;
        while (page < self.pages) : (page += 1) self.markPageDirty(page);
        return self;
    }

    /// A JOINED appender's allocator (charter §5.5 "allocates only from
    /// its own grants"): the volume's whole extent index space with EVERY
    /// extent claimed except the granted runs — the grant pool IS the free
    /// set, so `claimInternal` draws from it and a retired image's
    /// pending-free (gated on the joiner's OWN ring tail) recycles into it.
    /// No page is dirty: the joiner's bitmap pages have no device home —
    /// the manager's `extent_grant:{a}` record and the alloc/free deltas in
    /// the joiner's ring are the two durable homes a recovery replays.
    pub fn grantMirror(gpa: std.mem.Allocator, total: u64, reserve: u64, pending_cap: usize, granted: []const GrantedRun) !ExtentAllocator {
        var self = try initEmpty(gpa, total, reserve, pending_cap);
        errdefer self.deinit();
        var extent: u64 = 0;
        while (extent < total) : (extent += 1) self.core.markAllocated(extent);
        for (granted) |r| self.absorbGranted(r);
        return self;
    }

    /// One granted run of extent INDICES (the manager's `GrantRun` is a
    /// device extent; the mirror speaks indices).
    pub const GrantedRun = struct { start: u64, len: u64 };

    /// A grant landed (the join's word, a refill): its extents become the
    /// pool's free set. Mount/cadence-time, under the caller's lock.
    pub fn absorbGranted(self: *ExtentAllocator, r: GrantedRun) void {
        var i: u64 = 0;
        while (i < r.len) : (i += 1) self.core.release(r.start + i);
    }

    /// A surplus extent leaving the pool (returned to the manager): claimed
    /// out of the free set so no later SMO can draw it. Null = no free
    /// extent to withdraw.
    pub fn withdrawGranted(self: *const ExtentAllocator) ?u64 {
        return self.core.claim(.internal) catch null;
    }

    fn initEmpty(gpa: std.mem.Allocator, total: u64, reserve: u64, pending_cap: usize) !ExtentAllocator {
        const pages = bitmapPagesFor(total);
        var core = try ExtCore.init(gpa, total, reserve, pending_cap);
        errdefer core.deinit();
        const states = try gpa.alloc(PageState, @intCast(pages));
        errdefer gpa.free(states);
        for (states) |*s| s.* = .{ .generation = .init(0), .slot = .init(slot_none) };
        const dirty = try gpa.alloc(std.atomic.Value(u64), @intCast((pages + 63) / 64));
        errdefer gpa.free(dirty);
        for (dirty) |*d| d.* = .init(0);
        return .{
            .gpa = gpa,
            .core = core,
            .pages = pages,
            .page_states = states,
            .dirty = dirty,
            .replayed_parks = .empty,
        };
    }

    pub fn deinit(self: *ExtentAllocator) void {
        self.replayed_parks.deinit(self.gpa);
        self.gpa.free(self.dirty);
        self.gpa.free(self.page_states);
        self.core.deinit();
        self.* = undefined;
    }

    /// Mount: `region` is the bitmap region as read (short ⇒ zero-extended:
    /// zeros verify nothing and read as fresh pages), `mounted_tail` the
    /// selected ledger record's `journal_tail_seq`, `replay` the recovered
    /// window in order.
    pub fn load(gpa: std.mem.Allocator, region: []const u8, total: u64, reserve: u64, pending_cap: usize, mounted_tail: u64, replay: []const ReplayEntry, diag: ?*LoadRefusal) LoadError!ExtentAllocator {
        var self = try initEmpty(gpa, total, reserve, pending_cap);
        errdefer self.deinit();

        // 1–2. Per page: both slots decoded; newest valid wins (strict >, A
        // wins ties — writes never produce ties, law 2); none valid ⇒ fresh.
        var page: u64 = 0;
        while (page < self.pages) : (page += 1) {
            var newest: ?struct { gen: u64, slot: u8, bits: *const [page_data_len]u8 } = null;
            var slot: u8 = 0;
            while (slot < 2) : (slot += 1) {
                const off = pageSlotOffset(page, slot);
                if (off + page_len > region.len) continue;
                const decoded = decodeBitmapPage(region[off .. off + page_len], @intCast(page)) catch continue;
                if (newest == null or decoded.generation > newest.?.gen) newest = .{ .gen = decoded.generation, .slot = slot, .bits = decoded.bits };
            }
            if (newest) |n| {
                const first = page * page_bits;
                const last = @min(first + page_bits, total);
                var extent = first;
                while (extent < last) : (extent += 1) {
                    const off: usize = @intCast(extent - first);
                    if (n.bits[off / 8] & (@as(u8, 1) << @intCast(off % 8)) != 0) self.core.markAllocated(extent);
                }
                self.page_states[@intCast(page)] = .{ .generation = .init(n.gen), .slot = .init(n.slot) };
            }
        }

        // 3. Seed the coverage clock at the mounted tail: nothing at or
        // after it is covered, so no replayed free drains before a
        // POST-mount checkpoint's tail passes it.
        _ = self.core.advanceDurable(mounted_tail, {}, ignoreRelease);

        // 4. Fold the window's deltas per extent, newest wins (the gate is
        // the record's POSITION). Folding first is load-bearing: a free
        // superseded by a later in-window re-alloc must fold AWAY, or its
        // post-mount drain would clear a LIVE extent's bit.
        const Folded = struct { seq: u64, delta: AllocDelta };
        var folded: std.AutoHashMapUnmanaged(u64, Folded) = .empty;
        defer folded.deinit(gpa);
        for (replay) |entry| {
            for (entry.records, 0..) |rec, i| {
                if (rec.tree_id != tree_alloc_reserved) continue;
                var ddiag: DeltaRefusal = undefined;
                const delta = decodeAllocRecord(rec.key, rec.value, &ddiag) catch {
                    if (diag) |d| d.* = .{ .delta = ddiag };
                    return error.MalformedDelta;
                };
                try folded.put(gpa, delta.extent(), .{ .seq = entry.seq + i, .delta = delta });
            }
        }

        // 5. Apply survivors; every touched page is dirty.
        var it = folded.iterator();
        while (it.next()) |kv| {
            const extent = kv.key_ptr.*;
            switch (kv.value_ptr.delta) {
                .allocated => self.core.markAllocated(extent),
                .freed => |f| if (f.retire_seq == 0) {
                    // Never referenced by any checkpoint: reusable now.
                    self.core.release(extent);
                } else {
                    // In-window by construction ⇒ not covered. Set the bit
                    // now (it may live only in the window) so the FIFO's
                    // claimed-while-pending contract holds; the park itself
                    // waits for the roots.
                    self.core.markAllocated(extent);
                    try self.replayed_parks.append(gpa, .{ .seq = kv.value_ptr.seq, .extent = extent });
                },
            }
            self.markDirty(extent);
        }
        std.mem.sort(ReplayedPark, self.replayed_parks.items, {}, parkLess);
        return self;
    }

    pub const ParkOutcome = struct { parked: u64, dropped: u64 };

    /// Once, after every tree is open (roots final) and before any
    /// post-mount SMO. A free naming a LIVE ROOT is DROPPED (the
    /// unpublished-root-swap carve-out: a root swap journals no pointer
    /// record, so a kill before the naming ledger record makes the
    /// predecessor live again while its `free(old)` is in the window —
    /// parking it would release the live root at the first checkpoint).
    /// Others park FORCED: a recovered window may carry more frees than the
    /// cap, and refusing made that image unmountable.
    pub fn parkReplayedFrees(self: *ExtentAllocator, live_roots: []const u64) std.mem.Allocator.Error!ParkOutcome {
        var out: ParkOutcome = .{ .parked = 0, .dropped = 0 };
        var overflowed: u64 = 0;
        for (self.replayed_parks.items) |p| {
            if (std.mem.indexOfScalar(u64, live_roots, p.extent) != null) {
                out.dropped += 1;
                meta_kv_replay_root_frees_dropped.inc();
                log.warn("mount: replayed free of extent {d} (seq {d}) names a LIVE tree root — an unpublished root swap's retirement; dropped, the root stays allocated (meta_kv_replay_root_frees_dropped). Its successor image is unrouted: fsck C13 reclaims it on a forest grant", .{ p.extent, p.seq });
                continue;
            }
            if (try self.core.freePendingForced(p.extent, p.seq)) {
                overflowed += 1;
                meta_kv_pending_free_overflow.inc();
            }
            out.parked += 1;
            meta_kv_pending_free_parked.inc();
        }
        self.replayed_parks.clearRetainingCapacity();
        if (overflowed > 0) {
            log.warn("mount: {d} replayed pending-free record(s) exceeded the FIFO cap and parked in the overflow (a pre-crash pinned tail accumulated them); they drain at the first post-mount durable checkpoint", .{overflowed});
        }
        meta_kv_pending_free.set(self.core.pendingCount());
        return out;
    }

    /// Mount only, after the replay fold and before the first claim: an
    /// extent a root the recovered state names reaches is LIVE whatever the
    /// bitmap says — a claim made in RAM (a guest root minted on first
    /// touch journals no delta) is named by C4b / C11 before C6 lands its
    /// page. Sets the bit and dirties its page (the next checkpoint writes
    /// it); `true` = the bit was clear (counted).
    pub fn adoptLive(self: *ExtentAllocator, extent: u64) bool {
        if (self.core.isAllocated(extent)) return false;
        self.core.markAllocated(extent);
        self.markDirty(extent);
        meta_kv_replay_root_claims_adopted.inc();
        return true;
    }

    pub fn totalExtents(self: *const ExtentAllocator) u64 {
        return self.core.total;
    }

    pub fn reserveExtents(self: *const ExtentAllocator) u64 {
        return self.core.reserve;
    }

    /// Claims won since init (monotone).
    pub fn claimsTotal(self: *const ExtentAllocator) u64 {
        return self.core.claimsTotal();
    }

    pub fn freeExtents(self: *const ExtentAllocator) u64 {
        return self.core.freeExtents();
    }

    pub fn pendingCount(self: *const ExtentAllocator) u64 {
        return self.core.pendingCount();
    }

    pub fn durableSeq(self: *const ExtentAllocator) u64 {
        return self.core.durableSeq();
    }

    pub fn isAllocated(self: *const ExtentAllocator, extent: u64) bool {
        return self.core.isAllocated(extent);
    }

    /// Any page holding a delta the next write must persist — including a
    /// release the last cycle's barrier made AFTER that cycle wrote its
    /// pages (the shutdown fixpoint's second term: otherwise released
    /// images read CLAIMED at the next mount).
    pub fn hasDirtyPages(self: *const ExtentAllocator) bool {
        for (self.dirty) |*d| if (d.load(.acquire) != 0) return true;
        return false;
    }

    /// Newest page generation on disk; the mount's `checkpoint_seq` starts
    /// at `max(ledger.seq, resume_generation())` so a new write never ties.
    pub fn resumeGeneration(self: *const ExtentAllocator) u64 {
        var max: u64 = 0;
        for (self.page_states) |*s| max = @max(max, s.generation.load(.acquire));
        return max;
    }

    /// The typed ENOSPC detail for a `.user` refusal.
    pub fn noSpace(self: *const ExtentAllocator) NoSpace {
        return .{ .free = self.core.freeExtents(), .reserve = self.core.reserve };
    }

    pub fn pendingFreeFull(self: *const ExtentAllocator) PendingFreeFull {
        return .{ .pending = self.core.pendingCount() };
    }

    pub fn claimUser(self: *const ExtentAllocator) error{NoSpace}!u64 {
        return self.claim(.user);
    }

    /// May consume the reserve; `NoSpace` here is a genuinely exhausted heap.
    pub fn claimInternal(self: *const ExtentAllocator) error{NoSpace}!u64 {
        return self.claim(.internal);
    }

    fn claim(self: *const ExtentAllocator, class: AllocClass) error{NoSpace}!u64 {
        const extent = self.core.claim(class) catch |err| switch (err) {
            error.NoSpace => return error.NoSpace,
            error.InvariantDrift => {
                log.err("KV extent allocator INVARIANT DRIFT: the free budget won an entitlement the bitmap could not honour after {d} full rescans (free={d}, reserve={d}, total={d}). Refusing the claim instead of spinning; the volume needs an fsck (class C2/C3)", .{ alloc_core.claim_rescan_limit, self.core.freeExtents(), self.core.reserve, self.core.total });
                return error.NoSpace;
            },
        };
        self.markDirty(extent);
        if (self.core.freeExtents() <= self.low_water.load(.acquire)) if (self.on_low) |h| h.run(h.ctx);
        return extent;
    }

    /// The pool's low-water hook (a grant mirror): called on the claiming
    /// thread after a claim that leaves `freeExtents() ≤ low_water` — the
    /// joiner's proactive refill, kicked by the POOL's state rather than
    /// the checkpoint clock (a storm right after the join drains the
    /// floor's SMO headroom inside one cadence). Never blocks: the hook
    /// spawns. Installed once at the arm (single-threaded); the mark moves
    /// with the target at every cadence.
    pub const LowWaterHook = struct {
        ctx: *anyopaque,
        run: *const fn (ctx: *anyopaque) void,
    };

    pub fn installLowWater(self: *ExtentAllocator, hook: LowWaterHook, low_water: u64) void {
        self.on_low = hook;
        self.low_water.store(low_water, .release);
    }

    pub fn setLowWater(self: *ExtentAllocator, low_water: u64) void {
        self.low_water.store(low_water, .release);
    }

    /// The hook's owner is going away (no claim runs concurrently: the
    /// lane and the task are stopped first).
    pub fn uninstallLowWater(self: *ExtentAllocator) void {
        self.on_low = null;
        self.low_water.store(0, .release);
    }

    pub fn freePending(self: *const ExtentAllocator, extent: u64, gate_seq: u64) error{PendingFreeFull}!void {
        try self.core.freePending(extent, gate_seq);
        meta_kv_pending_free_parked.inc();
        meta_kv_pending_free.set(self.core.pendingCount());
    }

    /// Never refused by the valve: the flush pass's own compactions and
    /// mount-side re-parking.
    pub fn freePendingForced(self: *const ExtentAllocator, extent: u64, gate_seq: u64) std.mem.Allocator.Error!void {
        if (try self.core.freePendingForced(extent, gate_seq)) meta_kv_pending_free_overflow.inc();
        meta_kv_pending_free_parked.inc();
        meta_kv_pending_free.set(self.core.pendingCount());
    }

    pub fn pendingHasRoom(self: *const ExtentAllocator) bool {
        return self.core.pendingHasRoom();
    }

    /// For an SMO that parks `n` retirements (a merge parks two).
    pub fn pendingHasRoomFor(self: *const ExtentAllocator, n: u64) bool {
        return self.core.pendingRoom() >= n;
    }

    /// The coverage gate: `tail` is a post-barrier ledger record's
    /// `journal_tail_seq`. Released pages are dirtied.
    pub fn advanceDurable(self: *const ExtentAllocator, tail: u64) u64 {
        const n = self.core.advanceDurable(tail, self, markDirtyCb);
        if (n > 0) meta_kv_pending_free_released.add(n);
        meta_kv_pending_free.set(self.core.pendingCount());
        return n;
    }

    fn markDirtyCb(self: *const ExtentAllocator, extent: u64) void {
        self.markDirty(extent);
    }

    /// A build abandoned before publication: never referenced by any
    /// checkpoint, straight back to the pool.
    pub fn releaseUnpublished(self: *const ExtentAllocator, extent: u64) void {
        self.core.release(extent);
        self.markDirty(extent);
    }

    fn markDirty(self: *const ExtentAllocator, extent: u64) void {
        self.markPageDirty(extent / page_bits);
    }

    fn markPageDirty(self: *const ExtentAllocator, page: u64) void {
        // acq_rel pairs with the snapshot's acq_rel swap.
        _ = self.dirty[@intCast(page / 64)].fetchOr(@as(u64, 1) << @intCast(page % 64), .acq_rel);
    }

    /// One planned page write: `offset` relative to the bitmap region base.
    pub const PageWrite = struct { page: u32, offset: u64, target_slot: u8, generation: u64, image: [page_len]u8 };

    /// The planned write of every dirty page — DUR-4 as an image mutation.
    /// Law 1: snapshot-and-clear is transactional — `abort` folds the
    /// snapshot back into the dirty set (allocator deltas create no
    /// dirty-node floor; dropping the set would let the next tail advance
    /// past the only copy of those records). Law 2: an image never ties or
    /// trails the page's newest on-disk generation — the generation is
    /// RAISED to `newest + 1` (a checkpoint retry after a failed cycle
    /// arrives with the same seq; refusing would wedge the retry) and
    /// counted on `meta_kv_bitmap_generation_raises`.
    pub const PageWriteSet = struct {
        gpa: std.mem.Allocator,
        writes: []PageWrite,
        settled: bool = false,

        pub fn deinit(self: *PageWriteSet) void {
            std.debug.assert(self.settled or self.writes.len == 0);
            self.gpa.free(self.writes);
            self.* = undefined;
        }

        /// The writes landed: record the new newest-valid slot per page.
        pub fn commit(self: *PageWriteSet, alloc: *const ExtentAllocator) void {
            std.debug.assert(!self.settled);
            for (self.writes) |*w| {
                const state = &alloc.page_states[w.page];
                state.generation.store(w.generation, .release);
                state.slot.store(w.target_slot, .release);
            }
            self.settled = true;
        }

        /// Law 1: any failure (encode or I/O) re-dirties every snapshotted
        /// page. A partially landed batch is safe to re-dirty wholesale:
        /// rewriting a page that did land costs one more image at a higher
        /// generation, never a lost bit.
        pub fn abort(self: *PageWriteSet, alloc: *const ExtentAllocator) void {
            std.debug.assert(!self.settled);
            for (self.writes) |*w| alloc.markPageDirty(w.page);
            self.settled = true;
        }

        /// Apply every write to a byte image of the region (tests, the
        /// offline builder) and commit.
        pub fn applyToRegion(self: *PageWriteSet, alloc: *const ExtentAllocator, region: []u8) void {
            for (self.writes) |*w| @memcpy(region[@intCast(w.offset)..][0..page_len], &w.image);
            self.commit(alloc);
        }
    };

    /// Snapshot-and-clear the dirty set (claims racing this re-dirty their
    /// page and ride the next checkpoint — never lost), serialize each page
    /// from a mirror snapshot into the slot NOT holding its newest copy.
    /// Serialized-checkpoint-task only. Durability rides the caller's
    /// barrier; the caller records `generation` in its ledger record as
    /// `alloc_bitmap_generation`.
    pub fn beginDirtyPageWrite(self: *const ExtentAllocator, generation: u64) std.mem.Allocator.Error!PageWriteSet {
        // Every allocation that can fail happens BEFORE the dirty words are
        // cleared, so law 1 has nothing to restore on an allocation failure
        // (the page list is bounded by the page count: ≤ 128 on a 1 TiB heap).
        const page_buf = try self.gpa.alloc(u32, @intCast(self.pages));
        defer self.gpa.free(page_buf);
        const words = try self.gpa.alloc(u64, self.core.wordCount());
        defer self.gpa.free(words);
        var n_pages: usize = 0;
        for (self.dirty, 0..) |*word, wi| {
            // acq_rel pairs with `markPageDirty`'s fetchOr.
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

        self.core.snapshotWords(words);
        const writes = self.gpa.alloc(PageWrite, pages.len) catch |err| {
            // Law 1: the snapshot is this write's only copy.
            for (pages) |p| self.markPageDirty(p);
            return err;
        };
        for (pages, writes) |page, *w| {
            const state = &self.page_states[page];
            const cur_gen = state.generation.load(.acquire);
            const cur_slot = state.slot.load(.acquire);
            const has_copy = cur_slot != slot_none;
            var page_gen = generation;
            if (has_copy and generation <= cur_gen) {
                page_gen = cur_gen + 1;
                meta_kv_bitmap_generation_raises.inc();
                log.warn("bitmap page {d}: requested generation {d} does not exceed the newest on-disk copy {d} (a checkpoint retry after a failed cycle) — writing at {d} instead; a tie would make newest-valid-wins pick between the A/B slots arbitrarily", .{ page, generation, cur_gen, page_gen });
            }
            const target: u8 = if (cur_slot == slot_none) 0 else 1 - cur_slot;
            w.* = .{ .page = page, .offset = pageSlotOffset(page, target), .target_slot = target, .generation = page_gen, .image = undefined };
            var bits: [page_data_len]u8 = @splat(0);
            const start = @as(usize, page) * words_per_page;
            const end = @min(start + words_per_page, words.len);
            for (words[start..end], 0..) |word, i| std.mem.writeInt(u64, bits[i * 8 ..][0..8], word, .little);
            // The payload is exactly the page data area, never oversize.
            encodeBitmapPage(page, page_gen, &bits, &w.image) catch unreachable;
        }
        return .{ .gpa = self.gpa, .writes = writes };
    }

    /// `beginDirtyPageWrite` applied straight to a byte image of the
    /// region; returns the pages written, ascending (caller frees).
    pub fn writeDirtyPagesToRegion(self: *const ExtentAllocator, region: []u8, generation: u64) std.mem.Allocator.Error![]u32 {
        var set = try self.beginDirtyPageWrite(generation);
        defer set.deinit();
        const pages = try self.gpa.alloc(u32, set.writes.len);
        for (set.writes, pages) |w, *p| p.* = w.page;
        if (set.writes.len > 0) set.applyToRegion(self, region);
        return pages;
    }
};

fn ignoreRelease(_: void, _: u64) void {}

/// The mount's first checkpoint seq: `max(ledger.seq, resume_generation)` so
/// a new page write can never tie an existing valid slot.
pub fn resumeCheckpointSeq(ledger_seq: u64, resume_generation: u64) u64 {
    return @max(ledger_seq, resume_generation);
}

// ---------------------------------------------------------------------------
// Tests (PURE: byte images stand in for the device).
// ---------------------------------------------------------------------------

const testing = std.testing;

fn fmtRefusal(buf: []u8, r: anytype) []const u8 {
    var w: std.Io.Writer = .fixed(buf);
    r.write(&w) catch unreachable;
    return w.buffered();
}

test "extent keys are big-endian, memcmp-ordered, and only 8 bytes decode" {
    const k1 = extentKey(1);
    const k256 = extentKey(256);
    const k_big = extentKey(1 << 40);
    try testing.expect(std.mem.order(u8, &k1, &k256) == .lt);
    try testing.expect(std.mem.order(u8, &k256, &k_big) == .lt);
    try testing.expectEqual(@as(u64, 256), try decodeExtentKey(&k256, null));
    var diag: DeltaRefusal = undefined;
    try testing.expectError(error.BadExtentKey, decodeExtentKey(&.{ 1, 2, 3 }, &diag));
    var buf: [128]u8 = undefined;
    try testing.expectEqualStrings("extent key must be 8 bytes, got 3", fmtRefusal(&buf, diag));
    try testing.expectError(error.BadExtentKey, decodeExtentKey(&(k1 ++ [_]u8{0}), &diag));
}

test "alloc delta payloads round-trip and garbage is refused with the verbatim text" {
    const key = extentKey(7);
    const a = try decodeAllocRecord(&key, &allocRecordValue(), null);
    try testing.expectEqual(@as(u64, 7), a.allocated);
    const f = try decodeAllocRecord(&key, &freeRecordValue(0xDEAD_BEEF), null);
    try testing.expectEqual(@as(u64, 7), f.freed.extent);
    try testing.expectEqual(@as(u64, 0xDEAD_BEEF), f.freed.retire_seq);
    try testing.expectEqualSlices(u8, &.{ 2, 0xEF, 0xBE, 0xAD, 0xDE, 0, 0, 0, 0 }, &freeRecordValue(0xDEAD_BEEF));
    var diag: DeltaRefusal = undefined;
    var buf: [128]u8 = undefined;
    try testing.expectError(error.MalformedDeltaValue, decodeAllocRecord(&key, &.{}, &diag));
    try testing.expectEqualStrings("malformed allocator delta value (0 bytes, tag None) for extent 7", fmtRefusal(&buf, diag));
    try testing.expectError(error.MalformedDeltaValue, decodeAllocRecord(&key, &.{99}, &diag));
    try testing.expectEqualStrings("malformed allocator delta value (1 bytes, tag Some(99)) for extent 7", fmtRefusal(&buf, diag));
    try testing.expectError(error.MalformedDeltaValue, decodeAllocRecord(&key, &.{ 2, 1, 2, 3 }, &diag));
    try testing.expectError(error.MalformedDeltaValue, decodeAllocRecord(&key, &.{ 1, 0 }, &diag));
    try testing.expectError(error.BadExtentKey, decodeAllocRecord(&.{ 0, 7 }, &.{1}, &diag));
}

test "bitmap page image: exact round-trip; wrong magic, index, checksum and an oversize payload are invalid" {
    var bits: [page_data_len]u8 = @splat(0);
    bits[0] = 0b1010_0001;
    bits[4071] = 0x80;
    var img: [page_len]u8 = undefined;
    try encodeBitmapPage(3, 42, &bits, &img);
    // ASCII order on disk.
    try testing.expectEqual(page_magic, std.mem.readInt(u32, img[0..4], .little));
    try testing.expectEqualStrings("KVAB", img[0..4]);
    try testing.expectEqual(@as(u32, 3), std.mem.readInt(u32, img[4..8], .little));
    try testing.expectEqual(@as(u64, 42), std.mem.readInt(u64, img[8..16], .little));
    const d = try decodeBitmapPage(&img, 3);
    try testing.expectEqual(@as(u64, 42), d.generation);
    try testing.expectEqualSlices(u8, &bits, d.bits);
    try testing.expectError(error.WrongIndex, decodeBitmapPage(&img, 4));
    try testing.expectError(error.Truncated, decodeBitmapPage(img[0..4095], 3));
    var bad = img;
    bad[0] ^= 1;
    try testing.expectError(error.BadMagic, decodeBitmapPage(&bad, 3));
    bad = img;
    bad[1000] ^= 1;
    try testing.expectError(error.ChecksumMismatch, decodeBitmapPage(&bad, 3));
    var big: [page_data_len + 1]u8 = @splat(0);
    try testing.expectError(error.PayloadTooLarge, encodeBitmapPage(0, 1, &big, &img));
    // A short payload is zero-padded to the data area.
    try encodeBitmapPage(0, 1, &.{0xFF}, &img);
    const short = try decodeBitmapPage(&img, 0);
    try testing.expectEqual(@as(u8, 0xFF), short.bits[0]);
    try testing.expectEqual(@as(u8, 0), short.bits[1]);
    try testing.expectEqual(@as(u64, 2), bitmapPagesFor(page_bits + 1));
    try testing.expectEqual(@as(u64, 8192), bitmapRegionLen(1));
    try testing.expectEqual(@as(u64, 1_056_768), bitmapRegionLen(4_194_304));
}

test "wrapper: typed ENOSPC names claimable and reserve; PendingFreeFull names the parked count; release_unpublished is reclaimable at once; advance returns the count" {
    var alloc = try ExtentAllocator.format(testing.allocator, 4, 2, 2);
    defer alloc.deinit();
    const a = try alloc.claimUser();
    const b = try alloc.claimUser();
    try testing.expectError(error.NoSpace, alloc.claimUser());
    const ns = alloc.noSpace();
    try testing.expectEqual(@as(u64, 2), ns.free);
    try testing.expectEqual(@as(u64, 2), ns.reserve);
    var buf: [256]u8 = undefined;
    const msg = try std.fmt.bufPrint(&buf, "{f}", .{ns});
    try testing.expect(std.mem.indexOf(u8, msg, "ENOSPC") != null);
    try testing.expectEqualStrings("no space: 2 free extents with the 2-extent compaction reserve intact — allocation refused (ENOSPC)", msg);
    try alloc.freePending(a, 4);
    try alloc.freePending(b, 5);
    const c = try alloc.claimInternal();
    try testing.expect(!alloc.pendingHasRoom());
    try testing.expectError(error.PendingFreeFull, alloc.freePending(c, 6));
    const full = alloc.pendingFreeFull();
    try testing.expectEqual(@as(u64, 2), full.pending);
    const fmsg = try std.fmt.bufPrint(&buf, "{f}", .{full});
    try testing.expectEqualStrings("pending-free list full (2 extents awaiting durable checkpoints); checkpoint required before further frees", fmsg);
    alloc.releaseUnpublished(c);
    try testing.expectEqual(c, try alloc.claimInternal());
    try testing.expectEqual(@as(u64, 2), alloc.advanceDurable(5));
    try testing.expectEqual(@as(u64, 0), alloc.pendingCount());
    try testing.expect(alloc.pendingHasRoomFor(2));
}

test "A/B alternation and newest-valid selection: gen 1 → A, gen 2 → B (A keeps 1), nothing dirty ⇒ [], load picks gen 2, resume 2, gen 3 → A" {
    const gpa = testing.allocator;
    const region = try gpa.alloc(u8, bitmapRegionLen(10));
    defer gpa.free(region);
    @memset(region, 0);
    var alloc = try ExtentAllocator.format(gpa, 10, 0, 4);
    defer alloc.deinit();
    const w1 = try alloc.writeDirtyPagesToRegion(region, 1);
    defer gpa.free(w1);
    try testing.expectEqualSlices(u32, &.{0}, w1);
    try testing.expectEqual(@as(u64, 1), (try decodeBitmapPage(region[0..page_len], 0)).generation);
    try testing.expectError(error.BadMagic, decodeBitmapPage(region[page_len .. 2 * page_len], 0));
    _ = try alloc.claimInternal();
    const w2 = try alloc.writeDirtyPagesToRegion(region, 2);
    defer gpa.free(w2);
    try testing.expectEqualSlices(u32, &.{0}, w2);
    try testing.expectEqual(@as(u64, 1), (try decodeBitmapPage(region[0..page_len], 0)).generation);
    try testing.expectEqual(@as(u64, 2), (try decodeBitmapPage(region[page_len .. 2 * page_len], 0)).generation);
    const none = try alloc.writeDirtyPagesToRegion(region, 3);
    defer gpa.free(none);
    try testing.expectEqual(@as(usize, 0), none.len);
    var loaded = try ExtentAllocator.load(gpa, region, 10, 0, 4, 0, &.{}, null);
    defer loaded.deinit();
    try testing.expectEqual(@as(u64, 2), loaded.resumeGeneration());
    try testing.expect(loaded.isAllocated(0));
    try testing.expectEqual(@as(u64, 9), loaded.freeExtents());
    try testing.expect(!loaded.hasDirtyPages());
    // The mount's checkpoint seq resumes above the newest page generation.
    try testing.expectEqual(@as(u64, 2), resumeCheckpointSeq(1, loaded.resumeGeneration()));
    try testing.expectEqual(@as(u64, 7), resumeCheckpointSeq(7, 2));
    _ = try loaded.claimInternal();
    const w3 = try loaded.writeDirtyPagesToRegion(region, 3);
    defer gpa.free(w3);
    try testing.expectEqual(@as(u64, 3), (try decodeBitmapPage(region[0..page_len], 0)).generation);
    try testing.expectEqual(@as(u64, 2), (try decodeBitmapPage(region[page_len .. 2 * page_len], 0)).generation);
}

test "multi-page dirty tracking: format persists [0,1]; a release at extent 32576 dirties only page 1; load matches" {
    const gpa = testing.allocator;
    const total = page_bits + 10;
    const region = try gpa.alloc(u8, bitmapRegionLen(total));
    defer gpa.free(region);
    @memset(region, 0);
    var alloc = try ExtentAllocator.format(gpa, total, 8, 4);
    defer alloc.deinit();
    const w1 = try alloc.writeDirtyPagesToRegion(region, 1);
    defer gpa.free(w1);
    try testing.expectEqualSlices(u32, &.{ 0, 1 }, w1);
    // Seed extent 32576 allocated (mount seeding), then release it.
    alloc.core.markAllocated(page_bits);
    alloc.releaseUnpublished(page_bits);
    const w2 = try alloc.writeDirtyPagesToRegion(region, 2);
    defer gpa.free(w2);
    try testing.expectEqualSlices(u32, &.{1}, w2);
    var loaded = try ExtentAllocator.load(gpa, region, total, 8, 4, 0, &.{}, null);
    defer loaded.deinit();
    try testing.expectEqual(total, loaded.freeExtents());
    try testing.expectEqual(@as(u64, 2), loaded.resumeGeneration());
}

test "a short region loads as all-free, generation 0" {
    var short: [100]u8 = @splat(0);
    var loaded = try ExtentAllocator.load(testing.allocator, &short, 1000, 20, 4, 0, &.{}, null);
    defer loaded.deinit();
    try testing.expectEqual(@as(u64, 1000), loaded.freeExtents());
    try testing.expectEqual(@as(u64, 0), loaded.resumeGeneration());
    try testing.expect(!loaded.hasDirtyPages());
}

test "journal replay rebuilds alloc, free and pending: non-zero-tag frees park only after park_replayed_frees; tag-0 chain folds to allocated; gates are positions" {
    const gpa = testing.allocator;
    const region = try gpa.alloc(u8, bitmapRegionLen(16));
    defer gpa.free(region);
    @memset(region, 0);
    // Pages at gen 1 with extents 0..4 allocated.
    {
        var fresh = try ExtentAllocator.format(gpa, 16, 0, 4);
        defer fresh.deinit();
        var i: usize = 0;
        while (i < 4) : (i += 1) _ = try fresh.claimInternal();
        const w = try fresh.writeDirtyPagesToRegion(region, 1);
        defer gpa.free(w);
    }
    // Window: entry 10 frees e0 (tag 9) and e1 (tag 9); entry 20 allocs e5,
    // frees e5 with tag 0, allocs e5 again (folds to Allocated); entry 30
    // frees e2 with tag 0 (reusable now).
    const k0 = extentKey(0);
    const k1 = extentKey(1);
    const k2 = extentKey(2);
    const k5 = extentKey(5);
    const fv9 = freeRecordValue(9);
    const fv0 = freeRecordValue(0);
    const av = allocRecordValue();
    const other = [_]u8{ 1, 2, 3 };
    const e10 = [_]ReplayRecord{
        .{ .tree_id = 1, .key = &other, .value = &other },
        .{ .tree_id = tree_alloc_reserved, .key = &k0, .value = &fv9 },
        .{ .tree_id = tree_alloc_reserved, .key = &k1, .value = &fv9 },
    };
    const e20 = [_]ReplayRecord{
        .{ .tree_id = tree_alloc_reserved, .key = &k5, .value = &av },
        .{ .tree_id = tree_alloc_reserved, .key = &k5, .value = &fv0 },
        .{ .tree_id = tree_alloc_reserved, .key = &k5, .value = &av },
    };
    const e30 = [_]ReplayRecord{.{ .tree_id = tree_alloc_reserved, .key = &k2, .value = &fv0 }};
    const window = [_]ReplayEntry{ .{ .seq = 10, .records = &e10 }, .{ .seq = 20, .records = &e20 }, .{ .seq = 30, .records = &e30 } };
    var alloc = try ExtentAllocator.load(gpa, region, 16, 0, 4, 10, &window, null);
    defer alloc.deinit();
    // Frees with a non-zero tag keep their bits; e2 (tag 0) is free; e5 allocated.
    try testing.expect(alloc.isAllocated(0));
    try testing.expect(alloc.isAllocated(1));
    try testing.expect(!alloc.isAllocated(2));
    try testing.expect(alloc.isAllocated(3));
    try testing.expect(alloc.isAllocated(5));
    try testing.expectEqual(@as(u64, 0), alloc.pendingCount());
    try testing.expect(alloc.hasDirtyPages());
    try testing.expectEqual(@as(u64, 10), alloc.durableSeq());
    const parked = try alloc.parkReplayedFrees(&.{});
    try testing.expectEqual(@as(u64, 2), parked.parked);
    try testing.expectEqual(@as(u64, 0), parked.dropped);
    try testing.expectEqual(@as(u64, 2), alloc.pendingCount());
    // Parked extents are never claimable: 16 − {0,1,3,5 bits} − pending ⇒ 12 free.
    try testing.expectEqual(@as(u64, 12), alloc.freeExtents());
    var n: usize = 0;
    while (n < 12) : (n += 1) {
        const e = try alloc.claimInternal();
        try testing.expect(e != 0 and e != 1);
    }
    try testing.expectError(error.NoSpace, alloc.claimInternal());
    // e0's free is record 1 of entry 10 ⇒ position 11; e1's ⇒ 12.
    try testing.expectEqual(@as(u64, 1), alloc.advanceDurable(11));
    try testing.expectEqual(@as(u64, 0), try alloc.claimInternal());
    try testing.expectEqual(@as(u64, 1), alloc.advanceDurable(12));
    try testing.expectEqual(@as(u64, 1), try alloc.claimInternal());
    try testing.expectEqual(@as(u64, 0), alloc.pendingCount());
}

test "park_replayed_frees drops a free naming a live root (counted) and parks the rest forced" {
    const gpa = testing.allocator;
    var region: [8192]u8 = @splat(0);
    const k3 = extentKey(3);
    const k4 = extentKey(4);
    const k6 = extentKey(6);
    const fv = freeRecordValue(2);
    const av = allocRecordValue();
    const e5 = [_]ReplayRecord{
        .{ .tree_id = tree_alloc_reserved, .key = &k3, .value = &av },
        .{ .tree_id = tree_alloc_reserved, .key = &k4, .value = &av },
        .{ .tree_id = tree_alloc_reserved, .key = &k6, .value = &av },
    };
    const e8 = [_]ReplayRecord{
        .{ .tree_id = tree_alloc_reserved, .key = &k3, .value = &fv },
        .{ .tree_id = tree_alloc_reserved, .key = &k4, .value = &fv },
        .{ .tree_id = tree_alloc_reserved, .key = &k6, .value = &fv },
    };
    const window = [_]ReplayEntry{ .{ .seq = 5, .records = &e5 }, .{ .seq = 8, .records = &e8 } };
    // Cap 2: the third replayed park must overflow, never refuse.
    var alloc = try ExtentAllocator.load(gpa, &region, 8, 0, 2, 5, &window, null);
    defer alloc.deinit();
    const before_dropped = meta_kv_replay_root_frees_dropped.load();
    const before_overflow = meta_kv_pending_free_overflow.load();
    const out = try alloc.parkReplayedFrees(&.{4});
    try testing.expectEqual(@as(u64, 2), out.parked);
    try testing.expectEqual(@as(u64, 1), out.dropped);
    try testing.expectEqual(before_dropped + 1, meta_kv_replay_root_frees_dropped.load());
    try testing.expectEqual(before_overflow, meta_kv_pending_free_overflow.load());
    try testing.expect(alloc.isAllocated(4));
    try testing.expectEqual(@as(u64, 2), alloc.pendingCount());
    // A second park is a no-op (the list was drained).
    const again = try alloc.parkReplayedFrees(&.{});
    try testing.expectEqual(@as(u64, 0), again.parked);
    // Positions 8, 10 (records 0 and 2 of entry 8): tail 9 covers only e3.
    try testing.expectEqual(@as(u64, 1), alloc.advanceDurable(9));
    try testing.expect(!alloc.isAllocated(3));
    try testing.expect(alloc.isAllocated(6));
    try testing.expectEqual(@as(u64, 1), alloc.advanceDurable(10));
}

test "adoptLive: a free extent a named root reaches turns allocated once (counted, its page dirty, never claimed after); an allocated one is untouched" {
    const gpa = testing.allocator;
    var region: [8192]u8 = @splat(0);
    var alloc = try ExtentAllocator.load(gpa, &region, 8, 0, 2, 0, &.{}, null);
    defer alloc.deinit();
    try testing.expect(!alloc.hasDirtyPages());
    const before = meta_kv_replay_root_claims_adopted.load();
    try testing.expect(alloc.adoptLive(0));
    try testing.expect(!alloc.adoptLive(0));
    try testing.expectEqual(before + 1, meta_kv_replay_root_claims_adopted.load());
    try testing.expect(alloc.isAllocated(0));
    try testing.expect(alloc.hasDirtyPages());
    try testing.expectEqual(@as(u64, 7), alloc.freeExtents());
    for (0..7) |_| try testing.expect(try alloc.claimInternal() != 0);
    try testing.expectError(error.NoSpace, alloc.claimInternal());
}

test "replayed parks beyond the FIFO cap overflow (counted, warned) instead of refusing the mount" {
    const gpa = testing.allocator;
    var region: [8192]u8 = @splat(0);
    const k1 = extentKey(1);
    const k2 = extentKey(2);
    const k3 = extentKey(3);
    const fv = freeRecordValue(1);
    const recs = [_]ReplayRecord{
        .{ .tree_id = tree_alloc_reserved, .key = &k1, .value = &fv },
        .{ .tree_id = tree_alloc_reserved, .key = &k2, .value = &fv },
        .{ .tree_id = tree_alloc_reserved, .key = &k3, .value = &fv },
    };
    const window = [_]ReplayEntry{.{ .seq = 4, .records = &recs }};
    var alloc = try ExtentAllocator.load(gpa, &region, 8, 0, 2, 4, &window, null);
    defer alloc.deinit();
    const before = meta_kv_pending_free_overflow.load();
    const out = try alloc.parkReplayedFrees(&.{});
    try testing.expectEqual(@as(u64, 3), out.parked);
    try testing.expectEqual(before + 1, meta_kv_pending_free_overflow.load());
    try testing.expectEqual(@as(u64, 3), alloc.pendingCount());
    try testing.expectEqual(@as(u64, 3), alloc.advanceDurable(6));
}

test "load refuses a malformed replayed delta loud (a 16 B key under kind 4 included: the data bitmap's deltas are kind 5) and ignores other kinds" {
    const gpa = testing.allocator;
    var region: [8192]u8 = @splat(0);
    const k1 = extentKey(1);
    const bad = [_]u8{ 9, 9 };
    const malformed = [_]ReplayRecord{.{ .tree_id = tree_alloc_reserved, .key = &k1, .value = &bad }};
    var diag: LoadRefusal = undefined;
    try testing.expectError(error.MalformedDelta, ExtentAllocator.load(gpa, &region, 8, 0, 2, 0, &.{.{ .seq = 1, .records = &malformed }}, &diag));
    var buf: [256]u8 = undefined;
    try testing.expectEqualStrings("malformed allocator delta value (2 bytes, tag Some(9)) for extent 1", fmtRefusal(&buf, diag));
    const data_key: [16]u8 = @splat(0xAB);
    const wide = [_]ReplayRecord{.{ .tree_id = tree_alloc_reserved, .key = &data_key, .value = &allocRecordValue() }};
    try testing.expectError(error.MalformedDelta, ExtentAllocator.load(gpa, &region, 8, 0, 2, 0, &.{.{ .seq = 1, .records = &wide }}, &diag));
    try testing.expectEqualStrings("extent key must be 8 bytes, got 16", fmtRefusal(&buf, diag));
    // Kind 1 garbage and a kind-5 (data-alloc) record are both ignored.
    const key = @import("key.zig");
    const ignored = [_]ReplayRecord{
        .{ .tree_id = 1, .key = &k1, .value = &bad },
        .{ .tree_id = key.TREE_DATA_ALLOC_RESERVED, .key = &data_key, .value = &bad },
    };
    var ok = try ExtentAllocator.load(gpa, &region, 8, 0, 2, 0, &.{.{ .seq = 1, .records = &ignored }}, &diag);
    defer ok.deinit();
    try testing.expectEqual(@as(u64, 8), ok.freeExtents());
}

test "DUR-4: abort restores the dirty set (law 1); a retry at a stale generation is raised above the newest copy (law 2) and counted" {
    const gpa = testing.allocator;
    var region: [8192]u8 = @splat(0);
    var alloc = try ExtentAllocator.format(gpa, 8, 0, 2);
    defer alloc.deinit();
    var set = try alloc.beginDirtyPageWrite(5);
    try testing.expect(!alloc.hasDirtyPages());
    try testing.expectEqual(@as(usize, 1), set.writes.len);
    set.abort(&alloc);
    set.deinit();
    try testing.expect(alloc.hasDirtyPages());
    var set2 = try alloc.beginDirtyPageWrite(5);
    set2.applyToRegion(&alloc, &region);
    set2.deinit();
    try testing.expectEqual(@as(u64, 5), alloc.resumeGeneration());
    // The retry arrives with the same seq: raised to 6, into the other slot.
    _ = try alloc.claimInternal();
    const before = meta_kv_bitmap_generation_raises.load();
    var set3 = try alloc.beginDirtyPageWrite(5);
    try testing.expectEqual(@as(u64, 6), set3.writes[0].generation);
    try testing.expectEqual(@as(u8, 1), set3.writes[0].target_slot);
    try testing.expectEqual(@as(u64, page_len), set3.writes[0].offset);
    try testing.expectEqual(before + 1, meta_kv_bitmap_generation_raises.load());
    set3.applyToRegion(&alloc, &region);
    set3.deinit();
    try testing.expectEqual(@as(u64, 6), (try decodeBitmapPage(region[page_len..], 0)).generation);
    // An empty set is settled on arrival.
    var empty = try alloc.beginDirtyPageWrite(7);
    try testing.expectEqual(@as(usize, 0), empty.writes.len);
    empty.deinit();
}

test "invariant drift surfaces as a loud NoSpace at the wrapper" {
    var alloc = try ExtentAllocator.format(testing.allocator, 2, 0, 2);
    defer alloc.deinit();
    _ = try alloc.claimInternal();
    _ = try alloc.claimInternal();
    alloc.core.inflateFreeBudgetForTest(1);
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.NoSpace, alloc.claimInternal());
    try log.testing.expectAllFired();
}

fn allocRecordsFuzz(_: void, smith: *std.testing.Smith) !void {
    var key: [16]u8 = undefined;
    var value: [16]u8 = undefined;
    var page: [page_len + 8]u8 = undefined;
    const klen = smith.slice(&key);
    const vlen = smith.slice(&value);
    const plen = smith.slice(&page);
    var diag: DeltaRefusal = undefined;
    _ = decodeExtentKey(key[0..klen], &diag) catch {};
    _ = decodeAllocRecord(key[0..klen], value[0..vlen], &diag) catch {};
    _ = decodeBitmapPage(page[0..plen], 0) catch {};
    if (plen >= page_len) {
        const img = page[0..page_len];
        const index = std.mem.readInt(u32, img[off_page_index..][0..4], .little);
        checksum.stamp(img, off_checksum);
        if (decodeBitmapPage(img, index)) |d| {
            var re: [page_len]u8 = undefined;
            try encodeBitmapPage(index, d.generation, d.bits, &re);
            try std.testing.expectEqualSlices(u8, img, &re);
        } else |_| {}
    }
}

test "fuzz: the extent key, alloc record and bitmap page decoders are total; a restamped page that decodes re-encodes byte-identically" {
    try fuzzkit.fuzz({}, allocRecordsFuzz, .{});
}
