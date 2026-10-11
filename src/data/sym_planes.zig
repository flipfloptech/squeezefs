//! The data-plane planes under the symmetric model (charter §5.5 "Data-
//! plane planes under the symmetric model", §5.6, §6.2 block reclaim, §8
//! `df`; `docs/porting/sym_data_planes.md`): the allocation lease, the
//! `KVDA` data allocation bitmap, ranged block grants, shipped frees and
//! the free-grace ring, the shared-block index and the clone protocol,
//! the custody words — PORTABLE cores and models that fill the seams the
//! rung-4 allocator left (`block_alloc.{PlaneGate, FinishFreeSink,
//! GrantSource, AllocationLease, GraceRing, SharedFreeGate,
//! UntrackedFreeGate}`, `reclaim.FreeTarget`, `patch_w1.CustodyProbe.
//! sole_owner`). Device paths ride the existing seams (the extent-grant
//! pool, the home volume's `RegionIo`, the holder's ring as a
//! `DeltaJournal`, tree 0 as a `ControlStore`); the wire agent's verb
//! codecs bind the handlers here to the cluster wire.
//!
//! The one structural law every module shares (0002 row 237): on a grant-
//! armed allocator the bitmap IS the free list — the holder's `DataBitmap`
//! owns the allocator's `bits` words, the finish-free sink is "clear the
//! bit and journal the delta under ONE lock", a freed block is
//! reallocated only through a carve, and `df` is the population minus the
//! writer's own unconsumed window.

const std = @import("std");
const builtin = @import("builtin");
const counters = @import("../core/counters.zig");

pub const records = @import("sym_planes/records.zig");
pub const data_bitmap = @import("sym_planes/data_bitmap.zig");
pub const alloc_lease = @import("sym_planes/alloc_lease.zig");
pub const block_grant = @import("sym_planes/block_grant.zig");
pub const free_grace = @import("sym_planes/free_grace.zig");
pub const free_ship = @import("sym_planes/free_ship.zig");
pub const shared_index = @import("sym_planes/shared_index.zig");
pub const clone_protocol = @import("sym_planes/clone_protocol.zig");
pub const custody = @import("sym_planes/custody.zig");
pub const wire_bind = @import("sym_planes/wire_bind.zig");
/// The device-path arms over a mounted plane (Linux: the wire sessions).
pub const plane_arm = if (builtin.os.tag == .linux) @import("sym_planes/plane_arm.zig") else struct {
    pub const stats = struct {};
};

/// The `.stats` family: every module's instruments under its name (the
/// generated writer — a counter declared is a counter exported).
pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.writeByte('{');
    inline for (.{
        .{ "alloc_lease", alloc_lease.stats },
        .{ "data_alloc_bitmap", data_bitmap.stats },
        .{ "block_grant", block_grant.stats },
        .{ "free_grace", free_grace.stats },
        .{ "free_ship", free_ship.stats },
        .{ "shared_index", shared_index.stats },
        .{ "clone_protocol", clone_protocol.stats },
        .{ "custody", custody.stats },
        .{ "plane_arm", plane_arm.stats },
    }, 0..) |m, i| {
        if (i != 0) try w.writeByte(',');
        try w.print("\"{s}\":", .{m[0]});
        try counters.writeNamespaceJson(m[1], w);
    }
    try w.writeByte('}');
}

// ---------------------------------------------------------------------------
// Tests: the family and the two-writer in-process e2e.
// ---------------------------------------------------------------------------

const testing = std.testing;
const block_alloc = @import("block_alloc.zig");
const reclaim = @import("reclaim.zig");

test {
    std.testing.refAllDecls(@This());
    _ = records;
    _ = data_bitmap;
    _ = alloc_lease;
    _ = block_grant;
    _ = free_grace;
    _ = free_ship;
    _ = shared_index;
    _ = clone_protocol;
    _ = custody;
    _ = wire_bind;
    _ = plane_arm;
}

test "the stats family names every module's instruments by the registry names" {
    var buf: [16384]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    const s = w.buffered();
    for ([_][]const u8{ "\"alloc_lease\":{", "\"data_alloc_bitmap_population\":", "\"block_grant_window_remaining\":", "\"free_grace_deferrals\":", "\"free_shipped_blocks\":", "\"share_block_calls\":", "\"mark_shared_calls\":", "\"data_dma_epoch_refusals\":", "\"data_alloc_bitmap_drift\":", "\"dead_member_write_deferrals\":" }) |k| {
        try testing.expect(std.mem.indexOf(u8, s, k) != null);
    }
}

