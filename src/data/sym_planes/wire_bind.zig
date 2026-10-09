//! The data planes' verb handlers on the cluster wire (charter §5.5 verbs
//! `BlockGrant/ReturnBlocks`, `AllocLeaseAcquire/Bitmap/Release`,
//! `FreeBlocks`; `docs/porting/sym_data_planes.md` §1.2–§1.3, §3.4, §4.1):
//! thin adapters from `wire/service.zig`'s `Handler` contract onto the
//! portable cores — the SCREEN runs in the core, the wire integer is never
//! an allocation authority, and every answer is the ONE status table's
//! word (`ok` / `rejected` / `deferred` / `refused`). The handlers build
//! their variable tails in the session's scratch; nothing allocates per
//! request. PORTABLE (the loopback hub serves them in-process; the TCP
//! sessions on Linux).

const std = @import("std");
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const service = @import("../../wire/service.zig");
const verbs = @import("../../wire/verbs.zig");
const records = @import("records.zig");
const alloc_lease = @import("alloc_lease.zig");
const block_grant = @import("block_grant.zig");
const free_ship = @import("free_ship.zig");

pub const Verb = verbs.Verb;
pub const Answer = service.Answer;

fn identityOf(w: verbs.WireIdentity) records.WriterIdentity {
    return .{ .node_token = w.node_token, .mount_slot = w.mount_slot, .writer_id = w.writer_id };
}

fn refsFromWire(items: []align(1) const verbs.BitmapRefWire, out: *alloc_lease.Refs) bool {
    if (items.len > records.BITMAP_REFS_MAX) return false;
    for (items, 0..) |r, i| out.buf[i] = .{ .vol = r.vol, .start = r.start, .len = r.len };
    out.n = @intCast(items.len);
    return true;
}

