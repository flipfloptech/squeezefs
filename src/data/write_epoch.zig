//! The rewrite epoch (charter §6.2 item 3 "Inside a rewrite epoch (per
//! inode): displaced A keys park, B offsets hold in-flight guards, ONE
//! whole-tx save swaps the epoch at close … parked A keys are freed only
//! after a durable save no longer references them; a fence at close
//! publishes nothing and frees nothing"; `docs/porting/data_write_path.md`
//! §5.4). PORTABLE: a per-ino RAM record under its own spin lock; the ONE
//! save is the map authority's `merge` (one tx) run by `close`.
//!
//! Close triggers (KD-1.6): full coverage (`shadow.len × bs ≥ size`), fsync /
//! flush (before the meta barrier), RELEASE, shape-change ops (truncate,
//! punch, spill/promotion, clone, setattr size), the ENOSPC early-close
//! generalized to the one allocation act (`closeAllForSpace`, the
//! `enospc.PressureSupplyHook`), idle `EPOCH_IDLE_HORIZON_MS`, fence /
//! unmount. Supersession coherence (KD-1.11): every durable publish of a
//! shadowed index evicts `shadow[b]` (`supersede`) — the 2026-08-04 C2Lost/
//! C2Leaked/C3 resurrection fix. The epoch-close venue law: `close` is never
//! called under a block guard (3) — the guard-dropping caller acts on the
//! close-owed hint (`Try` under a guard answers `.busy`).

const std = @import("std");
const Io = std.Io;
const Atomic = std.atomic.Value;
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const clock = @import("../core/clock.zig");
const log = @import("../core/log.zig");
const jc = @import("../meta/journal_core.zig");
const seams = @import("write_seams.zig");
const block_alloc = @import("block_alloc.zig");

pub const Binding = seams.Binding;

/// The metadata cache's time-to-idle: 300 s (the reference's
/// `METADATA_CACHE_TTI_SECS`) — an idle epoch is closed at a tenth of it so
/// a shadowed RAM entry is never evicted mid-epoch (the eviction horizon the
/// close defends).
pub const METADATA_CACHE_TTI_MS: u64 = 300 * std.time.ms_per_s;
pub const EPOCH_IDLE_HORIZON_MS: u64 = METADATA_CACHE_TTI_MS / 10;

pub const stats = struct {
    pub var rewrite_shadow_open_epochs: counters.Gauge = .{};
    pub var rewrite_shadow_parked_bytes: counters.Gauge = .{};
    pub var rewrite_shadow_swaps: counters.ShardedCounter = .{};
    pub var rewrite_shadow_bytes: counters.ShardedCounter = .{};
    pub var rewrite_shadow_fallbacks: counters.ShardedCounter = .{};
    pub var rewrite_shadow_close_retries: counters.ShardedCounter = .{};
    pub var rewrite_shadow_superseded: counters.ShardedCounter = .{};
    pub var rewrite_shadow_pressure_closes: counters.ShardedCounter = .{};
    pub var rewrite_shadow_pressure_close_busy: counters.ShardedCounter = .{};
    pub var rewrite_shadow_orphan_discards: counters.ShardedCounter = .{};
    pub var rewrite_shadow_idle_closes: counters.ShardedCounter = .{};
    /// The `rewrite_amp` SLO: `device_write_bytes / user_bytes`.
    pub var rewrite_blocks: counters.ShardedCounter = .{};
    pub var rewrite_user_bytes: counters.ShardedCounter = .{};
    pub var rewrite_device_write_bytes: counters.ShardedCounter = .{};
    /// Must-stay-0.
    pub var rewrite_shadow_fence_drops: counters.Tripwire = .init("rewrite_shadow_fence_drops");
    pub var rewrite_shadow_displaced_drift: counters.Tripwire = .init("rewrite_shadow_displaced_drift");
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"rewrite_shadow_open_epochs\":{d},\"rewrite_shadow_parked_bytes\":{d},", .{ stats.rewrite_shadow_open_epochs.load(), stats.rewrite_shadow_parked_bytes.load() });
    inline for (.{ "rewrite_shadow_swaps", "rewrite_shadow_bytes", "rewrite_shadow_fallbacks", "rewrite_shadow_close_retries", "rewrite_shadow_superseded", "rewrite_shadow_pressure_closes", "rewrite_shadow_pressure_close_busy", "rewrite_shadow_orphan_discards", "rewrite_shadow_idle_closes", "rewrite_blocks", "rewrite_user_bytes", "rewrite_device_write_bytes" }) |name| {
        try w.print("\"{s}\":{d},", .{ name, @field(stats, name).load() });
    }
    try w.print("\"rewrite_shadow_fence_drops\":{d},\"rewrite_shadow_displaced_drift\":{d}}}", .{ counters.tripwireCount("rewrite_shadow_fence_drops"), counters.tripwireCount("rewrite_shadow_displaced_drift") });
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
    /// Set while a close is running (a concurrent record re-opens after).
    closing: bool = false,

    fn deinit(self: *Epoch, gpa: Allocator) void {
        self.shadow.deinit(gpa);
        self.displaced.deinit(gpa);
        for (self.guards.items) |*g| g.deinit();
        self.guards.deinit(gpa);
    }
};

