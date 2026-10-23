//! The rewrite epoch (charter §6.2 item 3 "Inside a rewrite epoch (per
//! inode): displaced A keys park, B offsets hold in-flight guards, ONE
//! whole-tx save swaps the epoch at close … parked A keys are freed only
//! after a durable save no longer references them; a fence at close
//! publishes nothing and frees nothing"; `docs/porting/data_write_path.md`
//! §5.4). PORTABLE: a per-ino RAM record under its own spin lock; the ONE
//! save is the map authority's `merge` (one tx) run by `close`.
//!
//! Close triggers (KD-1.6): full coverage (`shadow.len × bs ≥ size`), the
//! shadow at the save's close width (`CLOSE_WIDTH`, below), fsync / flush
//! (before the meta barrier), RELEASE, shape-change ops (truncate, punch,
//! spill/promotion, clone, setattr size), the ENOSPC early-close generalized
//! to the one allocation act (`closeAllForSpace`, the
//! `enospc.PressureSupplyHook`), the write-back timer within `T_wb`, idle
//! `EPOCH_IDLE_HORIZON_MS`, fence / unmount. Supersession coherence
//! (KD-1.11): every durable publish of a shadowed index evicts `shadow[b]`
//! (`supersede`) — the 2026-08-04 C2Lost/C2Leaked/C3 resurrection fix. The
//! epoch-close venue law: `close` is never called under a block guard (3) —
//! the guard-dropping caller acts on the close-owed hint.
//!
//! A FRESH block rides the epoch too (`prev == null`: nothing parked, the
//! save names it for the first time — `docs/evidence/rung-10-fresh-file-
//! publish.md`). The reference fed only overwrites (its epoch exists to park
//! displaced keys; a fresh record took the per-block durable publish), so a
//! file laid down for the first time journaled its whole inline map once per
//! block: 768 records of up to 37 KiB for a 3 GiB file. The save is the
//! coalescing unit for both: one publish per close width, the layout record
//! written once per window instead of once per block. The durability law is
//! unchanged — an acked write lands within `T_wb` (decision 0003 §AJ) or at
//! fsync, whole or absent, and the size claim journals only up to the
//! custody floor (`write_admit.custodyFloor`: 0 while an epoch holds the
//! ino), so the durable size never names a block the save has not landed.
//!
//! The save's visibility law (the 2026-10-07 lost overwrite): an epoch
//! whose ONE save is in flight stays VISIBLE — `bindsBlock` keeps W1 and
//! the overlay off every block it shadows, `shadowOf` keeps serving its B
//! keys — until the save landed (or dropped). Taking it out of sight first
//! opened a window in which W1 patched the A key the save was about to
//! displace (the patch vanished under the swap) and a park captured A as
//! its base (the next flush seeded the complement from A, losing the bytes
//! only B held; the next save then displaced a B it never parked and freed
//! A twice — `rewrite_shadow_displaced_drift`, `block_untracked_free_
//! refusals`). A record landing during the save opens the NEXT epoch over
//! the closing shadow (its displaced key is the closing B, never the map's
//! stale A), and closes of one ino serialize in epoch order
//! (`close_settled`): the saves land B then C, each displacing exactly what
//! it parked. `write_epoch_model.zig` proves both weakenings fail.

const std = @import("std");
const Io = std.Io;
const Atomic = std.atomic.Value;
const Allocator = std.mem.Allocator;

const counters = @import("../core/counters.zig");
const clock = @import("../core/clock.zig");
const log = @import("../core/log.zig");
const jc = @import("../meta/journal_core.zig");
const sync_notify = @import("../sync/notify.zig");
const seams = @import("write_seams.zig");
const block_alloc = @import("block_alloc.zig");

pub const Binding = seams.Binding;

/// The metadata cache's time-to-idle: 300 s (the reference's
/// `METADATA_CACHE_TTI_SECS`) — an idle epoch is closed at a tenth of it so
/// a shadowed RAM entry is never evicted mid-epoch (the eviction horizon the
/// close defends).
pub const METADATA_CACHE_TTI_MS: u64 = 300 * std.time.ms_per_s;
pub const EPOCH_IDLE_HORIZON_MS: u64 = METADATA_CACHE_TTI_MS / 10;

/// The close width: the distinct block whose record owes the close (the
/// `close_owed` hint, acted on by the guard-dropping caller) — HALF the ONE
/// save's width (`MergeOutcome.max_displaced`, derived from the journal's
/// entry cap). Half, not the width itself: a stream keeps recording while
/// the closed epoch's save is in flight, and `record` bounds the open
/// shadow UNITED with the saving one at the width (a transient save folds
/// the two — `reregister` — and the fold must fit one save), so an epoch
/// closed at the full width would park every record behind its save
/// (`save_in_flight`) and one closed at half leaves the next epoch the
/// other half: the save lands within the time the stream takes to fill it
/// (a few ms against ~40 ms at 2,800 blocks/s per file). Before this hint
/// the timer's cadence alone closed a streaming epoch, so a stream faster
/// than the width per cadence (213 blocks per 50–78 ms) paid the per-block
/// publish for the rest of the window (`rewrite_shadow_fallbacks`: 7.8 % of
/// the blocks of a 47 GB/s overwrite).
pub const CLOSE_WIDTH: usize = seams.MergeOutcome.max_displaced / 2;

comptime {
    // Two width-closed epochs fold into one save.
    std.debug.assert(2 * CLOSE_WIDTH <= seams.MergeOutcome.max_displaced);
    std.debug.assert(CLOSE_WIDTH >= 2);
}

pub const stats = struct {
    pub var rewrite_shadow_open_epochs: counters.Gauge = .{};
    pub var rewrite_shadow_parked_bytes: counters.Gauge = .{};
    pub var rewrite_shadow_swaps: counters.ShardedCounter = .{};
    pub var rewrite_shadow_bytes: counters.ShardedCounter = .{};
    pub var rewrite_shadow_fallbacks: counters.ShardedCounter = .{};
    /// Records with no key to displace: a fresh block named by its save for
    /// the first time (module doc).
    pub var rewrite_shadow_fresh_records: counters.ShardedCounter = .{};
    /// Records that owed the close at `CLOSE_WIDTH`.
    pub var rewrite_shadow_width_hints: counters.ShardedCounter = .{};
    pub var rewrite_shadow_close_retries: counters.ShardedCounter = .{};
    /// A close that parked on the ino's running save (closes serialize).
    pub var rewrite_shadow_close_waits: counters.ShardedCounter = .{};
    /// A record answered `save_in_flight` and its caller parked on the
    /// ino's running save before recording again.
    pub var rewrite_shadow_save_waits: counters.ShardedCounter = .{};
    pub var rewrite_shadow_superseded: counters.ShardedCounter = .{};
    pub var rewrite_shadow_pressure_closes: counters.ShardedCounter = .{};
    pub var rewrite_shadow_orphan_discards: counters.ShardedCounter = .{};
    pub var rewrite_shadow_idle_closes: counters.ShardedCounter = .{};
    /// The `rewrite_amp` SLO: `device_write_bytes / user_bytes`.
    pub var rewrite_blocks: counters.ShardedCounter = .{};
    pub var rewrite_user_bytes: counters.ShardedCounter = .{};
    pub var rewrite_device_write_bytes: counters.ShardedCounter = .{};
    /// A close fenced while the volume was FAILED (or a device fenced):
    /// the fail-stop working, never a tripwire (owner ruling 2026-10-19).
    pub var rewrite_shadow_fence_drops_failed_volume: counters.ShardedCounter = .{};
    /// Must-stay-0 (`rewrite_shadow_fence_drops`: a close fenced on a HEALTHY volume).
    pub var rewrite_shadow_fence_drops: counters.Tripwire = .init("rewrite_shadow_fence_drops");
    pub var rewrite_shadow_displaced_drift: counters.Tripwire = .init("rewrite_shadow_displaced_drift");
    pub var rewrite_shadow_save_overflow: counters.Tripwire = .init("rewrite_shadow_save_overflow");
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"rewrite_shadow_open_epochs\":{d},\"rewrite_shadow_parked_bytes\":{d},", .{ stats.rewrite_shadow_open_epochs.load(), stats.rewrite_shadow_parked_bytes.load() });
    inline for (.{ "rewrite_shadow_swaps", "rewrite_shadow_bytes", "rewrite_shadow_fallbacks", "rewrite_shadow_fresh_records", "rewrite_shadow_width_hints", "rewrite_shadow_close_retries", "rewrite_shadow_close_waits", "rewrite_shadow_save_waits", "rewrite_shadow_superseded", "rewrite_shadow_pressure_closes", "rewrite_shadow_orphan_discards", "rewrite_shadow_idle_closes", "rewrite_blocks", "rewrite_user_bytes", "rewrite_device_write_bytes", "rewrite_shadow_fence_drops_failed_volume" }) |name| {
        try w.print("\"{s}\":{d},", .{ name, @field(stats, name).load() });
    }
    try w.print("\"rewrite_shadow_fence_drops\":{d},\"rewrite_shadow_displaced_drift\":{d},\"rewrite_shadow_save_overflow\":{d}}}", .{ counters.tripwireCount("rewrite_shadow_fence_drops"), counters.tripwireCount("rewrite_shadow_displaced_drift"), counters.tripwireCount("rewrite_shadow_save_overflow") });
}

const Shadowed = struct {
    binding: Binding,
    proof: seams.DmaProof,
};

/// A parked key: the map's A (the durable publisher at close displaces
/// and frees it) or an intermediate B of ours (an unpublished mint).
const Parked = struct {
    block: u32,
    origin: enum(u8) { map, epoch },
    binding: Binding,
};

pub const Epoch = struct {
    ino: u64,
    /// b → the newest B key (the refetch-compose source, KD-1.9).
    shadow: std.AutoHashMapUnmanaged(u32, Shadowed) = .empty,
    /// Parked A keys (and superseded intermediate B keys).
    displaced: std.ArrayList(Parked) = .empty,
    /// B offsets' live-owner registrations — THE fsck C2/C3 exemption hook.
    guards: std.ArrayList(block_alloc.InflightAllocGuard) = .empty,
    recorded_bytes: u64 = 0,
    parked_bytes: u64 = 0,
    min_size: u64 = 0,
    last_record_ns: u64 = 0,

    fn shadowBinding(self: *const Epoch, block: u32) ?Binding {
        const s = self.shadow.get(block) orelse return null;
        return s.binding;
    }

    fn deinit(self: *Epoch, gpa: Allocator) void {
        self.shadow.deinit(gpa);
        self.displaced.deinit(gpa);
        for (self.guards.items) |*g| g.deinit();
        self.guards.deinit(gpa);
    }
};

fn shadows(e: ?*const Epoch, block: u32) bool {
    return if (e) |x| x.shadow.contains(block) else false;
}

/// The blocks a fold of `open` and `closing` shadows — what its ONE save
/// carries. At most `max_displaced` lookups: the closing shadow is capped too.
fn shadowUnion(open: ?*const Epoch, closing: ?*const Epoch) usize {
    var n: usize = if (open) |e| e.shadow.count() else 0;
    const c = closing orelse return n;
    var it = c.shadow.keyIterator();
    while (it.next()) |b| {
        if (!shadows(open, b.*)) n += 1;
    }
    return n;
}

pub const RecordVerdict = union(enum) {
    /// `close_owed`: the close-owed hint for the guard-dropping caller —
    /// the shadow covers the file (KD-1.6) or this record is the epoch's
    /// `CLOSE_WIDTH`-th distinct block (once per epoch: the record that
    /// reaches the width, so one close call per window, never one per
    /// block past it).
    shadowed: struct { displaced_prev: ?Binding, close_owed: bool },
    /// The ino's layout is not striped (a promotion in flight), or the
    /// epoch already shadows `MergeOutcome.max_displaced` other blocks (the
    /// ONE save's size) and no save is in flight: the caller takes the
    /// durable-merge arm.
    not_shadowed,
    /// The open epoch's shadow united with the ino's CLOSING epoch's is at
    /// the ONE save's size and neither shadows this block: a transient save
    /// folds the two into one epoch whose close must fit that save. The
    /// caller publishes nothing: it awaits the save (`awaitSaveSettled`,
    /// never under (3)) and records again — over the landed B, or into the
    /// folded epoch. (A block the closing epoch shadows is always taken: a
    /// durable merge under that save made it displace a key it never parked
    /// and free the parked one a second time — `rewrite_shadow_displaced_
    /// drift` + `block_untracked_free_refusals`: fstests generic/247, 500
    /// blocks overwritten by O_DIRECT and mmap at once.)
    save_in_flight,
};