/// Volume 0's manager face of the allocation lease: the three verbs over a
/// `ControlStore` and the arm's `Screen`.
pub const LeaseHandlers = struct {
    gpa: Allocator,
    store: alloc_lease.ControlStore,
    screen: alloc_lease.Screen,
    is_vol0_manager: bool,

    pub fn register(self: *LeaseHandlers, svc: *service.Service) void {
        svc.register(.alloc_lease_acquire, .{ .ctx = self, .serve = &serveAcquire });
        svc.register(.alloc_lease_bitmap, .{ .ctx = self, .serve = &serveBitmap });
        svc.register(.alloc_lease_release, .{ .ctx = self, .serve = &serveRelease });
    }

    fn serveAcquire(ctx: *anyopaque, req: *const service.Request, scratch: []u8) Answer {
        const self: *LeaseHandlers = @ptrCast(@alignCast(ctx));
        const r = req.payload.alloc_lease_acquire;
        const verdict = alloc_lease.acquire(self.store, self.gpa, self.screen, self.is_vol0_manager, .{ .vol_tag = r.vol_tag, .identity = identityOf(r.identity), .home_vol = r.home_vol, .appender_id = r.appender_id, .blocks = r.blocks });
        switch (verdict) {
            .granted => |g| {
                // The predecessor's refs ride the reply tail in the scratch.
                const need = g.refs.n * @sizeOf(verbs.BitmapRefWire);
                if (scratch.len < need) return Answer.refused(error.Unexpected, "reply scratch too small for the bitmap refs");
                const tail: []align(1) verbs.BitmapRefWire = @ptrCast(scratch[0..need]);
                for (g.refs.slice(), 0..) |ref, i| tail[i] = .{ .start = ref.start, .len = ref.len, .vol = ref.vol, .reserved = @splat(0) };
                return .{ .ok = .{ .alloc_lease_acquire = .{ .head = .{ .already = @intFromBool(g.already), .reserved = 0, .n_bitmap = g.refs.n, .reserved2 = 0, .term = g.term, .predecessor_blocks = g.blocks }, .items = tail } } };
            },
            .busy => return Answer.refused(error.Busy, "a live holder's lease moves only by its release; allocate through block grants from the holder"),
            .deferred => return Answer.deferred("the dead holder's region is not yet recovered (or the ring-0 window is full)"),
            .rejected => |why| return Answer.rejected(@tagName(why)),
            .not_manager => return .{ .status = .{ .status = .not_manager } },
            .io_error => return Answer.refused(error.InputOutput, "tree 0 could not be read or written"),
        }
    }

    fn serveBitmap(ctx: *anyopaque, req: *const service.Request, _: []u8) Answer {
        const self: *LeaseHandlers = @ptrCast(@alignCast(ctx));
        const r = req.payload.alloc_lease_bitmap;
        var refs: alloc_lease.Refs = .{};
        if (!refsFromWire(r.items, &refs)) return Answer.rejected("bitmap ref count past the run cap");
        return switch (alloc_lease.publishBitmap(self.store, self.gpa, self.screen, .{ .vol_tag = r.head.vol_tag, .identity = identityOf(r.head.identity), .term = r.head.term, .refs = refs.slice() })) {
            .published => .{ .ok = .{ .alloc_lease_bitmap = verbs.Ack.fresh } },
            .already => .{ .ok = .{ .alloc_lease_bitmap = verbs.Ack.replay } },
            .busy => Answer.refused(error.Busy, "not the holder at that term"),
            .rejected => |why| Answer.rejected(@tagName(why)),
            .deferred => Answer.deferred("the ring-0 window is full"),
            .io_error => Answer.refused(error.InputOutput, "tree 0 could not be read or written"),
        };
    }

    fn serveRelease(ctx: *anyopaque, req: *const service.Request, _: []u8) Answer {
        const self: *LeaseHandlers = @ptrCast(@alignCast(ctx));
        const r = req.payload.alloc_lease_release;
        return switch (alloc_lease.release(self.store, self.gpa, r.vol_tag, identityOf(r.identity), r.term)) {
            .released => .{ .ok = .{ .alloc_lease_release = verbs.Ack.fresh } },
            .already => .{ .ok = .{ .alloc_lease_release = verbs.Ack.replay } },
            .deferred => Answer.deferred("the ring-0 window is full"),
            .io_error => Answer.refused(error.InputOutput, "tree 0 could not be read or written"),
        };
    }
};