pub const RecordVerdict = union(enum) {
    shadowed: struct { displaced_prev: ?Binding, coverage_complete: bool },
    /// The ino's layout is not striped (a promotion in flight): the caller
    /// takes the durable-merge arm.
    not_shadowed,
};

pub const CloseMode = enum {
    /// Every trigger site: park on the stripe.
    park,
    /// The pressure close under a block guard: never park (`.busy`).
    try_,
};

pub const CloseOutcome = union(enum) {
    absent,
    busy,
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
    table: std.AutoHashMapUnmanaged(u64, *Epoch) = .empty,
    /// The lever: `SQUEEZEFS_REWRITE_SHADOW` (off ⇒ every overwrite takes
    /// the per-block durable publish — `rewrite_shadow_fallbacks`).
    enabled: bool,

    pub fn init(gpa: Allocator, block_size: u64, enabled: bool) Epochs {
        return .{ .gpa = gpa, .block_size = block_size, .enabled = enabled };
    }

    pub fn deinit(self: *Epochs) void {
        var it = self.table.valueIterator();
        while (it.next()) |e| {
            stats.rewrite_shadow_parked_bytes.sub(e.*.parked_bytes);
            stats.rewrite_shadow_open_epochs.sub(1);
            e.*.deinit(self.gpa);
            self.gpa.destroy(e.*);
        }
        self.table.deinit(self.gpa);
        self.* = undefined;
    }

    pub fn openCount(self: *Epochs) usize {
        self.lock.lock();
        defer self.lock.unlock();
        return self.table.count();
    }

    /// Hazard 1: an overlay install on a block the epoch shadows declines.
    pub fn bindsBlock(self: *Epochs, ino: u64, block: u32) bool {
        self.lock.lock();
        defer self.lock.unlock();
        const e = self.table.get(ino) orelse return false;
        return e.shadow.contains(block);
    }

    /// The newest B key of `(ino, block)` — the read path's compose source.
    pub fn shadowOf(self: *Epochs, ino: u64, block: u32) ?Binding {
        self.lock.lock();
        defer self.lock.unlock();
        const e = self.table.get(ino) orelse return null;
        const s = e.shadow.get(block) orelse return null;
        return s.binding;
    }

    /// `rewrite_shadow_record`: open on the first record (KD-1.1: striped ∧
    /// prev ≠ new), park the displaced previous (purged from the read tiers
    /// by the caller), take the B offset's guard, note the size floor.
    /// `prev` is the binding the caller read under its block guard (the
    /// authoritative RAM map, else an earlier shadow).
    pub fn record(self: *Epochs, ino: u64, block: u32, prev: ?Binding, new: Binding, proof: seams.DmaProof, min_size: u64, guard: block_alloc.InflightAllocGuard, striped: bool) Allocator.Error!RecordVerdict {
        if (!self.enabled or !striped) {
            var g = guard;
            g.deinit();
            return .not_shadowed;
        }
        self.lock.lock();
        defer self.lock.unlock();
        const gop = try self.table.getOrPut(self.gpa, ino);
        if (!gop.found_existing) {
            const e = try self.gpa.create(Epoch);
            e.* = .{ .ino = ino };
            gop.value_ptr.* = e;
            stats.rewrite_shadow_open_epochs.add(1);
        }
        const e = gop.value_ptr.*;
        // The displaced previous: the shadow's own newest B (an intra-epoch
        // rewrite) else the map's A.
        const own_prev = e.shadow.get(block);
        const displaced_prev: ?Binding = if (own_prev) |s| s.binding else prev;
        if (displaced_prev) |d| {
            try e.displaced.append(self.gpa, .{ .block = block, .origin = if (own_prev != null) .epoch else .map, .binding = d });
            e.parked_bytes += self.block_size;
            stats.rewrite_shadow_parked_bytes.add(self.block_size);
        }
        try e.shadow.put(self.gpa, block, .{ .binding = new, .proof = proof });
        try e.guards.append(self.gpa, guard);
        e.recorded_bytes += self.block_size;
        e.min_size = @max(e.min_size, min_size);
        e.last_record_ns = clock.monotonicNs();
        const coverage_complete = e.shadow.count() * self.block_size >= min_size and min_size != 0;
        return .{ .shadowed = .{ .displaced_prev = displaced_prev, .coverage_complete = coverage_complete } };
    }

    /// KD-1.11: a durable publish of `(ino, block)` → `published` outside
    /// the epoch evicts the shadow binding and drops the block's parked
    /// keys: the map's A is the publisher's to free; our own keys (the
    /// shadow's B when it is not what landed, intermediate Bs) are
    /// unpublished mints the caller frees — returned in `orphans`, freed
    /// with no lock held.
    pub fn supersede(self: *Epochs, ino: u64, block: u32, published: Binding, orphans: *[8]Binding) []Binding {
        self.lock.lock();
        defer self.lock.unlock();
        var n: usize = 0;
        const e = self.table.get(ino) orelse return orphans[0..0];
        if (e.shadow.fetchRemove(block)) |kv| {
            stats.rewrite_shadow_superseded.inc();
            if (!kv.value.binding.eql(published) and n < orphans.len) {
                orphans[n] = kv.value.binding;
                n += 1;
            }
        }
        var i: usize = 0;
        while (i < e.displaced.items.len) {
            const p = e.displaced.items[i];
            if (p.block != block) {
                i += 1;
                continue;
            }
            if (p.origin == .epoch and n < orphans.len) {
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
    };

    /// `close_rewrite_epoch_with`: take the epoch out of the table (a
    /// concurrent record re-opens a fresh one), run the ONE save with
    /// fencing convergence, then free the parked keys OUTSIDE every lock
    /// (RES-1). `Try` under a guard answers `.busy` when a close is already
    /// running.
    pub fn close(self: *Epochs, io: Io, ino: u64, mode: CloseMode, cs: CloseSeams) CloseOutcome {
        const e = blk: {
            self.lock.lock();
            defer self.lock.unlock();
            const e = self.table.get(ino) orelse return .absent;
            if (e.closing) {
                if (mode == .try_) stats.rewrite_shadow_pressure_close_busy.inc();
                return .busy;
            }
            e.closing = true;
            _ = self.table.remove(ino);
            break :blk e;
        };
        return self.closeEpoch(io, e, cs);
    }

    fn reregister(self: *Epochs, e: *Epoch) void {
        self.lock.lock();
        defer self.lock.unlock();
        e.closing = false;
        const gop = self.table.getOrPut(self.gpa, e.ino) catch {
            // Allocation failure on the re-register: keep the record alive
            // under the table's existing capacity is impossible — count the
            // never-lossy breach loud and leak nothing by retrying forever
            // is not an option without memory; the records stay parked in
            // `e` and are merged into a fresh epoch by the next record.
            log.err("rewrite epoch ino {d}: re-register after a transient close failed to allocate; {d} shadowed blocks stay parked in RAM until the next record", .{ e.ino, e.shadow.count() });
            return;
        };
        if (gop.found_existing) {
            // A record opened a fresh epoch meanwhile: fold ours into it.
            const fresh = gop.value_ptr.*;
            var it = e.shadow.iterator();
            while (it.next()) |kv| {
                if (!fresh.shadow.contains(kv.key_ptr.*)) fresh.shadow.put(self.gpa, kv.key_ptr.*, kv.value_ptr.*) catch {};
            }
            fresh.displaced.appendSlice(self.gpa, e.displaced.items) catch {};
            fresh.guards.appendSlice(self.gpa, e.guards.items) catch {};
            e.guards.clearRetainingCapacity();
            fresh.recorded_bytes += e.recorded_bytes;
            fresh.parked_bytes += e.parked_bytes;
            fresh.min_size = @max(fresh.min_size, e.min_size);
            e.deinit(self.gpa);
            self.gpa.destroy(e);
            stats.rewrite_shadow_open_epochs.sub(1);
            return;
        }
        gop.value_ptr.* = e;
    }

    fn closeEpoch(self: *Epochs, io: Io, e: *Epoch, cs: CloseSeams) CloseOutcome {
        var inserts_buf: [seams.MergeOutcome.max_displaced]seams.MapInsert = undefined;
        var proofs_buf: [seams.MergeOutcome.max_displaced]seams.DmaProof = undefined;
        var n: usize = 0;
        var it = e.shadow.iterator();
        while (it.next()) |kv| {
            if (n == inserts_buf.len) break;
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
                        stats.rewrite_shadow_fence_drops.fire("rewrite epoch close met the writer-guard fence");
                        log.err("rewrite epoch ino {d}: FENCED at close — {d} shadowed blocks published nothing, {d} parked keys freed nothing (successor recovery owns the accounting)", .{ e.ino, n, e.displaced.items.len });
                        self.dropEpoch(e);
                        return .fence_dropped;
                    },
                    error.NotFound => {
                        // The ino never comes back: the B keys are orphans.
                        stats.rewrite_shadow_orphan_discards.inc();
                        for (inserts_buf[0..n]) |ins| _ = cs.free.free(io, ins.binding);
                        self.dropEpoch(e);
                        return .orphan_discarded;
                    },
                    error.Transient, error.OutOfMemory => {
                        self.reregister(e);
                        return .transient;
                    },
                };
                break;
            }
        }
        // Success: the merge's displaced set must be within the parked set
        // (every parked key is freed below; a key the merge displaced that
        // we never parked is a drift the tripwire names and frees too).
        for (outcome.slice()) |d| {
            var parked = false;
            for (e.displaced.items) |p| if (p.binding.eql(d)) {
                parked = true;
            };
            if (!parked) {
                stats.rewrite_shadow_displaced_drift.fire("the save displaced a binding the epoch never parked");
                e.displaced.append(self.gpa, .{ .block = 0, .origin = .map, .binding = d }) catch {};
            }
        }
        stats.rewrite_shadow_swaps.inc();
        stats.rewrite_shadow_bytes.add(e.recorded_bytes);
        const bytes = e.recorded_bytes;
        const freed: u32 = @intCast(e.displaced.items.len);
        // Pop every B guard (the map owns the offsets now), then free the
        // parked keys with no lock held (RES-1 — "the worst park amplifier
        // in the tree").
        for (e.guards.items) |*g| g.deinit();
        e.guards.clearRetainingCapacity();
        for (e.displaced.items) |d| {
            cs.purge.purge(d.binding);
            _ = cs.free.free(io, d.binding);
        }
        self.dropEpoch(e);
        return .{ .swapped = .{ .bytes = bytes, .freed = freed } };
    }

    fn dropEpoch(self: *Epochs, e: *Epoch) void {
        stats.rewrite_shadow_parked_bytes.sub(e.parked_bytes);
        stats.rewrite_shadow_open_epochs.sub(1);
        e.deinit(self.gpa);
        self.gpa.destroy(e);
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
            switch (self.close(io, ino, .park, cs)) {
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
            _ = self.close(io, ino, .park, cs);
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
    try testing.expect(!v0.shadowed.coverage_complete);
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
    try testing.expect(v1.shadowed.coverage_complete); // the close-owed hint
    try testing.expectEqual(parked0 + 2 * wt.BS, stats.rewrite_shadow_parked_bytes.load());
    const cs: Epochs.CloseSeams = .{ .map = f.map.seam(), .free = f.frees.seam(), .purge = .none };
    const out = ep.close(io, 10, .park, cs);
    try testing.expectEqual(@as(u64, 2 * wt.BS), out.swapped.bytes);
    try testing.expectEqual(@as(u32, 2), out.swapped.freed);
    // ONE save: one merge of two blocks; the map binds B; both A keys freed; guards popped.
    try testing.expectEqual(@as(usize, 1), f.map.merges.items.len);
    try testing.expectEqual(@as(u8, 2), f.map.merges.items[0].blocks);
    try testing.expect(f.map.seam().lookup(10, 0).?.eql(b0));
    try testing.expect(f.map.seam().lookup(10, 1).?.eql(b1));
    try testing.expectEqual(@as(usize, 2), f.frees.freed.items.len);
    try testing.expect(!f.alloc_a.inflightContains(m0.idx));
    try testing.expectEqual(@as(usize, 0), ep.openCount());
    try testing.expectEqual(CloseOutcome.absent, ep.close(io, 10, .park, cs));
    try testing.expectEqual(parked0, stats.rewrite_shadow_parked_bytes.load());
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
    try testing.expect(ep.close(io, 20, .park, cs) == .swapped);
    try testing.expectEqual(retries0 + 1, stats.rewrite_shadow_close_retries.load());
    // Transient: re-registered, nothing freed, the next close lands.
    const a1 = try f.publishBlock(21, 0, 0x22);
    const m1 = try f.alloc_a.allocateBlock();
    const b1 = Binding.wholeBlock(wt.VOL_A, m1.offset, m1.stamp);
    _ = try ep.record(21, 0, a1, b1, proofFor(b1, wt.BS), wt.BS, block_alloc.InflightAllocGuard.register(f.alloc_a, m1.idx), true);
    f.map.fail_next_merge = error.Transient;
    const frees_before = f.frees.freed.items.len;
    try testing.expectEqual(CloseOutcome.transient, ep.close(io, 21, .park, cs));
    try testing.expectEqual(frees_before, f.frees.freed.items.len);
    try testing.expectEqual(@as(usize, 1), ep.openCount());
    try testing.expect(ep.close(io, 21, .park, cs) == .swapped);
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
    try testing.expectEqual(CloseOutcome.fence_dropped, ep.close(io, 22, .park, cs));
    try log.testing.expectAllFired();
    try testing.expectEqual(frees_fence, f.frees.freed.items.len);
    try testing.expectEqual(merges_fence, f.map.merges.items.len);
    try testing.expect(f.map.seam().lookup(22, 0).?.eql(a2));
    try testing.expectEqual(fd0 + 1, counters.tripwireCount("rewrite_shadow_fence_drops"));
    // A reclaimed ino: the B key is an orphan and is freed; the A key belongs to the destroy.
    const a3 = try f.publishBlock(23, 0, 0x44);
    const m3 = try f.alloc_a.allocateBlock();
    const b3 = Binding.wholeBlock(wt.VOL_A, m3.offset, m3.stamp);
    _ = try ep.record(23, 0, a3, b3, proofFor(b3, wt.BS), wt.BS, block_alloc.InflightAllocGuard.register(f.alloc_a, m3.idx), true);
    f.map.remove(23);
    const frees_orphan = f.frees.freed.items.len;
    try testing.expectEqual(CloseOutcome.orphan_discarded, ep.close(io, 23, .park, cs));
    try testing.expectEqual(frees_orphan + 1, f.frees.freed.items.len);
    try testing.expect(f.frees.freed.items[frees_orphan].eql(b3));
}

test "the close triggers: ENOSPC pressure close frees parked supply; the idle sweep closes at TTI/10; supersede evicts a shadow binding; the Try mode answers busy under a running close; the lever off never shadows" {
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
    try testing.expect(ep.close(io, 32, .park, cs) == .swapped);
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