/// The two-writer fixture: one data volume, writer A (appender 1) holds
/// the lease and the bitmap; writer B (appender 2) is a joined appender
/// with its own allocator view, pulling grants from A over the in-process
/// loopback and shipping its frees to A's executor.
const TwoWriters = struct {
    const VOL_TAG: u64 = 0x2A;
    const BLOCKS: u64 = 1024;
    const A_ID: records.WriterIdentity = .{ .node_token = 1, .mount_slot = 0, .writer_id = 0xA1 };
    const B_ID: records.WriterIdentity = .{ .node_token = 2, .mount_slot = 0, .writer_id = 0xB1 };

    gpa: std.mem.Allocator,
    tree0: alloc_lease.RamControlStore,
    home: data_bitmap.RamHome,
    journal: data_bitmap.RamJournal,
    // A: the holder.
    a_alloc: *block_alloc.BlockAllocator,
    a_bitmap: *data_bitmap.DataBitmap,
    a_holder: block_grant.HolderGrants,
    a_lease: alloc_lease.LeaseArm,
    a_gate: custody.PlaneGateCore = .{},
    a_sink: block_grant.LocalSink,
    a_arm: block_grant.WriterArm,
    a_ledger: RefLedger = .{},
    a_executor: free_ship.FreeExecutor,
    a_ladder: Ladder,
    // B: the joiner.
    b_alloc: *block_alloc.BlockAllocator,
    b_lease: alloc_lease.LeaseArm = .{},
    b_gate: custody.PlaneGateCore = .{},
    b_sink: block_grant.LocalSink,
    b_arm: block_grant.WriterArm,
    b_client: free_ship.LocalClient,
    b_tracking: Tracking = .{},
    b_epoch: EpochSrc = .{},
    b_shipper: free_ship.Shipper,
    b_reclaimer: *reclaim.Reclaimer,

    /// The durable reference ledger (what the layout publishes wrote):
    /// one bit per referenced block and the count per block.
    const RefLedger = struct {
        words: [BLOCKS / 64]u64 = @splat(0),
        counts: [BLOCKS]u8 = @splat(0),
        fn publish(self: *RefLedger, idx: u64) void {
            self.words[idx / 64] |= @as(u64, 1) << @intCast(idx % 64);
            self.counts[idx] += 1;
        }
        fn release(self: *RefLedger, idx: u64) void {
            self.counts[idx] -= 1;
            if (self.counts[idx] == 0) self.words[idx / 64] &= ~(@as(u64, 1) << @intCast(idx % 64));
        }
        fn refcount(ctx: *anyopaque, _: u64, idx: u64) u32 {
            const l: *RefLedger = @ptrCast(@alignCast(ctx));
            return l.counts[idx];
        }
        fn durable(self: *RefLedger) free_ship.DurableRefcounts {
            return .{ .ctx = self, .refcount = &refcount };
        }
    };

    const Ladder = struct {
        alloc: *block_alloc.BlockAllocator,
        fn free(ctx: *anyopaque, idx: u64) reclaim.FreeOutcome {
            const l: *Ladder = @ptrCast(@alignCast(ctx));
            return switch (l.alloc.freeBlock(idx) catch return .refused) {
                .terminal => .terminal,
                .nonterminal => .nonterminal,
                .refused => .refused,
            };
        }
        fn seam(self: *Ladder) free_ship.FreeLadder {
            return .{ .ctx = self, .free = &free };
        }
    };

    const Tracking = struct {
        retired: u32 = 0,
        fn purge(_: *anyopaque, _: u64, _: u64) void {}
        fn retire(ctx: *anyopaque, _: u64, _: u64) void {
            const t: *Tracking = @ptrCast(@alignCast(ctx));
            t.retired += 1;
        }
        fn release(_: *anyopaque, _: u64, _: u64) void {}
        fn seams(self: *Tracking) free_ship.TrackingSeams {
            return .{ .ctx = self, .purge = &purge, .retire = &retire, .release = &release };
        }
    };

    const EpochSrc = struct {
        term: u64 = 1,
        fn epoch(ctx: *anyopaque, _: u64) u64 {
            const e: *EpochSrc = @ptrCast(@alignCast(ctx));
            return e.term;
        }
        fn seam(self: *EpochSrc) free_ship.LeaseEpochSource {
            return .{ .ctx = self, .epoch = &epoch };
        }
    };

    const Skip = struct {
        fn issue(_: ?*anyopaque, _: u64, _: u64) @import("../core/errors.zig").Error!void {}
    };

    fn knownBlocks(_: ?*anyopaque, vol_tag: u64) ?u64 {
        return if (vol_tag == VOL_TAG) BLOCKS else null;
    }
    const heaps = [_]data_bitmap.HeapGeometry{.{ .vol = 0, .heap_start = 0, .heap_len = 1 << 24, .node_size = 8192 }};
    const screen: alloc_lease.Screen = .{ .ctx = null, .dataVolumeBlocks = &knownBlocks, .set_width = 1, .heaps = &heaps };

    fn init(gpa: std.mem.Allocator) !*TwoWriters {
        const f = try gpa.create(TwoWriters);
        f.gpa = gpa;
        f.tree0 = alloc_lease.RamControlStore.init(gpa);
        f.home = try data_bitmap.RamHome.init(gpa, 8192, 16);
        f.journal = .{ .gpa = gpa };
        f.a_alloc = try block_alloc.BlockAllocator.init(gpa, .{ .vol_tag = VOL_TAG, .vol = 0, .block_size = 4096, .capacity_bytes = BLOCKS * 4096 });
        f.b_alloc = try block_alloc.BlockAllocator.init(gpa, .{ .vol_tag = VOL_TAG, .vol = 0, .block_size = 4096, .capacity_bytes = BLOCKS * 4096 });
        f.a_bitmap = try data_bitmap.DataBitmap.init(gpa, f.a_alloc, .{ .vol_tag = VOL_TAG, .term = 1, .blocks = BLOCKS, .ring_capacity = data_bitmap.ringCapacity(block_grant.BLOCK_GRANT_FLOOR, 2, 64) });
        f.a_bitmap.journal = f.journal.sink();
        f.a_holder = .{ .ledger = try block_grant.Ledger.init(gpa, 32), .bitmap = f.a_bitmap };
        f.a_lease = .{ .bitmap = f.a_bitmap, .term = 1 };
        f.a_gate = .{};
        f.a_gate.setHeld(true);
        f.a_sink = .{ .holder = &f.a_holder, .writer = 1 };
        f.a_arm = .{ .capacity_blocks = BLOCKS, .sink = f.a_sink.sink() };
        f.a_ledger = .{};
        f.a_ladder = .{ .alloc = f.a_alloc };
        f.a_executor = .{ .alloc = f.a_alloc, .durable = f.a_ledger.durable(), .ladder = f.a_ladder.seam() };
        f.a_lease.window_remaining = &block_grant.GrantWindow.remainingFn;
        f.a_lease.window_ctx = &f.a_arm.window;
        f.a_alloc.lease = f.a_lease.lease();
        f.a_alloc.grant_source = f.a_arm.grantSource();
        f.a_alloc.finish_free_sink = f.a_bitmap.finishFreeSink();
        f.a_alloc.plane_gate = f.a_gate.gate();
        f.a_alloc.untracked_gate = f.a_executor.untrackedGate();
        // B.
        f.b_lease = .{};
        f.b_gate = .{};
        f.b_sink = .{ .holder = &f.a_holder, .writer = 2 };
        f.b_arm = .{ .capacity_blocks = BLOCKS, .sink = f.b_sink.sink() };
        f.b_lease.window_remaining = &block_grant.GrantWindow.remainingFn;
        f.b_lease.window_ctx = &f.b_arm.window;
        f.b_alloc.lease = f.b_lease.lease();
        f.b_alloc.grant_source = f.b_arm.grantSource();
        f.b_client = .{ .executor = &f.a_executor };
        f.b_tracking = .{};
        f.b_epoch = .{};
        f.b_shipper = .{ .vol_tag = VOL_TAG, .client = f.b_client.client(), .tracking = f.b_tracking.seams(), .lease_epoch = f.b_epoch.seam() };
        f.b_reclaimer = try reclaim.Reclaimer.init(gpa, .{ .budget_bytes = 1 << 30, .batch_ms = 0 });
        f.b_reclaimer.registerTarget(0, f.b_alloc, .{ .ctx = null, .substrate = .skip, .issue = &Skip.issue });
        f.b_reclaimer.setFreeTarget(0, f.b_shipper.freeTarget());
        return f;
    }

    fn deinit(f: *TwoWriters) void {
        f.b_reclaimer.deinit();
        f.a_holder.ledger.deinit();
        f.a_bitmap.deinit();
        f.a_alloc.deinit();
        f.b_alloc.deinit();
        f.journal.deinit();
        f.home.deinit();
        f.tree0.deinit();
        f.gpa.destroy(f);
    }

    /// A writer mints and publishes `n` blocks (the layout publish writes
    /// the durable reference); its window is topped up as the PULL law says.
    fn mintPublish(f: *TwoWriters, alloc: *block_alloc.BlockAllocator, arm: *block_grant.WriterArm, n: usize, out: []u64) !void {
        for (out[0..n]) |*o| {
            if (arm.window.wantsTopup()) _ = arm.topup();
            const m = try alloc.allocateBlock();
            alloc.publishBlock(m.idx);
            arm.meter.note(1, @import("../core/clock.zig").monotonicNs());
            f.a_ledger.publish(m.idx);
            o.* = m.idx;
        }
    }
};