/// The allocation HOLDER's face: `BlockGrant` / `ReturnBlocks` over one
/// data volume's `HolderGrants`, `FreeBlocks` over its `FreeExecutor`.
/// Writers are keyed by appender id; the wire carries the identity, and
/// the holder's appender directory maps it (the `writerId` seam).
pub const HolderHandlers = struct {
    vol_tag: u64,
    grants: *block_grant.HolderGrants,
    frees: *free_ship.FreeExecutor,
    /// The appender id of a wire identity (null = unknown writer: refused).
    writer_ctx: *anyopaque,
    writerId: *const fn (*anyopaque, records.WriterIdentity) ?u32,

    pub fn register(self: *HolderHandlers, svc: *service.Service) void {
        svc.register(.block_grant, .{ .ctx = self, .serve = &serveGrant });
        svc.register(.return_blocks, .{ .ctx = self, .serve = &serveReturn });
        svc.register(.free_blocks, .{ .ctx = self, .serve = &serveFrees });
    }

    pub fn serveGrant(ctx: *anyopaque, req: *const service.Request, scratch: []u8) Answer {
        const self: *HolderHandlers = @ptrCast(@alignCast(ctx));
        const r = req.payload.block_grant;
        if (r.vol_tag != self.vol_tag) return Answer.refused(error.Busy, "this mount does not hold the lease of that volume");
        const writer = self.writerId(self.writer_ctx, identityOf(r.writer)) orelse return Answer.rejected("unknown writer identity");
        switch (self.grants.blockGrant(writer, r.want, r.held_unconsumed)) {
            .granted => |g| {
                const need = g.grants.n * @sizeOf(verbs.GrantRunWire);
                if (scratch.len < need) return Answer.refused(error.Unexpected, "reply scratch too small for the grant runs");
                const tail: []align(1) verbs.GrantRunWire = @ptrCast(scratch[0..need]);
                for (g.grants.slice(), 0..) |run, i| tail[i] = .{ .start = run.start, .len = run.len, .reserved = 0 };
                return .{ .ok = .{ .block_grant = .{ .head = .{ .kind = @backingInt(verbs.BlocksGrantedKind.granted), .already = @intFromBool(g.already), .n = g.grants.n, .reserved = 0, .blocks = g.word.blocks, .population = g.word.population, .frontier = g.word.frontier }, .items = tail } } };
            },
            .full => |word| return .{ .ok = .{ .block_grant = .{ .head = .{ .kind = @backingInt(verbs.BlocksGrantedKind.full), .already = 0, .n = 0, .reserved = 0, .blocks = word.blocks, .population = word.population, .frontier = word.frontier }, .items = &.{} } } },
            .deferred => return Answer.deferred("the holder's ring-0 user window is full for the beat"),
            .busy => return Answer.refused(error.Busy, "this mount does not hold the lease"),
        }
    }

    pub fn serveReturn(ctx: *anyopaque, req: *const service.Request, _: []u8) Answer {
        const self: *HolderHandlers = @ptrCast(@alignCast(ctx));
        const r = req.payload.return_blocks;
        if (r.vol_tag != self.vol_tag) return Answer.refused(error.Busy, "this mount does not hold the lease of that volume");
        const writer = self.writerId(self.writer_ctx, identityOf(r.writer)) orelse return Answer.rejected("unknown writer identity");
        return switch (self.grants.returnBlocks(writer, r.start, r.len)) {
            .returned => |n| .{ .ok = .{ .return_blocks = .{ .present = 1, .reserved = @splat(0), .cleared = n } } },
            .not_granted => .{ .ok = .{ .return_blocks = .{ .present = 0, .reserved = @splat(0), .cleared = 0 } } },
            .deferred => Answer.deferred("the holder's ring-0 user window is full"),
        };
    }

    pub fn serveFrees(ctx: *anyopaque, req: *const service.Request, scratch: []u8) Answer {
        const self: *HolderHandlers = @ptrCast(@alignCast(ctx));
        const r = req.payload.free_blocks;
        if (r.head.vol_tag != self.vol_tag) return Answer.refused(error.Busy, "this mount does not hold the lease of that volume");
        if (r.items.len > free_ship.FREE_BATCH_MAX) return Answer.rejected("free batch past the quantum");
        var blocks: [free_ship.FREE_BATCH_MAX]u64 = undefined;
        for (r.items, 0..) |b, i| blocks[i] = b;
        // The request id is the frame's correlation: a resend carries the
        // same one, so the dedup window keys `(lease_epoch, request_id)`.
        const v = self.frees.execute(.{ .vol_tag = r.head.vol_tag, .blocks = blocks[0..r.items.len], .lease_epoch = r.head.lease_epoch, .request_id = req.correlation });
        if (scratch.len < v.n) return Answer.refused(error.Unexpected, "reply scratch too small for the verdicts");
        for (v.slice(), 0..) |verdict, i| scratch[i] = @backingInt(verdict);
        return .{ .ok = .{ .free_blocks = .{ .head = .{ .n = v.n, .reserved = @splat(0) }, .verdicts = scratch[0..v.n] } } };
    }
};

// ---------------------------------------------------------------------------
// Tests: the handlers over the service, in process.
// ---------------------------------------------------------------------------

const testing = std.testing;
const block_alloc = @import("../block_alloc.zig");
const data_bitmap = @import("data_bitmap.zig");