pub const CloseOutcome = union(enum) {
    absent,
    /// The ONE save landed: `bytes` recorded, `freed` displaced keys returned.
    swapped: struct { bytes: u64, freed: u32 },
    /// W5: publish nothing, free nothing.
    fence_dropped,
    /// Re-registered never-lossy; the next trigger retries.
    transient,
    /// The ino is gone: the B keys were orphans (freed), the A keys belong
    /// to the destroy.
    orphan_discarded,
};

pub const Epochs = struct {
    gpa: Allocator,
    block_size: u64,
    lock: jc.SpinLock = .{},
    /// The OPEN epochs (accepting records), one per ino.
    table: std.AutoHashMapUnmanaged(u64, *Epoch) = .empty,
    /// The epochs whose ONE save is in flight, one per ino: still visible
    /// to `bindsBlock`/`shadowOf` until the save landed or dropped (module
    /// doc). A second close of the ino parks on `close_settled`.
    closing: std.AutoHashMapUnmanaged(u64, *Epoch) = .empty,
    close_settled: sync_notify.Notify = .init,
    /// The lever: `SQUEEZEFS_REWRITE_SHADOW` (off ⇒ every overwrite takes
    /// the per-block durable publish — `rewrite_shadow_fallbacks`).
    enabled: bool,

    pub fn init(gpa: Allocator, block_size: u64, enabled: bool) Epochs {
        return .{ .gpa = gpa, .block_size = block_size, .enabled = enabled };
    }

    pub fn deinit(self: *Epochs) void {
        for ([_]*std.AutoHashMapUnmanaged(u64, *Epoch){ &self.table, &self.closing }) |t| {
            var it = t.valueIterator();
            while (it.next()) |e| {
                stats.rewrite_shadow_parked_bytes.sub(e.*.parked_bytes);
                stats.rewrite_shadow_open_epochs.sub(1);
                e.*.deinit(self.gpa);
                self.gpa.destroy(e.*);
            }
            t.deinit(self.gpa);
        }
        self.* = undefined;
    }

    /// The syncfs census: the ino of every open epoch and of every save in
    /// flight (`close` of the latter waits it out) appended to `out`. The
    /// maps grow without bound, so the room is reserved OUTSIDE the lock at
    /// the count last read and the fill re-checks it under the lock — maps
    /// that grew in between are re-reserved (never an allocation under the
    /// spin lock, never a truncated census).
    pub fn censusInos(self: *Epochs, gpa: Allocator, out: *std.ArrayList(u64)) Allocator.Error!void {
        var want: usize = 0;
        while (true) {
            try out.ensureUnusedCapacity(gpa, want);
            self.lock.lock();
            defer self.lock.unlock();
            const n = self.table.count() + self.closing.count();
            if (n > want) {
                want = n;
                continue;
            }
            for ([_]*std.AutoHashMapUnmanaged(u64, *Epoch){ &self.table, &self.closing }) |t| {
                var it = t.keyIterator();
                while (it.next()) |k| out.appendAssumeCapacity(k.*);
            }
            return;
        }
    }

    /// Whether `ino` has an epoch open or saving — the no-open FLUSH's
    /// try-only probe (decision 0003 §AF): one spin-locked lookup per map.
    pub fn holds(self: *Epochs, ino: u64) bool {
        self.lock.lock();
        defer self.lock.unlock();
        return self.table.contains(ino) or self.closing.contains(ino);
    }

    /// The lowest block `ino`'s epochs shadow — open or saving (the
    /// ordered law's floor, `write_admit.custodyFloor`): a shadowed block
    /// is named by no durable map until its save lands, so the size claim
    /// stops below it. One walk of at most two shadows under the spin lock.
    pub fn lowestBlockOf(self: *Epochs, ino: u64) ?u32 {
        self.lock.lock();
        defer self.lock.unlock();
        var low: ?u32 = null;
        for ([_]?*Epoch{ self.table.get(ino), self.closing.get(ino) }) |e| {
            const ep = e orelse continue;
            var it = ep.shadow.keyIterator();
            while (it.next()) |b| if (low == null or b.* < low.?) {
                low = b.*;
            };
        }
        return low;
    }

    /// Open epochs (a save in flight is not open: its ino's next record
    /// opens a fresh one).
    pub fn openCount(self: *Epochs) usize {
        self.lock.lock();
        defer self.lock.unlock();
        return self.table.count();
    }

    /// Hazard 1: an overlay install or a W1 patch on a block an epoch
    /// shadows — open OR saving — declines (module doc: the save's
    /// visibility law).
    pub fn bindsBlock(self: *Epochs, ino: u64, block: u32) bool {
        self.lock.lock();
        defer self.lock.unlock();
        return self.shadowLocked(ino, block) != null;
    }

    /// The newest B key of `(ino, block)` — the read path's compose source
    /// and the base an overwrite's complement is seeded from: the open
    /// epoch's, else the saving epoch's (what the map binds once it lands).
    pub fn shadowOf(self: *Epochs, ino: u64, block: u32) ?Binding {
        self.lock.lock();
        defer self.lock.unlock();
        return self.shadowLocked(ino, block);
    }

    fn shadowLocked(self: *Epochs, ino: u64, block: u32) ?Binding {
        if (self.table.get(ino)) |e| if (e.shadowBinding(block)) |b| return b;
        if (self.closing.get(ino)) |e| if (e.shadowBinding(block)) |b| return b;
        return null;
    }

    /// The write-side authority's binding for `(ino, block)`: the newest
    /// shadow else the durable map's — what an overwrite's complement is
    /// seeded from and what its feed displaces. Read under the block guard.
    pub fn baseBinding(self: *Epochs, map: seams.MapSeam, ino: u64, block: u32) ?Binding {
        return self.shadowOf(ino, block) orelse map.lookup(ino, block);
    }

    /// `rewrite_shadow_record`: open on the first record (KD-1.1: striped ∧
    /// prev ≠ new), park the displaced previous (purged from the read tiers
    /// by the caller), take the B offset's guard, note the size floor.
    /// `prev` is the map's binding the caller read under its block guard;
    /// a shadow outranks it: the open epoch's own newest B (an intra-epoch
    /// rewrite), else the saving epoch's B (the key the running save
    /// publishes — parked as the map's: our save displaces it; a save that
    /// re-registers folds it back as our orphan mint).
    pub fn record(self: *Epochs, ino: u64, block: u32, prev: ?Binding, new: Binding, proof: seams.DmaProof, min_size: u64, guard: block_alloc.InflightAllocGuard, striped: bool) Allocator.Error!RecordVerdict {
        if (!self.enabled or !striped) {
            var g = guard;
            g.deinit();
            return .not_shadowed;
        }
        self.lock.lock();
        defer self.lock.unlock();
        const open = self.table.get(ino);
        const closing = self.closing.get(ino);
        // The ONE save publishes at most `max_displaced` blocks, and a
        // transient save folds the closing epoch's shadow into the open one
        // (`reregister`): the cap bounds their UNION, so every close — a
        // fold's included — fits its save by construction. A block past it
        // awaits the save in flight (it lands, or folds within the cap) and
        // records again; with none in flight it takes the per-block durable
        // publish instead of being dropped from the save. A block the
        // closing epoch shadows never grows the union, so it is always
        // taken: no durable merge lands under that save (the visibility law).
        if (!shadows(open, block) and !shadows(closing, block) and shadowUnion(open, closing) >= seams.MergeOutcome.max_displaced) {
            var g = guard;
            g.deinit();
            if (closing != null) return .save_in_flight;
            stats.rewrite_shadow_fallbacks.inc();
            return .not_shadowed;
        }
        // Every reservation precedes the first mutation: a refusal registers
        // nothing (an empty epoch left registered lacked the fold's room).
        // A table slot stays free for every closing epoch: a transient save
        // re-registers with no allocation (`reregister`).
        try self.table.ensureUnusedCapacity(self.gpa, self.closing.count() + 1);
        const e = open orelse blk: {
            const fresh = try self.gpa.create(Epoch);
            fresh.* = .{ .ino = ino };
            break :blk fresh;
        };
        errdefer if (open == null) {
            e.deinit(self.gpa);
            self.gpa.destroy(e);
        };
        // Under a closing epoch this one keeps room for the closing one's
        // records past its own next: a transient save folds in with no
        // allocation (`reregister`).
        const fold: struct { shadow: u32, displaced: usize, guards: usize } = if (closing) |c| .{ .shadow = c.shadow.count(), .displaced = c.displaced.items.len, .guards = c.guards.items.len } else .{ .shadow = 0, .displaced = 0, .guards = 0 };
        try e.displaced.ensureUnusedCapacity(self.gpa, fold.displaced + 1);
        try e.shadow.ensureUnusedCapacity(self.gpa, fold.shadow + 1);
        try e.guards.ensureUnusedCapacity(self.gpa, fold.guards + 1);
        if (open == null) {
            self.table.putAssumeCapacityNoClobber(ino, e);
            stats.rewrite_shadow_open_epochs.add(1);
        }
        const own_prev = e.shadowBinding(block);
        const closing_prev: ?Binding = if (own_prev == null) (if (closing) |c| c.shadowBinding(block) else null) else null;
        const displaced_prev: ?Binding = own_prev orelse closing_prev orelse prev;
        if (displaced_prev) |d| {
            e.displaced.appendAssumeCapacity(.{ .block = block, .origin = if (own_prev != null) .epoch else .map, .binding = d });
            e.parked_bytes += self.block_size;
            stats.rewrite_shadow_parked_bytes.add(self.block_size);
        } else stats.rewrite_shadow_fresh_records.inc();
        e.shadow.putAssumeCapacity(block, .{ .binding = new, .proof = proof });
        e.guards.appendAssumeCapacity(guard);
        e.recorded_bytes += self.block_size;
        e.min_size = @max(e.min_size, min_size);
        e.last_record_ns = clock.monotonicNs();
        const coverage_complete = e.shadow.count() * self.block_size >= min_size and min_size != 0;
        // A distinct block grew the shadow (`own_prev == null`): the one
        // that reaches the close width owes the close.
        const at_width = own_prev == null and e.shadow.count() == CLOSE_WIDTH;
        if (at_width) stats.rewrite_shadow_width_hints.inc();
        return .{ .shadowed = .{ .displaced_prev = displaced_prev, .close_owed = coverage_complete or at_width } };
    }

    /// KD-1.11: a durable publish of `(ino, block)` → `published` outside
    /// the epoch evicts the shadow binding and drops the block's parked
    /// keys: the map's A is the publisher's to free; our own keys (the
    /// shadow's B when it is not what landed, intermediate Bs) are
    /// unpublished mints the caller frees — returned in `orphans`, freed
    /// with no lock held. The epoch parks one intermediate B per rewrite of
    /// the block, so their count has no bound: the ones past the batch stay
    /// parked (under their guards) and the epoch's close frees them with the
    /// rest of its parked set.
    pub fn supersede(self: *Epochs, ino: u64, block: u32, published: Binding, orphans: *[8]Binding) []Binding {
        self.lock.lock();
        defer self.lock.unlock();
        var n: usize = 0;
        const e = self.table.get(ino) orelse return orphans[0..0];
        if (e.shadow.fetchRemove(block)) |kv| {
            stats.rewrite_shadow_superseded.inc();
            if (!kv.value.binding.eql(published)) {
                orphans[0] = kv.value.binding;
                n = 1;
            }
        }
        var i: usize = 0;
        while (i < e.displaced.items.len) {
            const p = e.displaced.items[i];
            if (p.block != block) {
                i += 1;
                continue;
            }
            if (p.origin == .epoch) {
                if (n == orphans.len) {
                    i += 1;
                    continue;
                }
                orphans[n] = p.binding;
                n += 1;
            }
            _ = e.displaced.swapRemove(i);
            e.parked_bytes -= self.block_size;
            stats.rewrite_shadow_parked_bytes.sub(self.block_size);
        }
        return orphans[0..n];
    }

    pub const CloseSeams = struct {
        map: seams.MapSeam,
        free: seams.FreeSeam,
        purge: seams.PurgeSeam,
        /// The volume's FAILED/fenced probe the close's fence drop is
        /// classified on (`seams.classifyFence`); null = healthy.
        fence: ?seams.FenceProbe = null,
    };

    /// `close_rewrite_epoch_with`: move the ino's open epoch to `closing`
    /// (still visible; a concurrent record opens the next epoch over its
    /// shadow), run the ONE save with fencing convergence, then free the
    /// parked keys OUTSIDE every lock (RES-1). A save already in flight for
    /// the ino is awaited first — register-then-recheck on `close_settled`
    /// — so the saves of one ino land in epoch order, each displacing
    /// exactly what it parked. Never under a block guard (3).
    pub fn close(self: *Epochs, io: Io, ino: u64, cs: CloseSeams) CloseOutcome {
        const Taken = union(enum) { absent, taken: *Epoch, wait, oom };
        while (true) {
            var n = self.close_settled.notified();
            _ = n.enable(io);
            const verdict: Taken = blk: {
                self.lock.lock();
                defer self.lock.unlock();
                if (self.closing.contains(ino)) break :blk .wait;
                const e = self.table.get(ino) orelse break :blk .absent;
                self.closing.putNoClobber(self.gpa, ino, e) catch break :blk .oom;
                _ = self.table.remove(ino);
                break :blk .{ .taken = e };
            };
            switch (verdict) {
                .absent => {
                    n.cancel(io);
                    return .absent;
                },
                // The epoch stays open and parked (never-lossy); the next
                // trigger retries.
                .oom => {
                    n.cancel(io);
                    return .transient;
                },
                .taken => |e| {
                    n.cancel(io);
                    return self.closeEpoch(io, e, cs);
                },
                .wait => {
                    stats.rewrite_shadow_close_waits.inc();
                    n.waitUncancelable(io);
                },
            }
        }
    }

    /// The `save_in_flight` caller's wait: until no save of `ino` is in
    /// flight (register-then-recheck on `close_settled`; the tick is the
    /// liveness backstop underneath). Never under a block guard (3).
    pub fn awaitSaveSettled(self: *Epochs, io: Io, ino: u64) void {
        while (true) {
            var n = self.close_settled.notified();
            _ = n.enable(io);
            const in_flight = blk: {
                self.lock.lock();
                defer self.lock.unlock();
                break :blk self.closing.contains(ino);
            };
            if (!in_flight) {
                n.cancel(io);
                return;
            }
            stats.rewrite_shadow_save_waits.inc();
            n.waitUncancelable(io);
        }
    }

    /// A transient save: the epoch leaves `closing` and goes back to the
    /// open table — folded into the fresh epoch a record opened meanwhile.
    fn reregister(self: *Epochs, io: Io, e: *Epoch) void {
        {
            self.lock.lock();
            defer self.lock.unlock();
            _ = self.closing.remove(e.ino);
            // No allocation: `record` keeps the table slot and the fresh
            // epoch's room for ours.
            const gop = self.table.getOrPutAssumeCapacity(e.ino);
            if (gop.found_existing) {
                // A record opened a fresh epoch meanwhile: fold ours into it.
                // A key the fresh epoch parked as "the map's" because OUR
                // save was publishing it is now our unpublished mint again
                // (its guard moves below): it becomes the fresh epoch's own
                // orphan, freed at its close or returned by a supersede.
                const fresh = gop.value_ptr.*;
                for (fresh.displaced.items) |*p| {
                    if (p.origin == .map and e.shadowBinding(p.block) != null and e.shadowBinding(p.block).?.eql(p.binding)) p.origin = .epoch;
                }
                var it = e.shadow.iterator();
                while (it.next()) |kv| {
                    if (!fresh.shadow.contains(kv.key_ptr.*)) fresh.shadow.putAssumeCapacity(kv.key_ptr.*, kv.value_ptr.*);
                }
                fresh.displaced.appendSliceAssumeCapacity(e.displaced.items);
                fresh.guards.appendSliceAssumeCapacity(e.guards.items);
                e.guards.clearRetainingCapacity();
                fresh.recorded_bytes += e.recorded_bytes;
                fresh.parked_bytes += e.parked_bytes;
                fresh.min_size = @max(fresh.min_size, e.min_size);
                e.deinit(self.gpa);
                self.gpa.destroy(e);
                stats.rewrite_shadow_open_epochs.sub(1);
            } else {
                gop.value_ptr.* = e;
            }
        }
        self.close_settled.notifyWaiters(io);
    }

    fn closeEpoch(self: *Epochs, io: Io, e: *Epoch, cs: CloseSeams) CloseOutcome {
        var inserts_buf: [seams.MergeOutcome.max_displaced]seams.MapInsert = undefined;
        var proofs_buf: [seams.MergeOutcome.max_displaced]seams.DmaProof = undefined;
        var n: usize = 0;
        var it = e.shadow.iterator();
        while (it.next()) |kv| {
            // `record` caps the open shadow united with the closing one at
            // the ONE save's size, so a fold fits too. Past it is a bug:
            // loud, never lossy (the epoch re-registers whole).
            if (n == inserts_buf.len) {
                stats.rewrite_shadow_save_overflow.fire("an epoch shadows more blocks than the ONE save carries; it stays open, nothing published or freed");
                self.reregister(io, e);
                return .transient;
            }
            inserts_buf[n] = .{ .block = kv.key_ptr.*, .binding = kv.value_ptr.binding };
            proofs_buf[n] = kv.value_ptr.proof;
            n += 1;
        }
        var outcome: seams.MergeOutcome = .{};
        if (n != 0) {
            // Fencing convergence: a process-local rotation — re-present
            // the CURRENT generation; NEVER W5.
            while (true) {
                const token = cs.map.currentFencing(e.ino);
                outcome = cs.map.merge(.{ .ino = e.ino, .inserts = inserts_buf[0..n], .min_size = e.min_size, .proofs = proofs_buf[0..n], .fencing = token }) catch |err| switch (err) {
                    error.FencingTokenExpired => {
                        stats.rewrite_shadow_close_retries.inc();
                        continue;
                    },
                    error.WriterGuardFenced => {
                        switch (seams.classifyFence(cs.fence, &stats.rewrite_shadow_fence_drops_failed_volume, &stats.rewrite_shadow_fence_drops, "a rewrite epoch's close met the writer-guard fence on a HEALTHY volume")) {
                            .failed_volume => log.info("rewrite epoch ino {d}: fenced at close on a FAILED volume — {d} shadowed blocks published nothing, {d} parked keys freed nothing (successor recovery owns the accounting)", .{ e.ino, n, e.displaced.items.len }),
                            .healthy => log.err("rewrite epoch ino {d}: FENCED at close on a HEALTHY volume — {d} shadowed blocks published nothing, {d} parked keys freed nothing (successor recovery owns the accounting)", .{ e.ino, n, e.displaced.items.len }),
                        }
                        self.dropEpoch(io, e);
                        return .fence_dropped;
                    },
                    error.NotFound => {
                        // The ino never comes back: every key the epoch
                        // minted is an orphan — the newest B per block and
                        // each intermediate B parked as the epoch's (freed
                        // under their guards, which `dropEpoch` drops after).
                        // The map's A keys belong to the destroy.
                        stats.rewrite_shadow_orphan_discards.inc();
                        for (inserts_buf[0..n]) |ins| _ = cs.free.free(io, ins.binding);
                        for (e.displaced.items) |p| if (p.origin == .epoch) {
                            _ = cs.free.free(io, p.binding);
                        };
                        self.dropEpoch(io, e);
                        return .orphan_discarded;
                    },
                    error.Transient, error.OutOfMemory => {
                        self.reregister(io, e);
                        return .transient;
                    },
                };
                break;
            }
        }
        // Success: the merge's displaced set must be within the parked set
        // (every parked key is freed below). A key the merge displaced that
        // we never parked means another arm published the block under us —
        // the visibility law forbids it (module doc); the tripwire names it
        // and the key is freed too (the map no longer names it).
        var drift: [seams.MergeOutcome.max_displaced]Binding = undefined;
        var n_drift: usize = 0;
        for (outcome.slice()) |d| {
            var parked = false;
            for (e.displaced.items) |p| if (p.binding.eql(d)) {
                parked = true;
            };
            if (!parked) {
                // One line per site: the key rides the tripwire's own detail
                // (a second `log.err` would be a second error line for the
                // runner's "logs an error" law — the drift tests declare one).
                var detail: [160]u8 = undefined;
                const text = std.fmt.bufPrint(&detail, "the save displaced a binding the epoch never parked: ino {d} vol {x} off {d} (unwritten {}), {d} parked key(s)", .{ e.ino, d.vol_tag, d.offset, d.unwritten, e.displaced.items.len }) catch "the save displaced a binding the epoch never parked";
                stats.rewrite_shadow_displaced_drift.fire(text);
                drift[n_drift] = d;
                n_drift += 1;
            }
        }
        stats.rewrite_shadow_swaps.inc();
        stats.rewrite_shadow_bytes.add(e.recorded_bytes);
        const bytes = e.recorded_bytes;
        const freed: u32 = @intCast(e.displaced.items.len + n_drift);
        // Free the parked keys with no lock held (RES-1 — "the worst park
        // amplifier in the tree"), then drop the live-owner registrations:
        // the merge's on the keys it displaced, every B guard (an
        // intermediate B among the parked keys is an unpublished mint until
        // its free — decision 0003 §AH.1). The epoch leaves sight only now:
        // the map binds B, so a write landing between the save and here
        // bases on B either way.
        for (e.displaced.items) |d| {
            cs.purge.purge(d.binding);
            _ = cs.free.free(io, d.binding);
        }
        for (drift[0..n_drift]) |d| {
            cs.purge.purge(d);
            _ = cs.free.free(io, d);
        }
        outcome.dropHolds();
        for (e.guards.items) |*g| g.deinit();
        e.guards.clearRetainingCapacity();
        self.dropEpoch(io, e);
        return .{ .swapped = .{ .bytes = bytes, .freed = freed } };
    }

    /// The saving epoch is done (landed, fenced or orphaned): out of sight,
    /// the next close of the ino may proceed.
    fn dropEpoch(self: *Epochs, io: Io, e: *Epoch) void {
        {
            self.lock.lock();
            defer self.lock.unlock();
            _ = self.closing.remove(e.ino);
        }
        stats.rewrite_shadow_parked_bytes.sub(e.parked_bytes);
        stats.rewrite_shadow_open_epochs.sub(1);
        e.deinit(self.gpa);
        self.gpa.destroy(e);
        self.close_settled.notifyWaiters(io);
    }

    /// The ENOSPC early-close (KD-1.7) generalized to the ONE allocation
    /// act: close every open epoch, freeing its parked supply. True iff any
    /// close swapped (the `enospc.PressureSupplyHook` contract).
    pub fn closeAllForSpace(self: *Epochs, io: Io, cs: CloseSeams) bool {
        var inos: [64]u64 = undefined;
        var n: usize = 0;
        {
            self.lock.lock();
            defer self.lock.unlock();
            var it = self.table.keyIterator();
            while (it.next()) |k| {
                if (n == inos.len) break;
                inos[n] = k.*;
                n += 1;
            }
        }
        var released = false;
        for (inos[0..n]) |ino| {
            stats.rewrite_shadow_pressure_closes.inc();
            switch (self.close(io, ino, cs)) {
                .swapped => |s| released = released or s.freed != 0,
                else => {},
            }
        }
        return released;
    }

    /// The idle sweep: close every epoch with no record for
    /// `EPOCH_IDLE_HORIZON_MS`. Returns the closes attempted.
    pub fn closeIdle(self: *Epochs, io: Io, now_ns: u64, cs: CloseSeams) u32 {
        var inos: [64]u64 = undefined;
        var n: usize = 0;
        {
            self.lock.lock();
            defer self.lock.unlock();
            var it = self.table.iterator();
            while (it.next()) |kv| {
                if (n == inos.len) break;
                if (now_ns -| kv.value_ptr.*.last_record_ns >= EPOCH_IDLE_HORIZON_MS * std.time.ns_per_ms) {
                    inos[n] = kv.key_ptr.*;
                    n += 1;
                }
            }
        }
        for (inos[0..n]) |ino| {
            stats.rewrite_shadow_idle_closes.inc();
            _ = self.close(io, ino, cs);
        }
        return @intCast(n);
    }

    /// Unmount / fence: close every epoch (fence ⇒ nothing published, nothing freed).
    pub fn closeAll(self: *Epochs, io: Io, cs: CloseSeams) void {
        _ = self.closeAllForSpace(io, cs);
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const wt = @import("write_testing.zig");

fn proofFor(b: Binding, len: u64) seams.DmaProof {
    return .{ .vol_tag = b.vol_tag, .offset = b.offset, .len = len, .completed_ns = 1 };
}

test "epoch_records_are_ram_only_until_the_one_save_swap; full_coverage_auto_closes_the_epoch (hint); the swap frees parked A keys AFTER the save and only then" {
    const io = testing.io;
    const f = try wt.Fixture.init(testing.allocator, 16);
    defer f.deinit();
    var ep = Epochs.init(testing.allocator, wt.BS, true);
    defer ep.deinit();
    const parked0 = stats.rewrite_shadow_parked_bytes.load();
    const a0 = try f.publishBlock(10, 0, 0xAA);
    const a1 = try f.publishBlock(10, 1, 0xBB);
    // Two B mints.
    const m0 = try f.alloc_a.allocateBlock();
    const m1 = try f.alloc_a.allocateBlock();
    const b0 = Binding.wholeBlock(wt.VOL_A, m0.offset, m0.stamp);
    const b1 = Binding.wholeBlock(wt.VOL_A, m1.offset, m1.stamp);
    const v0 = try ep.record(10, 0, a0, b0, proofFor(b0, wt.BS), 2 * wt.BS, block_alloc.InflightAllocGuard.register(f.alloc_a, m0.idx), true);
    try testing.expect(v0.shadowed.displaced_prev.?.eql(a0));
    try testing.expect(!v0.shadowed.close_owed);
    try testing.expectEqual(@as(usize, 1), ep.openCount());
    try testing.expect(ep.bindsBlock(10, 0));
    try testing.expect(!ep.bindsBlock(10, 1));
    try testing.expect(ep.shadowOf(10, 0).?.eql(b0));
    // RAM only: the map still binds A, nothing merged, nothing freed.
    try testing.expect(f.map.seam().lookup(10, 0).?.eql(a0));
    try testing.expectEqual(@as(usize, 0), f.map.merges.items.len);
    try testing.expectEqual(@as(usize, 0), f.frees.freed.items.len);
    try testing.expect(f.alloc_a.inflightContains(m0.idx));
    const v1 = try ep.record(10, 1, a1, b1, proofFor(b1, wt.BS), 2 * wt.BS, block_alloc.InflightAllocGuard.register(f.alloc_a, m1.idx), true);
    try testing.expect(v1.shadowed.close_owed); // the close-owed hint (full coverage)
    try testing.expectEqual(parked0 + 2 * wt.BS, stats.rewrite_shadow_parked_bytes.load());
    const cs: Epochs.CloseSeams = .{ .map = f.map.seam(), .free = f.frees.seam(), .purge = .none };
    const out = ep.close(io, 10, cs);
    try testing.expectEqual(@as(u64, 2 * wt.BS), out.swapped.bytes);
    try testing.expectEqual(@as(u32, 2), out.swapped.freed);
    // ONE save: one merge of two blocks; the map binds B; both A keys freed; guards popped.
    try testing.expectEqual(@as(usize, 1), f.map.merges.items.len);
    try testing.expectEqual(@as(u8, 2), f.map.merges.items[0].blocks);
    try testing.expect(f.map.seam().lookup(10, 0).?.eql(b0));
    try testing.expect(f.map.seam().lookup(10, 1).?.eql(b1));
    try testing.expectEqual(@as(usize, 2), f.frees.freed.items.len);
    try testing.expect(!f.alloc_a.inflightContains(m0.idx));
    // The A keys the save released were held across their frees and are
    // not after (decision 0003 §AH.1).
    try testing.expectEqual(@as(u32, 0), f.frees.unheld);
    try testing.expect(!f.alloc_a.inflightContains(f.alloc_a.idxOf(a0.offset)));
    try testing.expect(!f.alloc_a.inflightContains(f.alloc_a.idxOf(a1.offset)));
    try testing.expectEqual(@as(usize, 0), ep.openCount());
    try testing.expectEqual(CloseOutcome.absent, ep.close(io, 10, cs));
    try testing.expectEqual(parked0, stats.rewrite_shadow_parked_bytes.load());
}

test "the save's frees are live owners' (decision 0003 §AH.1): an intermediate B the epoch superseded stays an unpublished mint under its guard until its free, the A key the save displaced stays held from before the save until after its free; nothing is left in flight" {
    const io = testing.io;
    const f = try wt.Fixture.init(testing.allocator, 16);
    defer f.deinit();
    var ep = Epochs.init(testing.allocator, wt.BS, true);
    defer ep.deinit();
    const a = try f.publishBlock(11, 0, 0xAA);
    const m1 = try f.alloc_a.allocateBlock();
    const m2 = try f.alloc_a.allocateBlock();
    const b1 = Binding.wholeBlock(wt.VOL_A, m1.offset, m1.stamp);
    const b2 = Binding.wholeBlock(wt.VOL_A, m2.offset, m2.stamp);
    _ = try ep.record(11, 0, a, b1, proofFor(b1, wt.BS), 2 * wt.BS, block_alloc.InflightAllocGuard.register(f.alloc_a, m1.idx), true);
    const v2 = try ep.record(11, 0, a, b2, proofFor(b2, wt.BS), 2 * wt.BS, block_alloc.InflightAllocGuard.register(f.alloc_a, m2.idx), true);
    try testing.expect(v2.shadowed.displaced_prev.?.eql(b1)); // B1 superseded, parked as ours
    const cs: Epochs.CloseSeams = .{ .map = f.map.seam(), .free = f.frees.seam(), .purge = .none };
    try testing.expect(ep.close(io, 11, cs) == .swapped);
    try testing.expect(f.map.seam().lookup(11, 0).?.eql(b2));
    try testing.expectEqual(@as(usize, 2), f.frees.freed.items.len); // A and B1
    try testing.expectEqual(@as(u32, 0), f.frees.unheld);
    for ([_]u64{ f.alloc_a.idxOf(a.offset), m1.idx, m2.idx }) |idx| try testing.expect(!f.alloc_a.inflightContains(idx));
}

test "fenced_close_publishes_nothing_and_frees_nothing; a_process_local_lease_rotation_converges_the_close; a transient re-registers never-lossy; a reclaimed ino discards orphans" {
    const io = testing.io;
    const f = try wt.Fixture.init(testing.allocator, 16);
    defer f.deinit();
    var ep = Epochs.init(testing.allocator, wt.BS, true);
    defer ep.deinit();
    const cs: Epochs.CloseSeams = .{ .map = f.map.seam(), .free = f.frees.seam(), .purge = .none };
    const a0 = try f.publishBlock(20, 0, 0x11);
    const m = try f.alloc_a.allocateBlock();
    const b = Binding.wholeBlock(wt.VOL_A, m.offset, m.stamp);
    _ = try ep.record(20, 0, a0, b, proofFor(b, wt.BS), wt.BS, block_alloc.InflightAllocGuard.register(f.alloc_a, m.idx), true);
    // Fencing rotation: the first merge refuses with an expired token; the close re-presents the current one.
    f.map.fail_next_merge = error.FencingTokenExpired;
    const retries0 = stats.rewrite_shadow_close_retries.load();
    try testing.expect(ep.close(io, 20, cs) == .swapped);
    try testing.expectEqual(retries0 + 1, stats.rewrite_shadow_close_retries.load());
    // Transient: re-registered, nothing freed, the next close lands.
    const a1 = try f.publishBlock(21, 0, 0x22);
    const m1 = try f.alloc_a.allocateBlock();
    const b1 = Binding.wholeBlock(wt.VOL_A, m1.offset, m1.stamp);
    _ = try ep.record(21, 0, a1, b1, proofFor(b1, wt.BS), wt.BS, block_alloc.InflightAllocGuard.register(f.alloc_a, m1.idx), true);
    f.map.fail_next_merge = error.Transient;
    const frees_before = f.frees.freed.items.len;
    try testing.expectEqual(CloseOutcome.transient, ep.close(io, 21, cs));
    try testing.expectEqual(frees_before, f.frees.freed.items.len);
    try testing.expectEqual(@as(usize, 1), ep.openCount());
    try testing.expect(ep.close(io, 21, cs) == .swapped);
    // Fence: publish nothing, free nothing, loud.
    const a2 = try f.publishBlock(22, 0, 0x33);
    const m2 = try f.alloc_a.allocateBlock();
    const b2 = Binding.wholeBlock(wt.VOL_A, m2.offset, m2.stamp);
    _ = try ep.record(22, 0, a2, b2, proofFor(b2, wt.BS), wt.BS, block_alloc.InflightAllocGuard.register(f.alloc_a, m2.idx), true);
    f.map.fail_next_merge = error.WriterGuardFenced;
    const frees_fence = f.frees.freed.items.len;
    const merges_fence = f.map.merges.items.len;
    const fd0 = counters.tripwireCount("rewrite_shadow_fence_drops");
    stats.rewrite_shadow_fence_drops.resetLogBudgetForTest();
    log.testing.expectErrorLogs(2);
    try testing.expectEqual(CloseOutcome.fence_dropped, ep.close(io, 22, cs));
    try log.testing.expectAllFired();
    try testing.expectEqual(frees_fence, f.frees.freed.items.len);
    try testing.expectEqual(merges_fence, f.map.merges.items.len);
    try testing.expect(f.map.seam().lookup(22, 0).?.eql(a2));
    try testing.expectEqual(fd0 + 1, counters.tripwireCount("rewrite_shadow_fence_drops"));
    // The same fence while the probe says FAILED: W5 unchanged, counted, no tripwire, no error log.
    const Latched = struct {
        fn poll(_: ?*anyopaque) bool {
            return true;
        }
    };
    const cs_failed: Epochs.CloseSeams = .{ .map = f.map.seam(), .free = f.frees.seam(), .purge = .none, .fence = .{ .ctx = null, .poll = &Latched.poll } };
    const a4 = try f.publishBlock(24, 0, 0x55);
    const m4 = try f.alloc_a.allocateBlock();
    const b4 = Binding.wholeBlock(wt.VOL_A, m4.offset, m4.stamp);
    _ = try ep.record(24, 0, a4, b4, proofFor(b4, wt.BS), wt.BS, block_alloc.InflightAllocGuard.register(f.alloc_a, m4.idx), true);
    f.map.fail_next_merge = error.WriterGuardFenced;
    const frees_failed = f.frees.freed.items.len;
    const merges_failed = f.map.merges.items.len;
    const failed0 = stats.rewrite_shadow_fence_drops_failed_volume.load();
    try testing.expectEqual(CloseOutcome.fence_dropped, ep.close(io, 24, cs_failed));
    try testing.expectEqual(frees_failed, f.frees.freed.items.len);
    try testing.expectEqual(merges_failed, f.map.merges.items.len);
    try testing.expect(f.map.seam().lookup(24, 0).?.eql(a4));
    try testing.expectEqual(failed0 + 1, stats.rewrite_shadow_fence_drops_failed_volume.load());
    try testing.expectEqual(fd0 + 1, counters.tripwireCount("rewrite_shadow_fence_drops"));
    // A reclaimed ino: the B key is an orphan and is freed; the A key belongs to the destroy.
    const a3 = try f.publishBlock(23, 0, 0x44);
    const m3 = try f.alloc_a.allocateBlock();
    const b3 = Binding.wholeBlock(wt.VOL_A, m3.offset, m3.stamp);
    _ = try ep.record(23, 0, a3, b3, proofFor(b3, wt.BS), wt.BS, block_alloc.InflightAllocGuard.register(f.alloc_a, m3.idx), true);
    f.map.remove(23);
    const frees_orphan = f.frees.freed.items.len;
    try testing.expectEqual(CloseOutcome.orphan_discarded, ep.close(io, 23, cs));
    try testing.expectEqual(frees_orphan + 1, f.frees.freed.items.len);
    try testing.expect(f.frees.freed.items[frees_orphan].eql(b3));
}

test "a reclaimed ino frees EVERY key the epoch minted: a block rewritten twice in the epoch leaves its intermediate B freed with the newest, the map's A the destroy's — no mint left tracked and referenced by nothing" {
    const io = testing.io;
    const f = try wt.Fixture.init(testing.allocator, 16);
    defer f.deinit();
    var ep = Epochs.init(testing.allocator, wt.BS, true);
    defer ep.deinit();
    const cs: Epochs.CloseSeams = .{ .map = f.map.seam(), .free = f.frees.seam(), .purge = .none };
    const a = try f.publishBlock(24, 0, 0x55);
    const m1 = try f.alloc_a.allocateBlock();
    const m2 = try f.alloc_a.allocateBlock();
    const b1 = Binding.wholeBlock(wt.VOL_A, m1.offset, m1.stamp);
    const b2 = Binding.wholeBlock(wt.VOL_A, m2.offset, m2.stamp);
    _ = try ep.record(24, 0, a, b1, proofFor(b1, wt.BS), 2 * wt.BS, block_alloc.InflightAllocGuard.register(f.alloc_a, m1.idx), true);
    const v2 = try ep.record(24, 0, a, b2, proofFor(b2, wt.BS), 2 * wt.BS, block_alloc.InflightAllocGuard.register(f.alloc_a, m2.idx), true);
    try testing.expect(v2.shadowed.displaced_prev.?.eql(b1)); // B1 parked as the epoch's own
    f.map.remove(24);
    try testing.expectEqual(CloseOutcome.orphan_discarded, ep.close(io, 24, cs));
    // B1 and B2 freed, each once, under their guards; A stays (the destroy frees it).
    try testing.expectEqual(@as(usize, 2), f.frees.freed.items.len);
    try testing.expectEqual(@as(u32, 0), f.frees.unheld);
    for ([_]u64{ m1.idx, m2.idx }) |idx| {
        try testing.expectEqual(@as(?u32, null), f.alloc_a.refcount(idx));
        try testing.expect(!f.alloc_a.inflightContains(idx));
    }
    try testing.expectEqual(@as(u64, 1), f.alloc_a.usedBlocks());
    try testing.expect(f.alloc_a.refcount(f.alloc_a.idxOf(a.offset)) != null);
}

test "the close triggers: ENOSPC pressure close frees parked supply; the idle sweep closes at TTI/10; supersede evicts a shadow binding; the lever off never shadows" {
    const io = testing.io;
    const f = try wt.Fixture.init(testing.allocator, 16);
    defer f.deinit();
    var ep = Epochs.init(testing.allocator, wt.BS, true);
    defer ep.deinit();
    const cs: Epochs.CloseSeams = .{ .map = f.map.seam(), .free = f.frees.seam(), .purge = .none };
    const a0 = try f.publishBlock(30, 0, 0x11);
    const m = try f.alloc_a.allocateBlock();
    const b = Binding.wholeBlock(wt.VOL_A, m.offset, m.stamp);
    _ = try ep.record(30, 0, a0, b, proofFor(b, wt.BS), wt.BS, block_alloc.InflightAllocGuard.register(f.alloc_a, m.idx), true);
    const used = f.alloc_a.usedBlocks();
    try testing.expect(ep.closeAllForSpace(io, cs));
    try testing.expectEqual(used - 1, f.alloc_a.usedBlocks()); // the parked A key returned to supply
    try testing.expect(!ep.closeAllForSpace(io, cs)); // nothing left to release
    // Idle: not yet, then at the horizon.
    const a1 = try f.publishBlock(31, 0, 0x22);
    const m1 = try f.alloc_a.allocateBlock();
    const b1 = Binding.wholeBlock(wt.VOL_A, m1.offset, m1.stamp);
    _ = try ep.record(31, 0, a1, b1, proofFor(b1, wt.BS), wt.BS, block_alloc.InflightAllocGuard.register(f.alloc_a, m1.idx), true);
    const now = clock.monotonicNs();
    try testing.expectEqual(@as(u32, 0), ep.closeIdle(io, now, cs));
    try testing.expectEqual(@as(u32, 1), ep.closeIdle(io, now + EPOCH_IDLE_HORIZON_MS * std.time.ns_per_ms, cs));
    try testing.expectEqual(@as(usize, 0), ep.openCount());
    try testing.expectEqual(@as(u64, 30_000), EPOCH_IDLE_HORIZON_MS);
    // Supersede: a durable publish outside the epoch evicts shadow[b]; the close then publishes nothing for it.
    const a2 = try f.publishBlock(32, 0, 0x33);
    const m2 = try f.alloc_a.allocateBlock();
    const b2 = Binding.wholeBlock(wt.VOL_A, m2.offset, m2.stamp);
    _ = try ep.record(32, 0, a2, b2, proofFor(b2, wt.BS), wt.BS, block_alloc.InflightAllocGuard.register(f.alloc_a, m2.idx), true);
    const sup0 = stats.rewrite_shadow_superseded.load();
    var orphans: [8]Binding = undefined;
    const other = Binding.wholeBlock(wt.VOL_A, 7 * wt.BS, 99); // what the outside publisher landed
    const parked_before = stats.rewrite_shadow_parked_bytes.load();
    const orph = ep.supersede(32, 0, other, &orphans);
    try testing.expectEqual(sup0 + 1, stats.rewrite_shadow_superseded.load());
    try testing.expect(!ep.bindsBlock(32, 0));
    try testing.expectEqual(@as(usize, 1), orph.len); // our B is the orphan; the parked A is the publisher's
    try testing.expect(orph[0].eql(b2));
    try testing.expectEqual(parked_before - wt.BS, stats.rewrite_shadow_parked_bytes.load());
    const merges0 = f.map.merges.items.len;
    const frees_sup = f.frees.freed.items.len;
    try testing.expect(ep.close(io, 32, cs) == .swapped);
    try testing.expectEqual(merges0, f.map.merges.items.len); // an empty shadow saves nothing
    try testing.expectEqual(frees_sup, f.frees.freed.items.len); // and frees nothing: A is the publisher's
    try testing.expect(f.map.seam().lookup(32, 0).?.eql(a2));
    // Lever off: never shadows (the guard is returned).
    var off = Epochs.init(testing.allocator, wt.BS, false);
    defer off.deinit();
    const m3 = try f.alloc_a.allocateBlock();
    const b3 = Binding.wholeBlock(wt.VOL_A, m3.offset, m3.stamp);
    try testing.expectEqual(RecordVerdict.not_shadowed, try off.record(33, 0, null, b3, proofFor(b3, wt.BS), wt.BS, block_alloc.InflightAllocGuard.register(f.alloc_a, m3.idx), true));
    try testing.expect(!f.alloc_a.inflightContains(m3.idx));
}

test "a supersede of a block rewritten more times in the epoch than its orphan batch holds never drops a mint: the batch is freed by the publisher, the rest stay parked and the close frees them — every B freed once, nothing left tracked and referenced by nothing" {
    const io = testing.io;
    const f = try wt.Fixture.init(testing.allocator, 32);
    defer f.deinit();
    var ep = Epochs.init(testing.allocator, wt.BS, true);
    defer ep.deinit();
    const cs: Epochs.CloseSeams = .{ .map = f.map.seam(), .free = f.frees.seam(), .purge = .none };
    const a = try f.publishBlock(34, 0, 0x66);
    // Ten records of block 0: the newest B in the shadow, nine intermediate Bs parked as the epoch's.
    var orphans: [8]Binding = undefined;
    const rewrites = orphans.len + 2;
    var minted: [rewrites]u64 = undefined;
    for (&minted) |*idx| {
        const m = try f.alloc_a.allocateBlock();
        const b = Binding.wholeBlock(wt.VOL_A, m.offset, m.stamp);
        _ = try ep.record(34, 0, f.map.seam().lookup(34, 0), b, proofFor(b, wt.BS), 2 * wt.BS, block_alloc.InflightAllocGuard.register(f.alloc_a, m.idx), true);
        idx.* = m.idx;
    }
    // A durable publish of block 0 outside the epoch (the upload's not-striped arm): A is the publisher's.
    const mp = try f.alloc_a.allocateBlock();
    const published = Binding.wholeBlock(wt.VOL_A, mp.offset, mp.stamp);
    f.alloc_a.publishBlock(mp.idx);
    directPublish(io, f, 34, 0, published);
    for (ep.supersede(34, 0, published, &orphans)) |o| _ = f.frees.seam().free(io, o);
    try testing.expect(!ep.bindsBlock(34, 0));
    try testing.expect(ep.close(io, 34, cs) == .swapped);
    // A and all ten Bs freed exactly once; only the published key is tracked.
    try testing.expectEqual(@as(usize, rewrites + 1), f.frees.freed.items.len);
    for (minted) |idx| {
        try testing.expectEqual(@as(?u32, null), f.alloc_a.refcount(idx));
        try testing.expect(!f.alloc_a.inflightContains(idx));
    }
    try testing.expectEqual(@as(?u32, null), f.alloc_a.refcount(f.alloc_a.idxOf(a.offset)));
    try testing.expectEqual(@as(u64, 1), f.alloc_a.usedBlocks());
    try testing.expect(f.map.seam().lookup(34, 0).?.eql(published));
}

test "the save stays visible and closes of one ino serialize in epoch order: during a save the block stays bound and the shadow served; a record meanwhile opens the next epoch over the CLOSING shadow (never the map's stale A); a second close parks on the running one; the saves land B then C, A and B freed once each, drift 0" {
    const io = testing.io;
    const f = try wt.Fixture.init(testing.allocator, 16);
    defer f.deinit();
    var ep = Epochs.init(testing.allocator, wt.BS, true);
    defer ep.deinit();
    const cs: Epochs.CloseSeams = .{ .map = f.map.seam(), .free = f.frees.seam(), .purge = .none };
    const drift0 = counters.tripwireCount("rewrite_shadow_displaced_drift");
    const waits0 = stats.rewrite_shadow_close_waits.load();
    const a = try f.publishBlock(60, 0, 0xAA);
    const mb = try f.alloc_a.allocateBlock();
    const b = Binding.wholeBlock(wt.VOL_A, mb.offset, mb.stamp);
    _ = try ep.record(60, 0, a, b, proofFor(b, wt.BS), wt.BS, block_alloc.InflightAllocGuard.register(f.alloc_a, mb.idx), true);
    // The first close parks inside its save window (before the merge).
    const Gate = struct {
        var hold: std.atomic.Value(bool) = .init(true);
        var in_window: std.atomic.Value(bool) = .init(false);
        fn hook(_: *anyopaque, _: *wt.RamMap) void {
            in_window.store(true, .release);
            while (hold.load(.acquire)) std.Thread.yield() catch {};
        }
    };
    var dummy: u8 = 0;
    f.map.before_merge = .{ .ctx = &dummy, .call = &Gate.hook };
    const Closer = struct {
        fn run(io_: Io, e: *Epochs, cs_: Epochs.CloseSeams, out: *CloseOutcome) void {
            out.* = e.close(io_, 60, cs_);
        }
    };
    var out1: CloseOutcome = .absent;
    const t1 = try std.Thread.spawn(.{}, Closer.run, .{ io, &ep, cs, &out1 });
    while (!Gate.in_window.load(.acquire)) try std.Thread.yield();
    // In the window: bound, served, not open; the map still binds A.
    try testing.expect(ep.bindsBlock(60, 0));
    try testing.expect(ep.shadowOf(60, 0).?.eql(b));
    try testing.expectEqual(@as(usize, 0), ep.openCount());
    try testing.expect(ep.baseBinding(f.map.seam(), 60, 0).?.eql(b));
    // A record over the block opens the next epoch; its displaced key is
    // the closing shadow B (parked as the map's), never A.
    const mc = try f.alloc_a.allocateBlock();
    const c = Binding.wholeBlock(wt.VOL_A, mc.offset, mc.stamp);
    const v = try ep.record(60, 0, f.map.seam().lookup(60, 0), c, proofFor(c, wt.BS), wt.BS, block_alloc.InflightAllocGuard.register(f.alloc_a, mc.idx), true);
    try testing.expect(v.shadowed.displaced_prev.?.eql(b));
    try testing.expect(ep.shadowOf(60, 0).?.eql(c));
    try testing.expectEqual(@as(usize, 1), ep.openCount());
    // A second close parks on the running save.
    var out2: CloseOutcome = .absent;
    const t2 = try std.Thread.spawn(.{}, Closer.run, .{ io, &ep, cs, &out2 });
    while (ep.close_settled.waiters(io) == 0) try std.Thread.yield();
    try testing.expectEqual(@as(usize, 0), f.map.merges.items.len);
    Gate.hold.store(false, .release);
    t1.join();
    t2.join();
    try testing.expect(out1 == .swapped and out2 == .swapped);
    try testing.expectEqual(@as(usize, 2), f.map.merges.items.len);
    try testing.expect(f.map.seam().lookup(60, 0).?.eql(c));
    try testing.expectEqual(@as(usize, 2), f.frees.freed.items.len);
    try testing.expect(f.frees.freed.items[0].eql(a));
    try testing.expect(f.frees.freed.items[1].eql(b));
    try testing.expectEqual(waits0 + 1, stats.rewrite_shadow_close_waits.load());
    try testing.expectEqual(drift0, counters.tripwireCount("rewrite_shadow_displaced_drift"));
    try testing.expectEqual(@as(usize, 0), ep.openCount());
    try testing.expect(!ep.bindsBlock(60, 0));
    try testing.expectEqual(@as(usize, 0), ep.close_settled.waiters(io));
}

test "a transient save folds back into the next epoch: the key the next epoch parked as the saving B becomes its own orphan mint again (freed once at its close, never left in flight); the guards travel" {
    const io = testing.io;
    const f = try wt.Fixture.init(testing.allocator, 16);
    defer f.deinit();
    var ep = Epochs.init(testing.allocator, wt.BS, true);
    defer ep.deinit();
    const cs: Epochs.CloseSeams = .{ .map = f.map.seam(), .free = f.frees.seam(), .purge = .none };
    const a = try f.publishBlock(61, 0, 0xAA);
    const mb = try f.alloc_a.allocateBlock();
    const b = Binding.wholeBlock(wt.VOL_A, mb.offset, mb.stamp);
    _ = try ep.record(61, 0, a, b, proofFor(b, wt.BS), wt.BS, block_alloc.InflightAllocGuard.register(f.alloc_a, mb.idx), true);
    // The save's merge is transient; a record lands in its window.
    const Racer = struct {
        var ep_: *Epochs = undefined;
        var f_: *wt.Fixture = undefined;
        var b_: Binding = undefined;
        var c: Binding = undefined;
        fn hook(_: *anyopaque, map: *wt.RamMap) void {
            map.fail_next_merge = error.Transient;
            const mc = f_.alloc_a.allocateBlock() catch unreachable;
            c = Binding.wholeBlock(wt.VOL_A, mc.offset, mc.stamp);
            const v = ep_.record(61, 0, map.seam().lookup(61, 0), c, proofFor(c, wt.BS), wt.BS, block_alloc.InflightAllocGuard.register(f_.alloc_a, mc.idx), true) catch unreachable;
            std.debug.assert(v.shadowed.displaced_prev.?.eql(b_)); // the closing shadow, not the map's A
        }
    };
    Racer.ep_ = &ep;
    Racer.f_ = f;
    Racer.b_ = b;
    var dummy: u8 = 0;
    f.map.before_merge = .{ .ctx = &dummy, .call = &Racer.hook };
    try testing.expectEqual(CloseOutcome.transient, ep.close(io, 61, cs));
    try testing.expectEqual(@as(usize, 1), ep.openCount());
    try testing.expect(ep.shadowOf(61, 0).?.eql(Racer.c));
    try testing.expect(f.alloc_a.inflightContains(mb.idx)); // B's guard travelled into the fresh epoch
    // The next close publishes C over A: A (parked by the folded epoch) and
    // B (the orphan mint) are freed exactly once each.
    const out = ep.close(io, 61, cs);
    try testing.expectEqual(@as(u32, 2), out.swapped.freed);
    try testing.expect(f.map.seam().lookup(61, 0).?.eql(Racer.c));
    try testing.expectEqual(@as(usize, 2), f.frees.freed.items.len);
    var saw_a = false;
    var saw_b = false;
    for (f.frees.freed.items) |x| {
        if (x.eql(a)) saw_a = true;
        if (x.eql(b)) saw_b = true;
    }
    try testing.expect(saw_a and saw_b);
    try testing.expect(!f.alloc_a.inflightContains(mb.idx));
    try testing.expectEqual(@as(u64, 1), f.alloc_a.usedBlocks());
}

test "a transient save folds into the next epoch with no allocation: an allocation failure at the fold drops no shadow and no guard — the next close publishes every B the folded epoch recorded and drops every guard" {
    const io = testing.io;
    const f = try wt.Fixture.init(testing.allocator, 16);
    defer f.deinit();
    var failing = std.testing.FailingAllocator.init(testing.allocator, .{});
    var ep = Epochs.init(failing.allocator(), wt.BS, true);
    defer ep.deinit();
    const cs: Epochs.CloseSeams = .{ .map = f.map.seam(), .free = f.frees.seam(), .purge = .none };
    try f.map.create(71, .striped, 0);
    var bs: [12]Binding = undefined;
    var idxs: [12]u64 = undefined;
    for (&bs, &idxs, 0..) |*b, *idx, i| {
        const mb = try f.alloc_a.allocateBlock();
        b.* = Binding.wholeBlock(wt.VOL_A, mb.offset, mb.stamp);
        idx.* = mb.idx;
        _ = try ep.record(71, @intCast(i), null, b.*, proofFor(b.*, wt.BS), 16 * wt.BS, block_alloc.InflightAllocGuard.register(f.alloc_a, mb.idx), true);
    }
    // The save's merge is transient; a record opens the next epoch in its
    // window, then every allocation fails.
    const Racer = struct {
        var ep_: *Epochs = undefined;
        var f_: *wt.Fixture = undefined;
        var fa: *std.testing.FailingAllocator = undefined;
        var c: Binding = undefined;
        fn hook(_: *anyopaque, map: *wt.RamMap) void {
            map.fail_next_merge = error.Transient;
            const mc = f_.alloc_a.allocateBlock() catch unreachable;
            c = Binding.wholeBlock(wt.VOL_A, mc.offset, mc.stamp);
            _ = ep_.record(71, 12, null, c, proofFor(c, wt.BS), 16 * wt.BS, block_alloc.InflightAllocGuard.register(f_.alloc_a, mc.idx), true) catch unreachable;
            fa.fail_index = fa.alloc_index;
            fa.resize_fail_index = fa.resize_index;
        }
    };
    Racer.ep_ = &ep;
    Racer.f_ = f;
    Racer.fa = &failing;
    var dummy: u8 = 0;
    f.map.before_merge = .{ .ctx = &dummy, .call = &Racer.hook };
    try testing.expectEqual(CloseOutcome.transient, ep.close(io, 71, cs));
    failing.fail_index = std.math.maxInt(usize);
    failing.resize_fail_index = std.math.maxInt(usize);
    _ = ep.close(io, 71, cs);
    for (bs, 0..) |b, i| {
        const got = f.map.seam().lookup(71, @intCast(i));
        try testing.expect(got != null and got.?.eql(b));
    }
    try testing.expect(f.map.seam().lookup(71, 12).?.eql(Racer.c));
    for (idxs) |idx| try testing.expect(!f.alloc_a.inflightContains(idx));
    try testing.expectEqual(@as(usize, 0), ep.openCount());
}

test "a drift key the save displaced is freed with no allocation: an allocation failure at the close leaves no key tracked by the allocator and referenced by nothing" {
    const io = testing.io;
    const f = try wt.Fixture.init(testing.allocator, 16);
    defer f.deinit();
    var failing = std.testing.FailingAllocator.init(testing.allocator, .{});
    var ep = Epochs.init(failing.allocator(), wt.BS, true);
    defer ep.deinit();
    const cs: Epochs.CloseSeams = .{ .map = f.map.seam(), .free = f.frees.seam(), .purge = .none };
    try f.map.create(81, .striped, 0);
    const mb = try f.alloc_a.allocateBlock();
    const b = Binding.wholeBlock(wt.VOL_A, mb.offset, mb.stamp);
    _ = try ep.record(81, 0, null, b, proofFor(b, wt.BS), 2 * wt.BS, block_alloc.InflightAllocGuard.register(f.alloc_a, mb.idx), true);
    // A publisher binds the block under the epoch (the visibility law
    // forbids it; the drift tripwire names it): the save displaces X,
    // which the epoch never parked.
    const mx = try f.alloc_a.allocateBlock();
    const x = Binding.wholeBlock(wt.VOL_A, mx.offset, mx.stamp);
    directPublish(io, f, 81, 0, x);
    const Fail = struct {
        fn hook(ctx: *anyopaque, _: *wt.RamMap) void {
            const fa: *std.testing.FailingAllocator = @ptrCast(@alignCast(ctx));
            fa.fail_index = fa.alloc_index;
            fa.resize_fail_index = fa.resize_index;
        }
    };
    f.map.before_merge = .{ .ctx = &failing, .call = &Fail.hook };
    stats.rewrite_shadow_displaced_drift.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    const out = ep.close(io, 81, cs);
    failing.fail_index = std.math.maxInt(usize);
    failing.resize_fail_index = std.math.maxInt(usize);
    try log.testing.expectAllFired();
    try testing.expectEqual(@as(u32, 1), out.swapped.freed);
    try testing.expectEqual(@as(usize, 1), f.frees.freed.items.len);
    try testing.expect(f.frees.freed.items[0].eql(x));
    try testing.expect(f.map.seam().lookup(81, 0).?.eql(b));
}

test "the ONE save's width caps the shadow: the first distinct block past it answers not_shadowed (the per-block durable publish) instead of being dropped from the save" {
    const io = testing.io;
    // Every A key published and every B minted: two blocks per shadowed index.
    const f = try wt.Fixture.init(testing.allocator, 2 * seams.MergeOutcome.max_displaced + 8);
    defer f.deinit();
    var ep = Epochs.init(testing.allocator, wt.BS, true);
    defer ep.deinit();
    const cs: Epochs.CloseSeams = .{ .map = f.map.seam(), .free = f.frees.seam(), .purge = .none };
    const fb0 = stats.rewrite_shadow_fallbacks.load();
    var block: u32 = 0;
    while (block < seams.MergeOutcome.max_displaced) : (block += 1) {
        const a = try f.publishBlock(70, block, 0x11);
        const m = try f.alloc_a.allocateBlock();
        const b = Binding.wholeBlock(wt.VOL_A, m.offset, m.stamp);
        try testing.expect((try ep.record(70, block, a, b, proofFor(b, wt.BS), 0, block_alloc.InflightAllocGuard.register(f.alloc_a, m.idx), true)) == .shadowed);
    }
    const a_extra = try f.publishBlock(70, block, 0x11);
    const m_extra = try f.alloc_a.allocateBlock();
    const b_extra = Binding.wholeBlock(wt.VOL_A, m_extra.offset, m_extra.stamp);
    try testing.expectEqual(RecordVerdict.not_shadowed, try ep.record(70, block, a_extra, b_extra, proofFor(b_extra, wt.BS), 0, block_alloc.InflightAllocGuard.register(f.alloc_a, m_extra.idx), true));
    try testing.expect(!f.alloc_a.inflightContains(m_extra.idx));
    try testing.expectEqual(fb0 + 1, stats.rewrite_shadow_fallbacks.load());
    // A re-record of a block the epoch already shadows is still taken.
    const m_again = try f.alloc_a.allocateBlock();
    const b_again = Binding.wholeBlock(wt.VOL_A, m_again.offset, m_again.stamp);
    try testing.expect((try ep.record(70, 0, null, b_again, proofFor(b_again, wt.BS), 0, block_alloc.InflightAllocGuard.register(f.alloc_a, m_again.idx), true)) == .shadowed);
    const out = ep.close(io, 70, cs);
    try testing.expectEqual(@as(u32, seams.MergeOutcome.max_displaced + 1), out.swapped.freed); // every A key + block 0's intermediate B
    try testing.expectEqual(@as(u8, seams.MergeOutcome.max_displaced), f.map.merges.items[0].blocks);
    try testing.expect(f.map.seam().lookup(70, 0).?.eql(b_again));
}

/// The durable-merge arm as the upload runs it on `.not_shadowed`: ONE
/// merge of `new` over the map's binding, the displaced key freed.
fn directPublish(io: Io, f: *wt.Fixture, ino: u64, block: u32, new: Binding) void {
    const out = f.map.seam().merge(.{ .ino = ino, .inserts = &.{.{ .block = block, .binding = new }}, .min_size = wt.BS, .proofs = &.{proofFor(new, wt.BS)}, .fencing = f.map.fencing }) catch unreachable;
    for (out.slice()) |d| _ = f.frees.seam().free(io, d);
}

// fstests generic/247 (a 500-block file overwritten by an O_DIRECT dd and a
// backward mmap writer at once): the epochs filled to the ONE save's size
// and closed back to back; a record for a block the CLOSING epoch still
// shadowed met the next epoch at its cap and took the durable merge under
// the running save — the save then displaced that fresh key (never parked:
// `rewrite_shadow_displaced_drift`) and freed the old A a second time
// (`block_untracked_free_refusals`, "REFUSED begin_free of block 315").
test "a record for a block the CLOSING epoch shadows never takes the durable merge under the save: the cap bounds the open shadow united with the closing one, so that block is always taken (parking the closing B) and a block past the union answers save_in_flight; the saves land B then C with A and B freed once each — drift 0, refusals 0" {
    const io = testing.io;
    const f = try wt.Fixture.init(testing.allocator, 2 * seams.MergeOutcome.max_displaced + 8);
    defer f.deinit();
    var ep = Epochs.init(testing.allocator, wt.BS, true);
    defer ep.deinit();
    const cs: Epochs.CloseSeams = .{ .map = f.map.seam(), .free = f.frees.seam(), .purge = .none };
    const max = seams.MergeOutcome.max_displaced;
    const drift0 = counters.tripwireCount("rewrite_shadow_displaced_drift");
    const refused0 = counters.tripwireCount("block_untracked_free_refusals");
    const a = try f.publishBlock(80, 0, 0xAA);
    const mb = try f.alloc_a.allocateBlock();
    const b = Binding.wholeBlock(wt.VOL_A, mb.offset, mb.stamp);
    _ = try ep.record(80, 0, a, b, proofFor(b, wt.BS), wt.BS, block_alloc.InflightAllocGuard.register(f.alloc_a, mb.idx), true);
    // In the save's window: the next epoch fills the union to the cap with
    // other blocks, one more block arrives, then a record for block 0 (the
    // upload's venue).
    const Racer = struct {
        var ep_: *Epochs = undefined;
        var f_: *wt.Fixture = undefined;
        var c: Binding = undefined;
        var p: Binding = undefined;
        var past: ?RecordVerdict = null;
        var verdict: ?RecordVerdict = null;
        fn hook(_: *anyopaque, map: *wt.RamMap) void {
            var block: u32 = 1;
            while (block < seams.MergeOutcome.max_displaced) : (block += 1) {
                const ai = f_.publishBlock(80, block, 0x11) catch unreachable;
                const m = f_.alloc_a.allocateBlock() catch unreachable;
                const bi = Binding.wholeBlock(wt.VOL_A, m.offset, m.stamp);
                std.debug.assert((ep_.record(80, block, ai, bi, proofFor(bi, wt.BS), 0, block_alloc.InflightAllocGuard.register(f_.alloc_a, m.idx), true) catch unreachable) == .shadowed);
            }
            const ap = f_.publishBlock(80, block, 0x11) catch unreachable;
            const mp = f_.alloc_a.allocateBlock() catch unreachable;
            p = Binding.wholeBlock(wt.VOL_A, mp.offset, mp.stamp);
            past = ep_.record(80, block, ap, p, proofFor(p, wt.BS), 0, block_alloc.InflightAllocGuard.register(f_.alloc_a, mp.idx), true) catch unreachable;
            const mc = f_.alloc_a.allocateBlock() catch unreachable;
            c = Binding.wholeBlock(wt.VOL_A, mc.offset, mc.stamp);
            verdict = ep_.record(80, 0, map.seam().lookup(80, 0), c, proofFor(c, wt.BS), wt.BS, block_alloc.InflightAllocGuard.register(f_.alloc_a, mc.idx), true) catch unreachable;
        }
    };
    Racer.ep_ = &ep;
    Racer.f_ = f;
    var dummy: u8 = 0;
    f.map.before_merge = .{ .ctx = &dummy, .call = &Racer.hook };
    try testing.expect(ep.close(io, 80, cs) == .swapped);
    try testing.expect(Racer.past.? == .save_in_flight);
    try testing.expect(Racer.verdict.? == .shadowed);
    try testing.expectEqual(drift0, counters.tripwireCount("rewrite_shadow_displaced_drift"));
    try testing.expectEqual(refused0, counters.tripwireCount("block_untracked_free_refusals"));
    // The save landed B over A; A freed once; nothing is in flight for the ino.
    try testing.expect(f.map.seam().lookup(80, 0).?.eql(b));
    try testing.expectEqual(@as(usize, 1), f.frees.freed.items.len);
    try testing.expect(f.frees.freed.items[0].eql(a));
    ep.awaitSaveSettled(io, 80);
    // The waiter records again: the open epoch is at the cap and no save is
    // in flight — the durable merge is now the right arm.
    const v2 = try ep.record(80, max, f.map.seam().lookup(80, max), Racer.p, proofFor(Racer.p, wt.BS), 0, block_alloc.InflightAllocGuard.register(f.alloc_a, f.alloc_a.idxOf(Racer.p.offset)), true);
    try testing.expect(v2 == .not_shadowed);
    directPublish(io, f, 80, max, Racer.p);
    // The next epoch's save publishes C over the landed B, freeing B with
    // the `max − 1` A keys it parked — each displacing exactly what it parked.
    const out = ep.close(io, 80, cs);
    try testing.expectEqual(@as(u32, max), out.swapped.freed);
    try testing.expectEqual(@as(usize, 2 + max), f.frees.freed.items.len);
    try testing.expectEqual(drift0, counters.tripwireCount("rewrite_shadow_displaced_drift"));
    try testing.expectEqual(refused0, counters.tripwireCount("block_untracked_free_refusals"));
    try testing.expect(f.map.seam().lookup(80, 0).?.eql(Racer.c));
    try testing.expect(f.map.seam().lookup(80, max).?.eql(Racer.p));
    try testing.expectEqual(@as(usize, 0), ep.openCount());
}

// A transient save folds the closing epoch into the one a record opened in
// its window (`reregister`); the next close's ONE save carries the UNION of
// their shadows — past `max_displaced` it tripped `closeEpoch`'s buffer
// assert (a panic: the daemon died).
test "a transient save's fold never carries more than the ONE save's size: a record in the save's window grows the open epoch only while its union with the closing shadow fits (past it: save_in_flight, the caller awaits and records again), so the folded epoch's close publishes every block in one merge" {
    const io = testing.io;
    const f = try wt.Fixture.init(testing.allocator, 2 * seams.MergeOutcome.max_displaced + 8);
    defer f.deinit();
    var ep = Epochs.init(testing.allocator, wt.BS, true);
    defer ep.deinit();
    const cs: Epochs.CloseSeams = .{ .map = f.map.seam(), .free = f.frees.seam(), .purge = .none };
    try f.map.create(90, .striped, 0);
    const max = seams.MergeOutcome.max_displaced;
    var block: u32 = 0;
    while (block < max / 2) : (block += 1) {
        const m = try f.alloc_a.allocateBlock();
        const b = Binding.wholeBlock(wt.VOL_A, m.offset, m.stamp);
        _ = try ep.record(90, block, null, b, proofFor(b, wt.BS), 0, block_alloc.InflightAllocGuard.register(f.alloc_a, m.idx), true);
    }
    // In the save's window a writer records `max` OTHER blocks; the save is transient.
    const Racer = struct {
        var ep_: *Epochs = undefined;
        var f_: *wt.Fixture = undefined;
        var shadowed: u32 = 0;
        var waited: std.ArrayList(u32) = .empty;
        fn hook(_: *anyopaque, map: *wt.RamMap) void {
            map.fail_next_merge = error.Transient;
            var blk: u32 = seams.MergeOutcome.max_displaced / 2;
            while (blk < seams.MergeOutcome.max_displaced * 3 / 2) : (blk += 1) {
                const m = f_.alloc_a.allocateBlock() catch unreachable;
                const b = Binding.wholeBlock(wt.VOL_A, m.offset, m.stamp);
                switch (ep_.record(90, blk, null, b, proofFor(b, wt.BS), 0, block_alloc.InflightAllocGuard.register(f_.alloc_a, m.idx), true) catch unreachable) {
                    .shadowed => shadowed += 1,
                    .save_in_flight => waited.append(testing.allocator, blk) catch unreachable,
                    .not_shadowed => unreachable,
                }
            }
        }
    };
    Racer.ep_ = &ep;
    Racer.f_ = f;
    Racer.shadowed = 0;
    Racer.waited = .empty;
    defer Racer.waited.deinit(testing.allocator);
    var dummy: u8 = 0;
    f.map.before_merge = .{ .ctx = &dummy, .call = &Racer.hook };
    try testing.expectEqual(CloseOutcome.transient, ep.close(io, 90, cs));
    // The fold: ONE open epoch whose shadow the next save carries whole.
    try testing.expectEqual(@as(u32, max), ep.table.get(90).?.shadow.count());
    try testing.expectEqual(@as(u32, max - max / 2), Racer.shadowed);
    try testing.expectEqual(@as(usize, max / 2), Racer.waited.items.len);
    try testing.expect(ep.close(io, 90, cs) == .swapped);
    try testing.expectEqual(@as(u8, max), f.map.merges.items[f.map.merges.items.len - 1].blocks);
    // The waiters record again over the settled save: a fresh epoch takes them.
    ep.awaitSaveSettled(io, 90);
    for (Racer.waited.items) |blk| {
        const m = try f.alloc_a.allocateBlock();
        const b = Binding.wholeBlock(wt.VOL_A, m.offset, m.stamp);
        try testing.expect((try ep.record(90, blk, null, b, proofFor(b, wt.BS), 0, block_alloc.InflightAllocGuard.register(f.alloc_a, m.idx), true)) == .shadowed);
    }
    try testing.expect(ep.close(io, 90, cs) == .swapped);
    block = 0;
    while (block < max * 3 / 2) : (block += 1) try testing.expect(f.map.seam().lookup(90, block) != null);
    try testing.expectEqual(@as(usize, 0), ep.openCount());
}

test "a shadow past the ONE save's size (record caps the fold's union, so it cannot happen) is loud and never lossy: rewrite_shadow_save_overflow fires, the close publishes and frees nothing, and the epoch stays open" {
    const io = testing.io;
    const f = try wt.Fixture.init(testing.allocator, seams.MergeOutcome.max_displaced + 8);
    defer f.deinit();
    var ep = Epochs.init(testing.allocator, wt.BS, true);
    defer ep.deinit();
    const cs: Epochs.CloseSeams = .{ .map = f.map.seam(), .free = f.frees.seam(), .purge = .none };
    try f.map.create(92, .striped, 0);
    const max = seams.MergeOutcome.max_displaced;
    var block: u32 = 0;
    while (block < max) : (block += 1) {
        const m = try f.alloc_a.allocateBlock();
        const b = Binding.wholeBlock(wt.VOL_A, m.offset, m.stamp);
        _ = try ep.record(92, block, null, b, proofFor(b, wt.BS), 0, block_alloc.InflightAllocGuard.register(f.alloc_a, m.idx), true);
    }
    const m = try f.alloc_a.allocateBlock();
    const extra = Binding.wholeBlock(wt.VOL_A, m.offset, m.stamp);
    try ep.table.get(92).?.shadow.put(ep.gpa, max, .{ .binding = extra, .proof = proofFor(extra, wt.BS) });
    const fired0 = counters.tripwireCount("rewrite_shadow_save_overflow");
    stats.rewrite_shadow_save_overflow.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(CloseOutcome.transient, ep.close(io, 92, cs));
    try log.testing.expectAllFired();
    try testing.expectEqual(fired0 + 1, counters.tripwireCount("rewrite_shadow_save_overflow"));
    try testing.expectEqual(@as(usize, 0), f.map.merges.items.len);
    try testing.expectEqual(@as(usize, 0), f.frees.freed.items.len);
    try testing.expectEqual(@as(usize, 1), ep.openCount());
    try testing.expectEqual(@as(u32, max + 1), ep.table.get(92).?.shadow.count());
}

test "record is all or nothing: an allocation failure at any of its reservations registers no epoch (a registered empty one went unreserved for the fold) and leaves the guard the caller's" {
    const f = try wt.Fixture.init(testing.allocator, 16);
    defer f.deinit();
    var failing = std.testing.FailingAllocator.init(testing.allocator, .{});
    try f.map.create(91, .striped, 0);
    const m = try f.alloc_a.allocateBlock();
    const b = Binding.wholeBlock(wt.VOL_A, m.offset, m.stamp);
    const open0 = stats.rewrite_shadow_open_epochs.load();
    // The five reservations of a first record: the table slot, the epoch,
    // its parked list, its shadow, its guards — the last first (a refused
    // `create` once left an undefined epoch pointer in the table).
    var i: usize = 5;
    while (i > 0) {
        i -= 1;
        var ep = Epochs.init(failing.allocator(), wt.BS, true);
        defer ep.deinit();
        failing.fail_index = failing.alloc_index + i;
        var g = block_alloc.InflightAllocGuard.register(f.alloc_a, m.idx);
        try testing.expectError(error.OutOfMemory, ep.record(91, 0, null, b, proofFor(b, wt.BS), wt.BS, g, true));
        g.deinit();
        try testing.expectEqual(@as(usize, 0), ep.openCount());
        try testing.expectEqual(open0, stats.rewrite_shadow_open_epochs.load());
    }
}

test "tie: the close width is half the ONE save's width (106 of 213), so two width-closed epochs fold into one save; the fold of a width-closed epoch with the half the stream filled under its save fits the cap exactly" {
    try testing.expectEqual(@as(usize, 106), CLOSE_WIDTH);
    try testing.expectEqual(seams.MergeOutcome.max_displaced / 2, CLOSE_WIDTH);
    try testing.expect(2 * CLOSE_WIDTH <= seams.MergeOutcome.max_displaced);
    // The room left under a saving width-closed epoch: the other half plus
    // the odd block — a stream records that many before any wait.
    try testing.expectEqual(@as(usize, 107), seams.MergeOutcome.max_displaced - CLOSE_WIDTH);
}

test "a FRESH stream rides the epoch and the close width owes ONE close per window: 2 × CLOSE_WIDTH + 5 fresh records (prev null, nothing parked) hint the close exactly at the width's record of each epoch and nowhere else past block 0's own coverage, each close is one merge of the width, and the final close lands the rest — ⌈N ÷ width⌉ + 1 saves, every block named, nothing freed" {
    const io = testing.io;
    const n: u32 = @intCast(2 * CLOSE_WIDTH + 5);
    const f = try wt.Fixture.init(testing.allocator, n + 8);
    defer f.deinit();
    var ep = Epochs.init(testing.allocator, wt.BS, true);
    defer ep.deinit();
    const cs: Epochs.CloseSeams = .{ .map = f.map.seam(), .free = f.frees.seam(), .purge = .none };
    try f.map.create(95, .striped, 0);
    const fresh0 = stats.rewrite_shadow_fresh_records.load();
    const hints0 = stats.rewrite_shadow_width_hints.load();
    const parked0 = stats.rewrite_shadow_parked_bytes.load();
    var hinted: std.ArrayList(u32) = .empty;
    defer hinted.deinit(testing.allocator);
    var block: u32 = 0;
    while (block < n) : (block += 1) {
        const m = try f.alloc_a.allocateBlock();
        const b = Binding.wholeBlock(wt.VOL_A, m.offset, m.stamp);
        // A sequential lay-down: every block complete, its size floor the block's end.
        const v = try ep.record(95, block, null, b, proofFor(b, wt.BS), (@as(u64, block) + 1) * wt.BS, block_alloc.InflightAllocGuard.register(f.alloc_a, m.idx), true);
        try testing.expect(v == .shadowed);
        try testing.expect(v.shadowed.displaced_prev == null);
        if (v.shadowed.close_owed) {
            try hinted.append(testing.allocator, block);
            // The guard-dropping caller's close: one merge of the epoch.
            try testing.expect(ep.close(io, 95, cs) == .swapped);
        }
    }
    try testing.expectEqual(parked0, stats.rewrite_shadow_parked_bytes.load());
    try testing.expectEqual(fresh0 + n, stats.rewrite_shadow_fresh_records.load());
    // Block 0 trips the coverage hint of a file growing from 0 (one block
    // shadowed, the floor one block); then exactly the width's record of
    // each epoch: blocks 1..106 close at 106, 107..212 at 212.
    const width: u32 = @intCast(CLOSE_WIDTH);
    try testing.expectEqualSlices(u32, &.{ 0, width, 2 * width }, hinted.items);
    try testing.expectEqual(hints0 + 2, stats.rewrite_shadow_width_hints.load());
    try testing.expectEqual(@as(usize, 3), f.map.merges.items.len);
    try testing.expectEqual(@as(u8, 1), f.map.merges.items[0].blocks);
    try testing.expectEqual(@as(u8, width), f.map.merges.items[1].blocks);
    try testing.expectEqual(@as(u8, width), f.map.merges.items[2].blocks);
    // The tail (4 blocks) waits for the next trigger: the final close.
    try testing.expectEqual(@as(usize, 1), ep.openCount());
    try testing.expect(ep.close(io, 95, cs) == .swapped);
    try testing.expectEqual(@as(usize, 4), f.map.merges.items.len);
    try testing.expectEqual(@as(u8, n - 1 - 2 * width), f.map.merges.items[3].blocks);
    try testing.expectEqual(@as(usize, (n + CLOSE_WIDTH - 1) / CLOSE_WIDTH + 1), f.map.merges.items.len);
    block = 0;
    while (block < n) : (block += 1) try testing.expect(f.map.seam().lookup(95, block) != null);
    try testing.expectEqual(@as(usize, 0), f.frees.freed.items.len);
    try testing.expectEqual(@as(u64, n * wt.BS), f.map.seam().view(95).?.size);
}

test "the width hint fires once per epoch: a re-record of a shadowed block at the width grows nothing and owes nothing; the record that reaches the width under a save in flight owes the close, and that close waits the save out then lands the epoch whole" {
    const io = testing.io;
    const f = try wt.Fixture.init(testing.allocator, 2 * seams.MergeOutcome.max_displaced + 8);
    defer f.deinit();
    var ep = Epochs.init(testing.allocator, wt.BS, true);
    defer ep.deinit();
    const cs: Epochs.CloseSeams = .{ .map = f.map.seam(), .free = f.frees.seam(), .purge = .none };
    try f.map.create(96, .striped, 0);
    const width: u32 = @intCast(CLOSE_WIDTH);
    var block: u32 = 1;
    while (block < width) : (block += 1) {
        const m = try f.alloc_a.allocateBlock();
        const b = Binding.wholeBlock(wt.VOL_A, m.offset, m.stamp);
        const v = try ep.record(96, block, null, b, proofFor(b, wt.BS), 0, block_alloc.InflightAllocGuard.register(f.alloc_a, m.idx), true);
        try testing.expect(!v.shadowed.close_owed);
    }
    // The width-th distinct block owes the close; a rewrite of it does not.
    const m = try f.alloc_a.allocateBlock();
    const b = Binding.wholeBlock(wt.VOL_A, m.offset, m.stamp);
    try testing.expect((try ep.record(96, width, null, b, proofFor(b, wt.BS), 0, block_alloc.InflightAllocGuard.register(f.alloc_a, m.idx), true)).shadowed.close_owed);
    const m2 = try f.alloc_a.allocateBlock();
    const b2 = Binding.wholeBlock(wt.VOL_A, m2.offset, m2.stamp);
    const again = try ep.record(96, width, f.map.seam().lookup(96, width), b2, proofFor(b2, wt.BS), 0, block_alloc.InflightAllocGuard.register(f.alloc_a, m2.idx), true);
    try testing.expect(again.shadowed.displaced_prev.?.eql(b)); // the intermediate B parked as ours
    try testing.expect(!again.shadowed.close_owed);
    // Under the save of this epoch a stream fills the next one to the
    // width: its width-th record owes the close, and the close parks on
    // the running save (closes of one ino serialize) then lands.
    const Racer = struct {
        var ep_: *Epochs = undefined;
        var f_: *wt.Fixture = undefined;
        var owed: ?bool = null;
        fn hook(_: *anyopaque, _: *wt.RamMap) void {
            var blk: u32 = width + 1;
            while (blk <= 2 * width) : (blk += 1) {
                const mm = f_.alloc_a.allocateBlock() catch unreachable;
                const bb = Binding.wholeBlock(wt.VOL_A, mm.offset, mm.stamp);
                const v = ep_.record(96, blk, null, bb, proofFor(bb, wt.BS), 0, block_alloc.InflightAllocGuard.register(f_.alloc_a, mm.idx), true) catch unreachable;
                std.debug.assert(v == .shadowed);
                if (blk == 2 * width) owed = v.shadowed.close_owed else std.debug.assert(!v.shadowed.close_owed);
            }
        }
    };
    Racer.ep_ = &ep;
    Racer.f_ = f;
    var dummy: u8 = 0;
    f.map.before_merge = .{ .ctx = &dummy, .call = &Racer.hook };
    const waits0 = stats.rewrite_shadow_close_waits.load();
    try testing.expect(ep.close(io, 96, cs) == .swapped);
    f.map.before_merge = null;
    try testing.expectEqual(@as(?bool, true), Racer.owed);
    try testing.expectEqual(@as(usize, 1), f.map.merges.items.len);
    try testing.expectEqual(@as(u8, width), f.map.merges.items[0].blocks);
    // The hinted close of the second epoch (the stream's) lands it whole.
    try testing.expect(ep.close(io, 96, cs) == .swapped);
    try testing.expectEqual(@as(usize, 2), f.map.merges.items.len);
    try testing.expectEqual(@as(u8, width), f.map.merges.items[1].blocks);
    try testing.expectEqual(waits0, stats.rewrite_shadow_close_waits.load()); // sequential here: no save was running at the second close
    try testing.expectEqual(@as(usize, 1), f.frees.freed.items.len); // the intermediate B alone
    try testing.expect(f.frees.freed.items[0].eql(b));
    try testing.expectEqual(@as(usize, 0), ep.openCount());
}

test {
    _ = @import("write_epoch_model.zig");
}