test "two writers in process: A holds the lease; B pulls grants, writes, its frees ship to A; A dies ⇒ B succeeds only after recovered:, adopts its own window and releases the rest; df per the rule; C6/C8 clean" {
    const gpa = testing.allocator;
    const f = try TwoWriters.init(gpa);
    defer f.deinit();
    const T = TwoWriters;
    const st = f.tree0.store();

    // 1. A acquires the lease at term 1 and publishes its pages on its home volume.
    const g = alloc_lease.acquire(st, gpa, T.screen, true, .{ .vol_tag = T.VOL_TAG, .identity = T.A_ID, .home_vol = 0, .appender_id = 1, .blocks = T.BLOCKS }).granted;
    try testing.expectEqual(@as(u64, 1), g.term);
    const image = try gpa.alloc(u8, @intCast(data_bitmap.regionLen(T.BLOCKS)));
    defer gpa.free(image);
    f.a_bitmap.regionImage(1, image);
    var refs: std.ArrayList(records.BitmapRef) = .empty;
    defer refs.deinit(gpa);
    try data_bitmap.holdPages(gpa, f.home.pool(), f.home.io(), 0, image, &refs);
    try testing.expectEqual(alloc_lease.BitmapVerdict.published, alloc_lease.publishBitmap(st, gpa, T.screen, .{ .vol_tag = T.VOL_TAG, .identity = T.A_ID, .term = 1, .refs = refs.items }));
    // B cannot take the lease while A is live.
    try testing.expectEqual(alloc_lease.AcquireVerdict.busy, alloc_lease.acquire(st, gpa, T.screen, true, .{ .vol_tag = T.VOL_TAG, .identity = T.B_ID, .home_vol = 0, .appender_id = 2, .blocks = T.BLOCKS }));

    // 2. B pulls grants and writes 100 blocks; A writes 50: disjoint ranges.
    var b_blocks: [100]u64 = undefined;
    var a_blocks: [50]u64 = undefined;
    try f.mintPublish(f.b_alloc, &f.b_arm, 100, &b_blocks);
    try f.mintPublish(f.a_alloc, &f.a_arm, 50, &a_blocks);
    for (b_blocks) |x| for (a_blocks) |y| try testing.expect(x != y);
    const pop_after_mints = f.a_bitmap.populationNow();
    try testing.expect(pop_after_mints >= 150);
    // Every carve's SET is durable before the grant reached a writer.
    try testing.expectEqual(@as(usize, 0), f.a_bitmap.queuedDeltas());

    // 3. B frees 20 of its blocks: the ladder SHIPS to A, whose executor
    // clears the bits through the sink and journals the CLEARs.
    const io = testing.io;
    for (b_blocks[0..20]) |idx| {
        f.a_ledger.release(idx);
        try testing.expectEqual(reclaim.FreeOutcome.nonterminal, f.b_reclaimer.freeBlock(io, 0, idx, f.b_alloc.stampOf(idx)));
    }
    try testing.expectEqual(free_ship.Shipper.FlushOutcome.served, f.b_shipper.flush());
    try testing.expectEqual(@as(u32, 20), f.b_tracking.retired);
    try testing.expectEqual(pop_after_mints - 20, f.a_bitmap.populationNow());
    for (b_blocks[0..20]) |idx| try testing.expect(!f.a_bitmap.isSet(idx));
    try testing.expectEqual(data_bitmap.DataBitmap.DrainOutcome.drained, f.a_bitmap.drain());

    // 4. df per the rule: used = population − own unconsumed window.
    try testing.expectEqual(f.a_bitmap.populationNow() - f.a_arm.window.remaining(), f.a_alloc.usedBlocks());
    _ = f.b_arm.topup(); // B's word refreshes on its next pull (0002 row 241)
    f.b_lease.noteHolderWord(f.b_arm.last_word.?.blocks, f.b_arm.last_word.?.population, f.b_arm.last_word.?.frontier);
    try testing.expectEqual(f.b_arm.last_word.?.population - f.b_arm.window.remaining(), f.b_alloc.usedBlocks());
    try testing.expectEqual(T.BLOCKS - f.a_bitmap.populationNow() + f.a_arm.window.remaining(), f.a_alloc.freeSupplyBlocks());

    // 5. A dies: its death is recorded; B's acquire is DEFERRED until the
    // volume manager replayed A's term-1 deltas onto A's pages and wrote
    // recovered:.
    var b_decl: std.ArrayList(block_grant.BlockGrant) = .empty;
    defer b_decl.deinit(gpa);
    try block_grant.heldBlockRanges(&f.b_arm.window, f.b_alloc, gpa, &b_decl);
    try testing.expect(block_grant.screenWindowDecl(b_decl.items, T.BLOCKS));
    try alloc_lease.recordDeath(st, gpa, T.A_ID, .{ .epoch = 1, .ts_realtime_ns = 1000, .pr_key = 0, .early = false });
    try testing.expectEqual(alloc_lease.AcquireVerdict.deferred, alloc_lease.acquire(st, gpa, T.screen, true, .{ .vol_tag = T.VOL_TAG, .identity = T.B_ID, .home_vol = 0, .appender_id = 2, .blocks = T.BLOCKS }));
    // The recovery: the pages as written at the hold (an image at the
    // first checkpoint) plus the kept window of term-1 deltas.
    const recovered_image = try data_bitmap.readRegionImage(gpa, f.home.io(), refs.items, T.BLOCKS);
    defer gpa.free(recovered_image);
    try alloc_lease.recordRecovered(st, gpa, T.A_ID, 0, 1, 2000);
    const g2 = alloc_lease.acquire(st, gpa, T.screen, true, .{ .vol_tag = T.VOL_TAG, .identity = T.B_ID, .home_vol = 0, .appender_id = 2, .blocks = T.BLOCKS }).granted;
    try testing.expectEqual(@as(u64, 2), g2.term);
    try testing.expect(g2.refs.eql(refs.items)); // the predecessor's pages until B copies

    // 6. B holds: its bitmap over ITS allocator loads A's pages, folds the
    // term-1 deltas, and runs the census oracle — no LOSS; the LEAKs are
    // A's and B's unconsumed windows the dead RAM ledger granted.
    const b_bitmap = try data_bitmap.DataBitmap.init(gpa, f.b_alloc, .{ .vol_tag = T.VOL_TAG, .term = 2, .blocks = T.BLOCKS, .ring_capacity = data_bitmap.ringCapacity(block_grant.BLOCK_GRANT_FLOOR, 2, 64) });
    defer b_bitmap.deinit();
    var b_journal: data_bitmap.RamJournal = .{ .gpa = gpa };
    defer b_journal.deinit();
    b_bitmap.journal = b_journal.sink();
    _ = try b_bitmap.loadRegionImage(recovered_image);
    const recs = try f.journal.replayRecords(gpa);
    defer data_bitmap.RamJournal.freeReplay(gpa, recs);
    _ = try b_bitmap.replay(recs, 1);
    try testing.expectEqual(f.a_bitmap.populationNow(), b_bitmap.populationNow());
    var b_holder: block_grant.HolderGrants = .{ .ledger = try block_grant.Ledger.init(gpa, 32), .bitmap = b_bitmap };
    defer b_holder.ledger.deinit();
    var granted: [T.BLOCKS / 64]u64 = @splat(0);
    b_holder.ledger.openRangesInto(&granted);
    var leaks: std.ArrayList(u64) = .empty;
    defer leaks.deinit(gpa);
    const drift = try b_bitmap.verifyBitmap(&f.a_ledger.words, &granted, &leaks);
    try testing.expectEqual(@as(u64, 0), drift.loss); // C6: no referenced block reads clear
    try testing.expectEqual(f.a_arm.window.remaining() + f.b_arm.window.remaining(), drift.leak);
    var dl = try block_grant.DeferredLeaks.init(gpa, T.BLOCKS);
    defer dl.deinit();
    dl.deferLeaks(leaks.items);
    // B's own declared window is adopted under its name; A is dead (never
    // declares), so the remainder — A's window — is released.
    const adopted = dl.adoptDeclared(&b_holder.ledger, b_bitmap, 2, b_decl.items);
    try testing.expectEqual(f.b_arm.window.remaining(), adopted);
    const released = dl.convergeDeferred(b_bitmap, true);
    try testing.expectEqual(f.a_arm.window.remaining(), released);
    try testing.expect(dl.closureHolds());
    // B's first carve as the holder lands above the adopted frontier.
    const frontier = b_holder.ledger.grantFrontier();
    const first = b_holder.blockGrant(3, 0, 0).granted.grants.buf[0];
    try testing.expect(first.start >= frontier);
    // C8: every referenced block's RAM count at its minter equals the ledger's.
    for (b_blocks[20..]) |idx| try testing.expectEqual(@as(u32, f.a_ledger.counts[idx]), f.b_alloc.refcount(idx).?);
    for (a_blocks) |idx| try testing.expectEqual(@as(u32, f.a_ledger.counts[idx]), f.a_alloc.refcount(idx).?);
    // B publishes its copy and the lease names its pages.
    var b_refs: std.ArrayList(records.BitmapRef) = .empty;
    defer b_refs.deinit(gpa);
    const b_image = try gpa.alloc(u8, @intCast(data_bitmap.regionLen(T.BLOCKS)));
    defer gpa.free(b_image);
    b_bitmap.regionImage(2, b_image);
    try data_bitmap.holdPages(gpa, f.home.pool(), f.home.io(), 0, b_image, &b_refs);
    try testing.expectEqual(alloc_lease.BitmapVerdict.published, alloc_lease.publishBitmap(st, gpa, T.screen, .{ .vol_tag = T.VOL_TAG, .identity = T.B_ID, .term = 2, .refs = b_refs.items }));
    try testing.expect(alloc_lease.acquire(st, gpa, T.screen, true, .{ .vol_tag = T.VOL_TAG, .identity = T.B_ID, .home_vol = 0, .appender_id = 2, .blocks = T.BLOCKS }).granted.refs.eql(b_refs.items));
    try testing.expect(b_bitmap.populationDriftCheck());
}