const test_heaps = [_]data_bitmap.HeapGeometry{.{ .vol = 0, .heap_start = 0, .heap_len = 1 << 24, .node_size = 8192 }};
fn knownBlocks(_: ?*anyopaque, vol_tag: u64) ?u64 {
    return if (vol_tag == 0x77) 256 else null;
}

fn wireId(node: u64, slot: u32, writer: u64) verbs.WireIdentity {
    return .{ .node_token = node, .mount_slot = slot, .writer_id = writer, .reserved = 0 };
}

test "the lease verbs on the service: Acquire grants term 1 and replays `already`, a live peer is refused Busy, the screen is Rejected, Bitmap and Release ack fresh/replay" {
    var ram = alloc_lease.RamControlStore.init(testing.allocator);
    defer ram.deinit();
    var lh: LeaseHandlers = .{ .gpa = testing.allocator, .store = ram.store(), .screen = .{ .ctx = null, .dataVolumeBlocks = &knownBlocks, .set_width = 1, .heaps = &test_heaps }, .is_vol0_manager = true };
    var svc: service.Service = .init;
    lh.register(&svc);
    try testing.expect(svc.handles(.alloc_lease_acquire) and svc.handles(.alloc_lease_bitmap) and svc.handles(.alloc_lease_release));
    var scratch: [4096]u8 = undefined;
    const a = wireId(1, 0, 0xA);
    var req: service.Request = .{ .verb = .alloc_lease_acquire, .payload = .{ .alloc_lease_acquire = .{ .volume = 0, .home_vol = 0, .appender_id = 1, .identity = a, .vol_tag = 0x77, .blocks = 256 } }, .peer = a, .correlation = 1, .session_id = 1 };
    const ans = svc.handlers[Verb.alloc_lease_acquire.index()].?.serve(&lh, &req, &scratch);
    try testing.expectEqual(@as(u64, 1), ans.ok.alloc_lease_acquire.head.term);
    try testing.expectEqual(@as(u8, 0), ans.ok.alloc_lease_acquire.head.already);
    const again = svc.handlers[Verb.alloc_lease_acquire.index()].?.serve(&lh, &req, &scratch);
    try testing.expectEqual(@as(u8, 1), again.ok.alloc_lease_acquire.head.already);
    // A live peer: refused Busy (the typed error).
    req.payload.alloc_lease_acquire.identity = wireId(2, 0, 0xB);
    const busy = svc.handlers[Verb.alloc_lease_acquire.index()].?.serve(&lh, &req, &scratch);
    try testing.expectEqual(service.Status.refused, busy.status.status);
    try testing.expectEqual(@as(?@import("../../core/errors.zig").Error, error.Busy), busy.status.err);
    // The screen: a wire integer is never an allocation authority.
    req.payload.alloc_lease_acquire.blocks = 1 << 40;
    try testing.expectEqual(service.Status.rejected, svc.handlers[Verb.alloc_lease_acquire.index()].?.serve(&lh, &req, &scratch).status.status);
    // Bitmap: published, then a replay.
    const refs = [_]verbs.BitmapRefWire{.{ .start = 0, .len = 8192, .vol = 0, .reserved = @splat(0) }};
    var breq: service.Request = .{ .verb = .alloc_lease_bitmap, .payload = .{ .alloc_lease_bitmap = .{ .head = .{ .volume = 0, .n = 1, .reserved = 0, .identity = a, .vol_tag = 0x77, .term = 1 }, .items = &refs } }, .peer = a, .correlation = 2, .session_id = 1 };
    try testing.expectEqual(@as(u8, 0), svc.handlers[Verb.alloc_lease_bitmap.index()].?.serve(&lh, &breq, &scratch).ok.alloc_lease_bitmap.already);
    try testing.expectEqual(@as(u8, 1), svc.handlers[Verb.alloc_lease_bitmap.index()].?.serve(&lh, &breq, &scratch).ok.alloc_lease_bitmap.already);
    // The successor learns the pages through the reply tail.
    req.payload.alloc_lease_acquire.identity = a;
    req.payload.alloc_lease_acquire.blocks = 256;
    const replay = svc.handlers[Verb.alloc_lease_acquire.index()].?.serve(&lh, &req, &scratch);
    try testing.expectEqual(@as(u16, 1), replay.ok.alloc_lease_acquire.head.n_bitmap);
    try testing.expectEqual(@as(u64, 8192), replay.ok.alloc_lease_acquire.items[0].len);
    // Release: fresh, then a replay.
    var rreq: service.Request = .{ .verb = .alloc_lease_release, .payload = .{ .alloc_lease_release = .{ .volume = 0, .reserved = 0, .reserved2 = 0, .identity = a, .vol_tag = 0x77, .term = 1 } }, .peer = a, .correlation = 3, .session_id = 1 };
    try testing.expectEqual(@as(u8, 0), svc.handlers[Verb.alloc_lease_release.index()].?.serve(&lh, &rreq, &scratch).ok.alloc_lease_release.already);
    try testing.expectEqual(@as(u8, 1), svc.handlers[Verb.alloc_lease_release.index()].?.serve(&lh, &rreq, &scratch).ok.alloc_lease_release.already);
    // Not the manager: the block's word.
    lh.is_vol0_manager = false;
    try testing.expectEqual(service.Status.not_manager, svc.handlers[Verb.alloc_lease_acquire.index()].?.serve(&lh, &req, &scratch).status.status);
}

const WriterMap = struct {
    fn id(_: *anyopaque, w: records.WriterIdentity) ?u32 {
        return if (w.node_token == 9) null else @intCast(w.node_token);
    }
};

test "the holder verbs on the service: BlockGrant carves and carries the runs, Full when nothing is left, an unknown writer is Rejected; ReturnBlocks answers present/absent; FreeBlocks serves the verdicts and the correlation is the dedup key" {
    const a = try block_alloc.BlockAllocator.init(testing.allocator, .{ .vol_tag = 0x77, .vol = 0, .block_size = 4096, .capacity_bytes = 128 * 4096 });
    defer a.deinit();
    const bm = try data_bitmap.DataBitmap.init(testing.allocator, a, .{ .vol_tag = 0x77, .term = 1, .blocks = 128, .ring_capacity = 256 });
    defer bm.deinit();
    var hg: block_grant.HolderGrants = .{ .ledger = try block_grant.Ledger.init(testing.allocator, 8), .bitmap = bm };
    defer hg.ledger.deinit();
    const Ledger = struct {
        fn refcount(_: *anyopaque, _: u64, _: u64) u32 {
            return 0;
        }
    };
    const Ladder = struct {
        fn free(ctx: *anyopaque, idx: u64) @import("../reclaim.zig").FreeOutcome {
            const al: *block_alloc.BlockAllocator = @ptrCast(@alignCast(ctx));
            return switch (al.freeBlock(idx) catch return .refused) {
                .terminal => .terminal,
                .nonterminal => .nonterminal,
                .refused => .refused,
            };
        }
    };
    var ex: free_ship.FreeExecutor = .{ .alloc = a, .durable = .{ .ctx = a, .refcount = &Ledger.refcount }, .ladder = .{ .ctx = a, .free = &Ladder.free } };
    var map: u8 = 0;
    var hh: HolderHandlers = .{ .vol_tag = 0x77, .grants = &hg, .frees = &ex, .writer_ctx = &map, .writerId = &WriterMap.id };
    var svc: service.Service = .init;
    hh.register(&svc);
    var scratch: [4096]u8 = undefined;
    const w2 = wireId(2, 0, 0xB);
    var greq: service.Request = .{ .verb = .block_grant, .payload = .{ .block_grant = .{ .volume = 0, .reserved = 0, .want = 0, .writer = w2, .vol_tag = 0x77, .held_unconsumed = 0 } }, .peer = w2, .correlation = 1, .session_id = 2 };
    const g = svc.handlers[Verb.block_grant.index()].?.serve(&hh, &greq, &scratch).ok.block_grant;
    try testing.expectEqual(@as(u8, 0), g.head.kind);
    try testing.expectEqual(@as(u16, 1), g.head.n);
    try testing.expectEqual(@as(u64, 0), g.items[0].start);
    try testing.expectEqual(@as(u32, 64), g.items[0].len);
    const g2 = svc.handlers[Verb.block_grant.index()].?.serve(&hh, &greq, &scratch).ok.block_grant; // 64..128
    try testing.expectEqual(@as(u64, 64), g2.items[0].start);
    const full = svc.handlers[Verb.block_grant.index()].?.serve(&hh, &greq, &scratch).ok.block_grant;
    try testing.expectEqual(@as(u8, 1), full.head.kind);
    try testing.expectEqual(@as(u16, 0), full.head.n);
    greq.payload.block_grant.writer = wireId(9, 0, 0);
    try testing.expectEqual(service.Status.rejected, svc.handlers[Verb.block_grant.index()].?.serve(&hh, &greq, &scratch).status.status);
    greq.payload.block_grant.writer = w2;
    greq.payload.block_grant.vol_tag = 0x78;
    try testing.expectEqual(service.Status.refused, svc.handlers[Verb.block_grant.index()].?.serve(&hh, &greq, &scratch).status.status);
    // ReturnBlocks.
    var rreq: service.Request = .{ .verb = .return_blocks, .payload = .{ .return_blocks = .{ .volume = 0, .reserved = 0, .len = 8, .writer = w2, .vol_tag = 0x77, .start = 8 } }, .peer = w2, .correlation = 2, .session_id = 2 };
    const ret = svc.handlers[Verb.return_blocks.index()].?.serve(&hh, &rreq, &scratch).ok.return_blocks;
    try testing.expectEqual(@as(u8, 1), ret.present);
    try testing.expectEqual(@as(u64, 8), ret.cleared);
    rreq.payload.return_blocks.start = 200;
    try testing.expectEqual(@as(u8, 0), svc.handlers[Verb.return_blocks.index()].?.serve(&hh, &rreq, &scratch).ok.return_blocks.present);
    // FreeBlocks: writer 2 minted block 20 (granted, published); its free ships here.
    const blocks = [_]u64{ 20, 8 }; // 20: a former lessee's granted block ⇒ seeded + Freed; 8: returned (clear) ⇒ Refused
    var freq: service.Request = .{ .verb = .free_blocks, .payload = .{ .free_blocks = .{ .head = .{ .volume = 0, .n = 2, .reserved = 0, .vol_tag = 0x77, .lease_epoch = 1 }, .items = &blocks } }, .peer = w2, .correlation = 3, .session_id = 2 };
    block_alloc.stats.block_untracked_free_refusals.resetLogBudgetForTest();
    @import("../../core/log.zig").testing.expectErrorLogs(1);
    const fv = svc.handlers[Verb.free_blocks.index()].?.serve(&hh, &freq, &scratch).ok.free_blocks;
    try @import("../../core/log.zig").testing.expectAllFired();
    try testing.expectEqual(@as(u16, 2), fv.head.n);
    try testing.expectEqual(@backingInt(free_ship.FreeVerdict.freed), fv.verdicts[0]);
    try testing.expectEqual(@backingInt(free_ship.FreeVerdict.refused), fv.verdicts[1]);
    try testing.expect(!bm.isSet(20));
    // The same correlation replays the verdicts without re-executing.
    const replays0 = free_ship.stats.free_dedup_replays.load();
    _ = svc.handlers[Verb.free_blocks.index()].?.serve(&hh, &freq, &scratch).ok.free_blocks;
    try testing.expectEqual(replays0 + 1, free_ship.stats.free_dedup_replays.load());
}