test "the dismount defect's core: a mint after the grant plane is DISARMED meets the carved window's SET bit at the frontier ⇒ a typed PlaneRefused + the anomaly tripwire, never an assert, never StorageFull; the cursor stays; re-armed, the window mints again" {
    const gpa = testing.allocator;
    const f = try TwoWriters.init(gpa);
    defer f.deinit();
    // A (the holder) mints 15 striped blocks: ONE carve of the floor window
    // SETS 64 bits at the frontier; the cursor stands at 15.
    var a_blocks: [15]u64 = undefined;
    try f.mintPublish(f.a_alloc, &f.a_arm, 15, &a_blocks);
    try testing.expectEqual(@as(u64, 15), f.a_alloc.highestBlockIndex());
    try testing.expect(f.a_alloc.bitIsSet(15));
    try testing.expect(f.a_arm.window.remaining() >= block_grant.BLOCK_GRANT_FLOOR - 15);
    // The grant plane is torn down (`HolderPlane.disarm`'s word) while the
    // staged residents still wait for their dismount promotion.
    const src = f.a_alloc.grant_source;
    f.a_alloc.grant_source = null;
    const pop0 = f.a_bitmap.populationNow();
    const anomalies0 = counters.tripwireCount("block_claim_anomalies");
    block_alloc.stats.block_claim_anomalies.resetLogBudgetForTest();
    @import("../core/log.zig").testing.expectErrorLogs(1);
    try testing.expectError(error.PlaneRefused, f.a_alloc.allocateBlock());
    try @import("../core/log.zig").testing.expectAllFired();
    try testing.expectEqual(anomalies0 + 1, counters.tripwireCount("block_claim_anomalies"));
    // Retryable, never the terminal latch; the cursor did not walk over the
    // window (a walk would mint past the holder's carve without a grant).
    try testing.expect(!f.a_alloc.freshSupplyExhausted());
    try testing.expectEqual(@as(u64, 15), f.a_alloc.highestBlockIndex());
    try testing.expectEqual(pop0, f.a_bitmap.populationNow());
    // Re-armed (the fix's order: the dismount pass runs BEFORE the plane
    // disarms), the same window mints block 15.
    f.a_alloc.grant_source = src;
    try testing.expectEqual(@as(u64, 15), (try f.a_alloc.allocateBlock()).idx);
}
