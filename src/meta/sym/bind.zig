//! The symmetric plane bound to a writable backend (Linux; charter §5.5,
//! §5.7 "the D0 winner IS the manager and a later RW mount JOINS it";
//! `docs/porting/sym_appenders_manager.md` §2.5, §3, §4.1, §4.4): the
//! `AppenderSet` — this mount's regions (region 0 = the fixed ring with
//! `is_manager` on its page; the in-process regions a second writer in
//! this process joins through the manager's door — the N-daemon harness
//! shape), the directory census at open (a Live page of ANOTHER appender
//! is unrecovered: its slots are refused until the recovery wave recovers
//! it, loud), the `Manager` over the volume seam (tree 0 through the
//! backend's trees, control entries as USER-class ring-0 entries through
//! the structural writer and BARRIERED, pages and zeroing through the
//! device, the bitmap through the allocator), the checkpoint's page writes
//! under the directory-first law, and the in-process append into a
//! region's own ring.
//!
//! The REGION TAIL LAW in force (region.zig `tailAfterCycle`): a region's
//! record floors the shared cache's node at RING 0's HEAD (an entry
//! boundary in ring 0's position domain), so ring 0's tail rule never
//! reads a region position as its tail; the region's own ring is
//! reclaimed at a cycle whose flush left the cache clean. ONE lane per
//! writable region: region 0's is the backend's; a joined region spawns
//! its own over its segment table.

const std = @import("std");
const builtin = @import("builtin");
const Allocator = std.mem.Allocator;

const counters = @import("../../core/counters.zig");
const log = @import("../../core/log.zig");
const knobs = @import("../../core/knobs.zig");
const derive = @import("../../core/derive.zig");
const backend = @import("../backend.zig");
const journal = @import("../journal.zig");
const journal_lane = @import("../journal_lane.zig");
const jc = @import("../journal_core.zig");
const smo = @import("../smo.zig");
const tree = @import("../tree.zig");
const key = @import("../key.zig");
const node = @import("../node.zig");
const node_seq = @import("../node_seq.zig");
const alloc_ext = @import("../alloc_ext.zig");
const ledger = @import("../ledger.zig");
const geometry = @import("../geometry.zig");
const checkpoint_cycle = @import("../checkpoint_cycle.zig");
const record = @import("../record.zig");
const sym = @import("../sym.zig");
const page = @import("appender_page.zig");
const dir = @import("appender_dir.zig");
const region = @import("region.zig");
const manager = @import("manager.zig");
const grant = @import("extent_grant.zig");
const join = @import("join.zig");
const cr = @import("control_records.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("sym/bind is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

const KvBackend = backend.KvBackend;

pub const Error = error{ Io, Corrupt, OutOfMemory, Capacity, Rejected, Deferred, GrantExhausted, RingFull, LaneSpawnFailed };

/// One region this mount holds.
pub const Region = struct {
    core: region.RegionCore,
    grant: grant.RegionGrant,
    /// Region 0 mints in the volume's shared space; a joined region in its
    /// incarnation's.
    seq: node_seq.NodeSeqHandle,
    identity: page.Identity,
    /// Region 0 writes through the backend's lane; a joined region owns one.
    lane: ?*journal_lane.JournalLane = null,
    ring_stats: journal.RingStats = .{},
    /// The structural writer over the region's ring (region 0's is the
    /// backend's `smo_journal`).
    smo_journal: smo.SmoJournal,
    /// The tail the last page write named: durable at the next cycle's
    /// barrier, when the ring reclaims up to it.
    tail_written: u64,
    /// Records this region appended since its join.
    entries: u64 = 0,
    /// The region's measured SMO rate (milli-SMO/s), folded per cycle off
    /// its own images (0 until its first SMO — the derivation's floor).
    smo_ewma_milli_per_s: u64 = 0,

    pub fn appenderId(self: *const Region) u32 {
        return self.core.appender_id;
    }
};

pub const AppenderSet = struct {
    gpa: Allocator,
    be: *KvBackend,
    /// Region 0 first; the in-process regions after. Mutated under the
    /// backend's SMO lock (the checkpoint cycle's hook walks it under the
    /// same lock); read by the manager's verbs on the joining thread.
    regions: std.ArrayList(*Region) = .empty,
    manager: manager.Manager,
    identity: page.Identity,
    /// Live pages of OTHER appenders found at open: unrecovered (their
    /// rings are the recovery wave's; their slots are refused).
    unrecovered: std.ArrayList(u32) = .empty,
    /// The directory's Live count at open.
    live_at_open: u64 = 0,
    /// The ring-budget remainder as of the last census.
    budget_remaining: u64 = 0,
    /// A page-aligned zero buffer for the carve's zeroing (one per set).
    zeros: []align(4096) u8,

    const ZERO_BUF: usize = 1 << 20;

    /// The open-time binding: the census, region 0 Live with `is_manager`
    /// and `term + 1` under this identity (a page 0 Live under a FOREIGN
    /// node means the D0 ladder was won over its holder — the role passes
    /// with the page), the lease held.
    pub fn open(be: *KvBackend) backend.OpenError!*AppenderSet {
        const gpa = be.gpa;
        const self = try gpa.create(AppenderSet);
        errdefer gpa.destroy(self);
        const g = be.guard orelse return error.Corrupt;
        const identity = join.identityFor(g.identity, be.index);
        self.* = .{ .gpa = gpa, .be = be, .manager = undefined, .identity = identity, .zeros = undefined };
        self.zeros = gpa.alignedAlloc(u8, .fromByteUnits(4096), ZERO_BUF) catch return error.OutOfMemory;
        errdefer gpa.free(self.zeros);
        @memset(self.zeros, 0);
        self.manager = manager.Manager.init(gpa, be.io, self.volume());
        errdefer self.manager.deinit();
        try self.census();
        try self.joinRegion0();
        sym.stats.symmetric_meta.add(1);
        sym.stats.appenders_live.add(1);
        return self;
    }

    pub fn deinit(self: *AppenderSet) void {
        for (self.regions.items) |r| self.destroyRegion(r);
        self.regions.deinit(self.gpa);
        self.unrecovered.deinit(self.gpa);
        self.manager.deinit();
        self.gpa.free(self.zeros);
        sym.stats.symmetric_meta.sub(1);
        self.gpa.destroy(self);
    }

    fn destroyRegion(self: *AppenderSet, r: *Region) void {
        if (r.lane) |l| {
            l.shutdown();
            l.deinit();
        }
        r.grant.deinit();
        r.core.deinit();
        self.gpa.destroy(r);
    }

    /// The directory census: every pair's newest image; a Live page under
    /// another identity is unrecovered and loud.
    fn census(self: *AppenderSet) backend.OpenError!void {
        const be = self.be;
        var links: std.ArrayList(dir.Link) = .empty;
        defer links.deinit(self.gpa);
        const vol = self.volume();
        var reader_ctx = vol;
        const reader: dir.PageReader = .{ .ctx = &reader_ctx, .read = readPageSeam };
        dir.walkChain(self.gpa, reader, .{ .start = be.sb.appender_dir.start, .len = be.sb.appender_dir.len }, be.sb.node_size, .{ .start = be.sb.heap.start, .len = be.sb.heap.len }, be.sb.totalExtents(), &links) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
            error.Corrupt => blk: {
                log.err("meta volume {s}: the appender directory chain does not verify — refusing the mount", .{be.path});
                break :blk error.Corrupt;
            },
        };
        var rings_in_use: u64 = 0;
        for (links.items) |l| {
            var pair: u32 = 0;
            while (pair < l.header.pairs) : (pair += 1) {
                const imgs = dir.readPair(reader, l, pair) catch return error.Io;
                if (imgs.foreign) {
                    log.err("meta volume {s}: appender {d}'s directory page carries a layout this binary does not read — refusing the mount", .{ be.path, imgs.appender_id });
                    return error.UnsupportedFormat;
                }
                const p = imgs.newest() orelse continue;
                if (p.state != .live) continue;
                self.live_at_open += 1;
                rings_in_use += p.ringBytes();
                sym.stats.appender_live_pages_at_mount.inc();
                try self.unrecovered.append(self.gpa, p.appender_id);
                log.warn("meta volume {s}: appender {d} (node 0x{x}, mount slot {d}, term {d}) is LIVE on the directory with its ring unrecovered — its slots are refused until the recovery ladder recovers it", .{ be.path, p.appender_id, p.identity.node_token, p.identity.mount_slot, p.term });
            }
        }
        self.budget_remaining = region.ringBudgetBytes(be.sb.heap.len) -| (rings_in_use + be.sb.journal.len);
    }

    /// Region 0 = the fixed ring: the page under THIS identity, Live,
    /// `is_manager`, `term + 1`, `recovered_by_term 0`, into the rotation
    /// (both directory slots first) + barrier.
    fn joinRegion0(self: *AppenderSet) backend.OpenError!void {
        const be = self.be;
        const offs = page.appender0PageOffsets(be.sb.journal.start);
        var bufs: [4][page.PAGE_LEN]u8 = undefined;
        var imgs: [4]?[]const u8 = .{ null, null, null, null };
        for (offs, 0..) |off, i| {
            const img = try be.dev.readRange(self.gpa, off, page.PAGE_LEN);
            defer self.gpa.free(img);
            @memcpy(&bufs[i], img[0..page.PAGE_LEN]);
            imgs[i] = &bufs[i];
        }
        const prior = page.newestValid(imgs) catch {
            log.err("meta volume {s}: appender 0's page carries a layout this binary does not read — refusing the mount", .{be.path});
            return error.UnsupportedFormat;
        };
        var words: page.Page = prior orelse page.Page.free(0, 0, page.SegmentTable.one(.{ .start = be.sb.journal.start, .len = be.sb.journal.len }));
        if (prior) |p| if (p.state == .live and !p.identity.sameMount(self.identity)) {
            log.info("meta volume {s}: appender 0's page is Live under node 0x{x} mount slot {d} (term {d}) — the D0 ladder was won over its holder; the manager role passes with the page", .{ be.path, p.identity.node_token, p.identity.mount_slot, p.term });
        };
        words.appender_id = journal.FIXED_RING_APPENDER;
        words.term += 1;
        words.state = .live;
        words.recovered_by_term = 0;
        words.identity = self.identity;
        words.is_manager = true;
        words.home_volume = be.stamp.member_position;
        words.segments = page.SegmentTable.one(.{ .start = be.sb.journal.start, .len = be.sb.journal.len });
        words.head_hint = be.core.headPos();
        words.ledger_tail_seq = be.mounted.journal_tail_seq;
        words.ckpt_seq = be.mounted.seq;
        words.seq_offset = be.reg.seqOffset();
        words.grants = .{};
        words.n_slots = 0;
        const r = try self.gpa.create(Region);
        errdefer self.gpa.destroy(r);
        r.* = .{
            .core = region.RegionCore.init(self.gpa, .{
                .appender_id = journal.FIXED_RING_APPENDER,
                .table = words.segments,
                .dir_slots = .{ offs[0], offs[1] },
                .grain = be.dev.mode.grain(),
                .head_pos = be.core.headPos(),
                .tail_pos = be.mounted.journal_tail_seq,
                .page_words = words,
            }) catch return error.Corrupt,
            .grant = grant.RegionGrant.init(self.gpa),
            .seq = node_seq.NodeSeqHandle.sharedBounded(be.seq.load(), node_seq.nodeSeqBase(be.sb.uuid)),
            .identity = self.identity,
            .smo_journal = undefined,
            .tail_written = be.mounted.journal_tail_seq,
        };
        // Region 0's ring IS the backend's: its words are read from the
        // backend's core at every page write (`onCheckpoint`).
        r.core.page.seq_offset = be.reg.seqOffset();
        try self.regions.append(self.gpa, r);
        try self.writePages(r, false);
        self.manager.holdLease(words.term);
        self.manager.appenders_known = @max(self.live_at_open + 1, 1);
        log.info("meta volume {s}: appender 0 Live as the manager (term {d}, node 0x{x}, mount slot {d}); {d} other appender(s) live on the directory", .{ be.path, words.term, self.identity.node_token, self.identity.mount_slot, self.live_at_open });
    }

    /// The page plan executed: every slot the law names, then one barrier.
    fn writePages(self: *AppenderSet, r: *Region, from_cycle: bool) error{ Io, Corrupt, OutOfMemory }!void {
        var plan = r.core.planPageWrite() catch return error.Corrupt;
        for (plan.slice()) |w| {
            self.be.pool.writeAt(self.be.path, w.offset, &w.image) catch return error.Io;
            sym.stats.appender_page_writes.inc();
        }
        if (!from_cycle) self.be.dev.barrier() catch return error.Io;
        r.core.pageWritten(&plan);
    }

    /// `RecordSource.appenderPages` (the cycle's C11): every region's page
    /// with the cycle's words; a region ≥ 1's ring reclaims up to the tail
    /// its PREVIOUS page named (durable at this cycle's barrier #1) and
    /// advances its tail at the fixpoint. The cycle's own barriers cover
    /// these writes (barrier #1 of the next cycle at the latest).
    pub fn onCheckpoint(self: *AppenderSet, rec: *const ledger.LedgerRecord, cache_clean: bool) checkpoint_cycle.IoError!void {
        for (self.regions.items, 0..) |r, i| {
            if (i == 0) {
                // Region 0's ring IS the backend's: its words are the core's.
                r.core.page.head_hint = self.be.core.headPos();
                r.core.page.ledger_tail_seq = rec.journal_tail_seq;
                r.core.page.ckpt_seq = rec.seq;
                r.core.reg.recoverSeqOffset(self.be.reg.seqOffset());
            } else {
                r.core.core.advanceReusableUpto(@min(r.tail_written, r.core.core.headPos()));
                const tail = r.core.tailAfterCycle(cache_clean);
                if (tail > r.tail_written) sym.stats.appender_ring_tail_advances.inc();
                r.tail_written = tail;
                r.core.captureHead();
                r.core.stampCycle(rec.seq);
                // The pool at the cadence: the covered frees become
                // returnable, the pool refills itself from them up to its
                // target (a compaction's claim + retire is net-zero inside
                // the grant), the surplus waits for the quiet return.
                _ = r.grant.advanceDurable(tail) catch return error.Io;
                const recycled = r.grant.recycle(self.poolTarget(r)) catch return error.Io;
                sym.stats.extent_grant_recycled.add(recycled);
                // The page names the pool's largest runs.
                var runs: std.ArrayList(grant.GrantRun) = .empty;
                defer runs.deinit(self.gpa);
                r.grant.unclaimedRuns(self.gpa, &runs) catch return error.Io;
                r.core.page.grants = grant.pageRunsOf(runs.items);
            }
            self.writePages(r, true) catch return error.Io;
        }
    }

    // ---- the in-process join door (the N-daemon harness shape) ----

    /// A second writer in THIS process joins at `mount_slot`: the manager's
    /// `joinAppender` in-process → the ladder → a region with its own lane
    /// over its ring segments. Capacity refuses naming the volume count.
    pub fn joinInProcess(self: *AppenderSet, mount_slot: u32) Error!*Region {
        const be = self.be;
        const g = be.guard orelse return error.Corrupt;
        const identity = join.identityFor(g.identity, mount_slot);
        var d: Door = .{ .set = self };
        var jr = join.join(self.gpa, d.door(), .{
            .identity = identity,
            .ring_want_bytes = try self.ringAsk(),
            .home_volume = be.stamp.member_position,
            .volume_base = node_seq.nodeSeqBase(be.sb.uuid),
            .grain = be.dev.mode.grain(),
            .ring0_head = be.core.headPos(),
        }) catch |e| return switch (e) {
            error.Capacity => error.Capacity,
            error.Rejected => error.Rejected,
            error.Deferred => error.Deferred,
            error.Io => error.Io,
            error.Corrupt => error.Corrupt,
            error.OutOfMemory => error.OutOfMemory,
        };
        errdefer jr.deinit();
        const r = try self.gpa.create(Region);
        errdefer self.gpa.destroy(r);
        r.* = .{ .core = jr.core, .grant = jr.grant, .seq = jr.seq, .identity = identity, .smo_journal = undefined, .tail_written = jr.core.page.ledger_tail_seq };
        // The region's lane over its segment table.
        r.lane = journal_lane.JournalLane.spawn(self.gpa, be.env, .{
            .path = be.path,
            .index = be.index,
            .core = &r.core.core,
            .registry = &r.core.reg,
            .ring_stats = &r.ring_stats,
        }) catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            else => return error.LaneSpawnFailed,
        };
        r.smo_journal = .{ .gpa = self.gpa, .layout = r.core.ring_layout, .geo = r.core.geo, .core = &r.core.core, .reg = &r.core.reg, .image = &.{}, .barrier_hook = be.dev.barrierSink(), .latch = &r.lane.?.latch, .device = .{ .ctx = r, .write = regionWriteHook, .completed = regionCompletedHook }, .appender_id = r.core.appender_id };
        be.smo_lock.lockUncancelable(be.io);
        defer be.smo_lock.unlock(be.io);
        try self.regions.append(self.gpa, r);
        sym.stats.appenders_live.add(1);
        // The joined page is the manager's write; the region's own first
        // page write lands at the next cycle under the directory-first law
        // (its `dir_named` starts at 0: both slots again).
        return r;
    }

    /// `SQUEEZEFS_SYM_RING_KB`: explicit wins verbatim inside the volume's
    /// ceiling and REFUSES above it (never a clamp); absent = 0 = derive
    /// (the identity's hint, else the floor — the manager's law).
    fn ringAsk(self: *AppenderSet) Error!u64 {
        const kib = knobs.optIntKnob("SQUEEZEFS_SYM_RING_KB", u64, self.be.env) orelse return 0;
        return region.ringBytesExplicit(kib, self.be.sb.volume_len) catch {
            log.err("meta volume {s}: SQUEEZEFS_SYM_RING_KB={d} is above this volume's ring ceiling of {d} bytes (clamp(volume/64, 8 MiB, 32 MiB)) — refusing the join", .{ self.be.path, kib, region.ringCeiling(self.be.sb.volume_len) });
            return error.Rejected;
        };
    }

    /// The pool target of a joined region at this cadence: the explicit
    /// `SQUEEZEFS_SYM_GRANT_EXTENTS`, else the derivation over the region's
    /// measured SMO rate (0 until its first SMO: the floor) and the volume's
    /// free heap over the appenders known — the recycle's keep.
    fn poolTarget(self: *AppenderSet, r: *const Region) u64 {
        const bound = grant.failoverBoundMs(self.be.gate_ns / std.time.ns_per_ms, self.be.replayStats().replay_ns / std.time.ns_per_ms);
        const derived = grant.grantDerived(r.smo_ewma_milli_per_s, bound, self.be.alloc.freeExtents(), self.manager.appenders_known);
        return grant.resolveGrantExtents(knobs.optIntKnob("SQUEEZEFS_SYM_GRANT_EXTENTS", u64, self.be.env), @max(derived, grant.joinedPoolFloor(self.mintSlots())));
    }

    fn regionWriteHook(ctx: *anyopaque, ops: []const journal.WriteOp) error{Io}!void {
        const r: *Region = @ptrCast(@alignCast(ctx));
        const lane = r.lane.?;
        const t = while (true) {
            if (lane.submitStructural(ops)) |t| break t else |e| {
                if (e != error.LaneQueueFull) return error.Io;
                std.Thread.yield() catch {};
            }
        };
        t.wait().result catch return error.Io;
    }

    fn regionCompletedHook(ctx: *anyopaque) void {
        const r: *Region = @ptrCast(@alignCast(ctx));
        r.lane.?.kick();
    }

    /// The clean leave of an in-process region: a fixpoint cycle drains its
    /// ring (tail = head), the pool's unclaimed runs return with the page
    /// (Free, both directory slots), the ring's bits clear.
    pub fn leaveInProcess(self: *AppenderSet, r: *Region) Error!void {
        self.be.checkpointNow() catch return error.Io;
        if (r.core.page.ledger_tail_seq != r.core.core.headPos()) {
            // A second fixpoint cycle: the first wrote the page, this one
            // reclaims up to it.
            self.be.checkpointNow() catch return error.Io;
        }
        // Out of the cycle's walk BEFORE the page goes Free: a later page
        // write of this region would resurrect it Live.
        self.be.smo_lock.lockUncancelable(self.be.io);
        for (self.regions.items, 0..) |x, i| if (x == r) {
            _ = self.regions.orderedRemove(i);
            break;
        };
        self.be.smo_lock.unlock(self.be.io);
        var d: Door = .{ .set = self };
        var jr: join.JoinedRegion = .{ .core = r.core, .grant = r.grant, .seq = r.seq, .identity = r.identity, .already = false };
        _ = join.leave(self.gpa, d.door(), &jr) catch |e| {
            // The page stays Live for the next open's recovery; the region
            // is gone from this process either way.
            self.destroyRegion(r);
            sym.stats.appenders_live.sub(1);
            return switch (e) {
                error.Deferred => error.Deferred,
                error.Rejected => error.Rejected,
                error.OutOfMemory => error.OutOfMemory,
                error.Io => error.Io,
                else => error.Corrupt,
            };
        };
        self.manager.forgetEndpoint(r.appenderId());
        self.destroyRegion(r);
        sym.stats.appenders_live.sub(1);
    }

    /// Every in-process region leaves (the backend's close).
    pub fn leaveAll(self: *AppenderSet) void {
        while (self.regions.items.len > 1) {
            const r = self.regions.items[self.regions.items.len - 1];
            const id = r.appenderId();
            self.leaveInProcess(r) catch |e| log.err("meta volume {s}: appender {d} could not leave cleanly ({s}) — its page stays Live for the next open's recovery", .{ self.be.path, id, @errorName(e) });
        }
    }

    pub fn region0(self: *AppenderSet) *Region {
        return self.regions.items[0];
    }

    /// Is `appender_id`'s ring unrecovered (a Live page of another writer
    /// found at open)? Its slots are refused until recovered.
    pub fn isUnrecovered(self: *const AppenderSet, appender_id: u32) bool {
        for (self.unrecovered.items) |id| if (id == appender_id) return true;
        return false;
    }

    // ---- the in-process append into a region's own ring ----

    const StampCtx = struct {
        j: *smo.SmoJournal,
        adm: jc.Admission,
        /// The node floor: null = the reservation's own entry start (a ring-0
        /// writer); a region's append floors at ring 0's head (an entry
        /// boundary in ring 0's position domain).
        floor: ?u64 = null,
        registered: ?jc.InflightRegistry.Registered = null,

        fn stamp(ctx: *anyopaque) Allocator.Error!tree.Stamp {
            const self: *StampCtx = @ptrCast(@alignCast(ctx));
            const r = try self.j.reserve(self.adm);
            self.registered = r;
            return .{ .seq = r.seq_base, .entry_start = self.floor orelse r.res.start };
        }
    };

    /// ONE record put into `tree_id`'s tree through region `r`'s ring:
    /// admitted on the region's core (USER class), reserved INSIDE the
    /// leaf's lock window on the region's registry (its seq = position +
    /// the region's offset), the node floored at ring 0's head, the entry
    /// written through the region's lane, answered when it landed.
    pub fn appendPut(self: *AppenderSet, r: *Region, tree_id: u8, k: []const u8, value: []const u8) Error!void {
        const be = self.be;
        std.debug.assert(r.appenderId() != journal.FIXED_RING_APPENDER);
        const t = tree.routeContent(be.trees.load(), tree_id, k) orelse return error.Corrupt;
        var frames = [_]journal.RecordFrame{.{ .tag = key.tagFor(tree_id, 0), .key = k, .kind = .put, .seq = 0, .value = value }};
        const len = journal.entryLenFor(&frames) catch return error.Corrupt;
        const adm = r.core.core.tryAdmit(len, .user) orelse return error.RingFull;
        var st: StampCtx = .{ .j = &r.smo_journal, .adm = adm, .floor = be.core.headPos() };
        var attempt: u32 = 0;
        while (attempt < node.RETRY_BUDGET) : (attempt += 1) {
            const leaf = t.resolveLeaf(k) catch |e| {
                r.core.core.release(adm);
                return switch (e) {
                    error.OutOfMemory => error.OutOfMemory,
                    error.Io => error.Io,
                    else => error.Corrupt,
                };
            };
            defer leaf.release();
            const outcome = t.applyAtSeqClass(leaf, k, .put, value, .{ .reserve = .{ .ctx = &st, .stamp = StampCtx.stamp } }, false) catch |e| {
                r.core.core.release(adm);
                return switch (e) {
                    error.OutOfMemory => error.OutOfMemory,
                    error.Io => error.Io,
                    else => error.Corrupt,
                };
            };
            switch (outcome) {
                .applied => {
                    const reg = st.registered.?;
                    frames[0].seq = reg.seq_base;
                    r.smo_journal.commitEntry(reg.res, &frames) catch return error.Io;
                    r.entries += 1;
                    sym.stats.appender_region_entries.inc();
                    sym.stats.appender_region_floors.inc();
                    return;
                },
                .stale => continue,
            }
        }
        r.core.core.release(adm);
        return error.Corrupt;
    }

    // ---- the manager's door, in-process ----

    const Door = struct {
        set: *AppenderSet,

        fn door(self: *Door) join.Door {
            return .{ .ctx = self, .join = joinFn, .leave = leaveFn, .in_process = true };
        }

        fn joinFn(ctx: *anyopaque, req: manager.JoinAppender) manager.VerbError!manager.Joined {
            const self: *Door = @ptrCast(@alignCast(ctx));
            return self.set.manager.joinAppender(req);
        }

        fn leaveFn(ctx: *anyopaque, req: manager.LeaveAppender) manager.VerbError!manager.Left {
            const self: *Door = @ptrCast(@alignCast(ctx));
            return self.set.manager.leaveAppender(req);
        }
    };

    // ---- the volume seam ----

    pub fn volume(self: *AppenderSet) manager.Volume {
        return .{ .ctx = self, .vt = &vtable };
    }

    const vtable: manager.Volume.VTable = .{
        .geometry = geometryFn,
        .lookup = lookupFn,
        .range = rangeFn,
        .controlEntry = controlEntryFn,
        .readPage = readPageFn,
        .writePages = writePagesFn,
        .zero = zeroFn,
        .claimInternal = claimInternalFn,
        .releaseUnpublished = releaseUnpublishedFn,
        .freeExtents = freeExtentsFn,
        .ring0Head = ring0HeadFn,
        .ownsRegion = ownsRegionFn,
        .regionGrantHeld = regionGrantHeldFn,
        .ring0WindowClearOf = windowClearFn,
        .cycleCheckpoint = cycleFn,
        .kickCheckpoint = kickFn,
    };

    fn self_(ctx: *anyopaque) *AppenderSet {
        return @ptrCast(@alignCast(ctx));
    }

    fn readPageSeam(ctx: *anyopaque, offset: u64, out: *[page.PAGE_LEN]u8) error{Io}!void {
        const vol: *manager.Volume = @ptrCast(@alignCast(ctx));
        return vol.vt.readPage(vol.ctx, offset, out);
    }

    /// `SQUEEZEFS_SYM_MINT_SLOTS`: explicit verbatim, else `clamp(W / (2 ×
    /// writers_known), 1, 64)`.
    fn mintSlots(self: *AppenderSet) u64 {
        if (knobs.optIntKnob("SQUEEZEFS_SYM_MINT_SLOTS", u64, self.be.env)) |m| return m;
        return derive.clamp(geometry.routing_width / (2 * @max(self.manager.appenders_known, 1)), 1, 64);
    }

    fn geometryFn(ctx: *anyopaque) manager.Geometry {
        const self = self_(ctx);
        const sb = self.be.sb;
        return .{
            .node_size = sb.node_size,
            .volume_len = sb.volume_len,
            .heap = .{ .start = sb.heap.start, .len = sb.heap.len },
            .total_extents = sb.totalExtents(),
            .journal = .{ .start = sb.journal.start, .len = sb.journal.len },
            .appender_dir = .{ .start = sb.appender_dir.start, .len = sb.appender_dir.len },
            .volume_base = node_seq.nodeSeqBase(sb.uuid),
            .grain = self.be.dev.mode.grain(),
            .mint_slots = self.mintSlots(),
        };
    }

    fn lookupFn(ctx: *anyopaque, gpa: Allocator, k: []const u8) error{ Io, OutOfMemory }!?[]u8 {
        const self = self_(ctx);
        return self.be.lookup(gpa, key.TREE_CONTROL, k) catch |e| switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            else => error.Io,
        };
    }

    fn rangeFn(ctx: *anyopaque, gpa: Allocator, start: []const u8, end: []const u8, out: *std.ArrayList(manager.KeyValue)) error{ Io, OutOfMemory }!void {
        const self = self_(ctx);
        var kvs: std.ArrayList(tree.KeyValue) = .empty;
        defer kvs.deinit(gpa);
        self.be.range(gpa, key.TREE_CONTROL, start, end, std.math.maxInt(usize), &kvs) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            else => error.Io,
        };
        for (kvs.items) |kv| try out.append(gpa, .{ .key = kv.key, .value = kv.value });
    }

    /// ONE ring-0 control entry in the USER class (`tryAdmit(len, .user)`:
    /// a verb storm competes with user commits and never eats the
    /// checkpoint reserve; a full window is `JournalReserveExhausted` → the
    /// caller's `deferred` + kick): the tree-0 puts/deletes applied to
    /// their leaves — the first reserving INSIDE its lock window, the rest
    /// at `seq_base + i` — the allocator deltas as kind-4 frames, the entry
    /// written through the lane, then BARRIERED. The value cap is enforced
    /// here, never discovered at the flush pass.
    fn controlEntryFn(ctx: *anyopaque, recs: []const manager.ControlRecord) manager.ControlError!void {
        const self = self_(ctx);
        const be = self.be;
        const gpa = self.gpa;
        var frames: std.ArrayList(journal.RecordFrame) = .empty;
        defer frames.deinit(gpa);
        var alloc_keys: std.ArrayList([alloc_ext.extent_key_len]u8) = .empty;
        defer alloc_keys.deinit(gpa);
        try alloc_keys.ensureTotalCapacity(gpa, recs.len);
        const value_cap = geometry.recordValueCap(be.sb.node_size);
        for (recs) |r| switch (r) {
            .put => |p| {
                if (p.value.len > value_cap) return error.ValueTooLarge;
                try frames.append(gpa, .{ .tag = key.tagFor(key.TREE_CONTROL, 0), .key = p.key, .kind = .put, .seq = 0, .value = p.value });
            },
            .delete => |k| try frames.append(gpa, .{ .tag = key.tagFor(key.TREE_CONTROL, 0), .key = k, .kind = .delete, .seq = 0, .value = &.{} }),
            .alloc => |e| {
                alloc_keys.appendAssumeCapacity(alloc_ext.extentKey(e));
                try frames.append(gpa, .{ .tag = key.tagFor(alloc_ext.tree_alloc_reserved, 0), .key = &alloc_keys.items[alloc_keys.items.len - 1], .kind = .put, .seq = 0, .value = &alloc_ext.allocRecordValue() });
            },
            .free => |e| {
                alloc_keys.appendAssumeCapacity(alloc_ext.extentKey(e));
                try frames.append(gpa, .{ .tag = key.tagFor(alloc_ext.tree_alloc_reserved, 0), .key = &alloc_keys.items[alloc_keys.items.len - 1], .kind = .put, .seq = 0, .value = &alloc_ext.freeRecordValue(0) });
            },
        };
        if (frames.items.len == 0) return;
        const len = journal.entryLenFor(frames.items) catch return error.ValueTooLarge;
        const adm = be.core.tryAdmit(len, .user) orelse return error.JournalReserveExhausted;
        const control = be.trees.find(.control) orelse return error.Corrupt;
        var st: StampCtx = .{ .j = &be.smo_journal, .adm = adm };
        var reserved: ?jc.InflightRegistry.Registered = null;
        for (frames.items, 0..) |*f, i| {
            if (journal.untag(f.tag).tree_id != key.TREE_CONTROL) continue;
            const kind: record.RecordKind = if (f.kind == .put) .put else .delete;
            var attempt: u32 = 0;
            while (attempt < node.RETRY_BUDGET) : (attempt += 1) {
                const leaf = control.resolveLeaf(f.key) catch |e| {
                    if (reserved == null) be.core.release(adm);
                    return switch (e) {
                        error.OutOfMemory => error.OutOfMemory,
                        error.Io => error.Io,
                        else => error.Corrupt,
                    };
                };
                defer leaf.release();
                const plan: tree.SeqPlan = if (reserved) |r| .{ .replay = .{ .seq = r.seq_base + i, .entry_start = r.res.start } } else .{ .reserve = .{ .ctx = &st, .stamp = StampCtx.stamp } };
                const outcome = control.applyAtSeqClass(leaf, f.key, kind, f.value, plan, true) catch |e| {
                    if (reserved == null) be.core.release(adm);
                    return switch (e) {
                        error.OutOfMemory => error.OutOfMemory,
                        error.Io => error.Io,
                        else => error.Corrupt,
                    };
                };
                if (outcome == .applied) break;
            } else {
                if (reserved == null) be.core.release(adm);
                return error.Corrupt;
            }
            if (reserved == null) reserved = st.registered.?;
        }
        if (reserved == null) reserved = be.smo_journal.reserve(adm) catch return error.OutOfMemory;
        const r = reserved.?;
        for (frames.items, 0..) |*f, i| f.seq = r.seq_base + i;
        be.smo_journal.commitEntry(r.res, frames.items) catch return error.Io;
        be.dev.barrier() catch return error.Io;
    }

    fn readPageFn(ctx: *anyopaque, offset: u64, out: *[page.PAGE_LEN]u8) error{Io}!void {
        const self = self_(ctx);
        const img = self.be.dev.readRange(self.gpa, offset, page.PAGE_LEN) catch return error.Io;
        defer self.gpa.free(img);
        @memcpy(out, img[0..page.PAGE_LEN]);
    }

    fn writePagesFn(ctx: *anyopaque, writes: []const manager.PageWriteOp) error{Io}!void {
        const self = self_(ctx);
        for (writes) |w| self.be.pool.writeAt(self.be.path, w.offset, w.image) catch return error.Io;
        self.be.dev.barrier() catch return error.Io;
    }

    fn zeroFn(ctx: *anyopaque, chunks: []const region.ZeroChunk) error{Io}!void {
        const self = self_(ctx);
        for (chunks) |c| {
            var off = c.offset;
            const end = c.offset + c.len;
            while (off < end) {
                const n: usize = @intCast(@min(end - off, self.zeros.len));
                self.be.pool.writeAt(self.be.path, off, self.zeros[0..n]) catch return error.Io;
                off += n;
            }
        }
        self.be.dev.barrier() catch return error.Io;
    }

    fn claimInternalFn(ctx: *anyopaque) error{NoSpace}!u64 {
        return self_(ctx).be.alloc.claimInternal();
    }

    fn releaseUnpublishedFn(ctx: *anyopaque, extent: u64) void {
        self_(ctx).be.alloc.releaseUnpublished(extent);
    }

    fn freeExtentsFn(ctx: *anyopaque) u64 {
        return self_(ctx).be.alloc.freeExtents();
    }

    fn ring0HeadFn(ctx: *anyopaque) u64 {
        return self_(ctx).be.core.headPos();
    }

    fn ownsRegionFn(ctx: *anyopaque, appender_id: u32) bool {
        for (self_(ctx).regions.items) |r| if (r.appenderId() == appender_id) return true;
        return false;
    }

    fn regionGrantHeldFn(ctx: *anyopaque, appender_id: u32, extent: u64) bool {
        for (self_(ctx).regions.items) |r| if (r.appenderId() == appender_id) {
            r.grant.returnable_(extent) catch return true;
            return false;
        };
        return false;
    }

    /// Ring 0's window names no record of the slots iff it is EMPTY
    /// (`reusable_upto == head`): the per-slot record frontiers are the
    /// slot-lease wave's; until then the door cycles to the fixpoint.
    fn windowClearFn(ctx: *anyopaque, _: []const u16) bool {
        const be = self_(ctx).be;
        return be.core.reusableUpto() >= be.core.headPos();
    }

    fn cycleFn(ctx: *anyopaque) error{ Io, Corrupt }!u64 {
        const be = self_(ctx).be;
        be.checkpointNow() catch |e| return switch (e) {
            error.Io, error.OutOfMemory, error.VolumeFailed => error.Io,
            error.Corrupt => error.Corrupt,
        };
        return be.cp.lastLedgerTail();
    }

    fn kickFn(ctx: *anyopaque) void {
        self_(ctx).be.task.kick(.ring_park);
    }

    /// The per-volume words of the `.stats` family.
    pub fn writeStatsJson(self: *AppenderSet, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.writeAll("{\"manager_lease\":\"");
        try self.manager.leaseWord(w);
        try w.print("\",\"appenders_live\":{d},\"appenders_known\":{d},\"appenders_capacity\":{d},\"appender_ring_budget_remaining_bytes\":{d},\"appender_live_pages_at_mount\":{d},\"appender_unrecovered\":{d},\"regions\":[", .{ self.regions.items.len, self.manager.appenders_known, region.appendersCapacity(self.budget_remaining), self.budget_remaining, self.live_at_open, self.unrecovered.items.len });
        for (self.regions.items, 0..) |r, i| {
            if (i != 0) try w.writeByte(',');
            try w.print("{{\"appender_id\":{d},\"term\":{d},\"ring_bytes\":{d},\"ring_segments\":{d},\"seq_offset\":{d},\"grant_unclaimed\":{d},\"grant_held\":{d},\"grant_granted\":{d},\"grant_returned\":{d},\"entries\":{d}}}", .{ r.appenderId(), r.core.page.term, r.core.ringBytes(), r.core.table.len, r.core.reg.seqOffset(), r.grant.unclaimedCount(), r.grant.held(), r.grant.granted, r.grant.returned, r.entries });
        }
        try w.writeAll("]}");
    }
};

// ---------------------------------------------------------------------------
// Tests (Linux; regular files on the venue's ext4 under /build/tmp).
// ---------------------------------------------------------------------------

const testing = std.testing;
const linux = std.os.linux;
const exec = @import("../../exec/exec.zig");
const align_probe = @import("../../io/align_probe.zig");
const superblock = @import("../superblock.zig");

const test_dir = "/build/tmp";
const test_volume_len: u64 = 64 << 20;
const test_set: geometry.SetIdentity = .{ .set_uuid = "sym-bind-set!!!!".*, .hash_seed = 0x5EED_CAFE_F00D_D00D };

const TestVol = struct {
    gpa: Allocator,
    env: std.process.Environ.Map,
    sqz: *exec.SqzIo,
    path_buf: [128]u8 = undefined,
    path_len: usize = 0,

    fn path(self: *const TestVol) []const u8 {
        return self.path_buf[0..self.path_len];
    }

    fn create(gpa: Allocator, name: []const u8, class: geometry.Class) !*TestVol {
        const self = try gpa.create(TestVol);
        errdefer gpa.destroy(self);
        self.* = .{ .gpa = gpa, .env = std.process.Environ.Map.init(gpa), .sqz = undefined };
        errdefer self.env.deinit();
        self.sqz = try exec.SqzIo.testInstance();
        errdefer self.sqz.deinit();
        var zbuf: [linux.PATH_MAX]u8 = undefined;
        _ = linux.mkdirat(linux.AT.FDCWD, try align_probe.pathZ(&zbuf, test_dir), 0o755);
        self.path_len = (try std.fmt.bufPrint(&self.path_buf, "{s}/sqz-sym-{d}-{s}.img", .{ test_dir, linux.getpid(), name })).len;
        try truncateFile(self.path(), test_volume_len);
        try backend.formatVolume(gpa, self.path(), .{ .env = &self.env, .class = class, .node_size = 65536, .journal_len_override = 1 << 20, .uuid = "sym-bind-uuid!!!".*, .set = test_set });
        return self;
    }

    fn destroy(self: *TestVol) void {
        unlinkPath(self.path());
        self.sqz.deinit();
        self.env.deinit();
        self.gpa.destroy(self);
    }

    fn open(self: *TestVol) !*KvBackend {
        return self.openAt(self.path(), 0);
    }

    fn openAt(self: *TestVol, p: []const u8, index: u16) !*KvBackend {
        return KvBackend.open(self.gpa, self.sqz.io(), p, .{ .env = &self.env, .cache_budget_bytes = 64 << 20, .cpus = 4, .index = index });
    }
};

fn truncateFile(path: []const u8, len: u64) !void {
    var zbuf: [linux.PATH_MAX]u8 = undefined;
    const z = try align_probe.pathZ(&zbuf, path);
    const rc = linux.open(z, .{ .ACCMODE = .RDWR, .CLOEXEC = true, .CREAT = true, .TRUNC = true }, 0o644);
    if (linux.errno(rc) != .SUCCESS) return error.CannotCreateDevice;
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    if (linux.errno(linux.ftruncate(fd, @intCast(len))) != .SUCCESS) return error.CannotSizeDevice;
}

fn unlinkPath(path: []const u8) void {
    var zbuf: [linux.PATH_MAX]u8 = undefined;
    if (align_probe.pathZ(&zbuf, path)) |z| {
        _ = linux.unlink(z);
    } else |_| {}
}

/// The kill: a byte copy of the live image (the process state abandoned).
fn snapshotImage(src: []const u8, dst: []const u8) !void {
    var sz: [linux.PATH_MAX]u8 = undefined;
    var dz: [linux.PATH_MAX]u8 = undefined;
    const sfd_rc = linux.open(try align_probe.pathZ(&sz, src), .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
    if (linux.errno(sfd_rc) != .SUCCESS) return error.CannotOpen;
    const sfd: linux.fd_t = @intCast(sfd_rc);
    defer _ = linux.close(sfd);
    const dfd_rc = linux.open(try align_probe.pathZ(&dz, dst), .{ .ACCMODE = .WRONLY, .CLOEXEC = true, .CREAT = true, .TRUNC = true }, 0o644);
    if (linux.errno(dfd_rc) != .SUCCESS) return error.CannotCreate;
    const dfd: linux.fd_t = @intCast(dfd_rc);
    defer _ = linux.close(dfd);
    var buf: [1 << 20]u8 = undefined;
    var off: u64 = 0;
    while (off < test_volume_len) {
        const n = linux.pread(sfd, &buf, buf.len, @intCast(off));
        if (linux.errno(n) != .SUCCESS or n == 0) return error.ReadFailed;
        const w = linux.pwrite(dfd, &buf, n, @intCast(off));
        if (linux.errno(w) != .SUCCESS or w != n) return error.WriteFailed;
        off += n;
    }
    if (linux.errno(linux.fsync(dfd)) != .SUCCESS) return error.SyncFailed;
}

/// A guest-slot inode record (the slot's tree is minted on first touch).
fn putInodeInSlot(be: *KvBackend, slot: u16, local: u64) ![]const u8 {
    const global = key.guestLocalIno(slot, local);
    _ = try be.mintGuestTree(slot);
    const k = try testing.allocator.dupe(u8, &key.inodeKey(global));
    return k;
}

test "symmetric default: format → mount A (the manager: region 0 Live with is_manager, term 1, the lease held) → an in-process second region joins (page Live, ring carved + zeroed, grant, incarnation) → both append to their own rings → checkpoint → close → remount: every record present, term 2, the second page Free, the ring's extents back" {
    const gpa = testing.allocator;
    const v = try TestVol.create(gpa, "e2e", .symmetric);
    defer v.destroy();
    const iv: record.InodeValue = .{ .mode = 0o100644, .nlink = 1 };
    const value = iv.encode();
    var keys: std.ArrayList([]const u8) = .empty;
    defer {
        for (keys.items) |k| gpa.free(k);
        keys.deinit(gpa);
    }
    var region_ring: page.SegmentTable = .{};
    const refusals_before = manager.stats.manager_verb_refusals.load();
    const conflicts_before = manager.stats.extent_grant_conflicts.load();
    const rejected_before = join.stats.joined_wire_words_rejected.load();
    {
        const be = try v.open();
        try testing.expect(be.sb.symmetricPlaneStamped());
        const set = be.sym.?;
        const r0 = set.region0();
        try testing.expect(r0.core.page.is_manager and r0.core.page.state == .live and r0.core.page.term == 1);
        try testing.expect(r0.core.page.identity.eql(set.identity));
        try testing.expectEqual(manager.Lease.held, set.manager.lease);
        try testing.expectEqual(@as(usize, 0), set.unrecovered.items.len);
        // The manager's page is on the device, newest-valid over the four slots.
        const p0 = (try set.manager.readPageOf(0)).?;
        try testing.expect(p0.is_manager and p0.term == 1);
        // The in-process second region.
        const r1 = try set.joinInProcess(join.HARNESS_MOUNT_SLOT_BASE);
        try testing.expectEqual(@as(u32, 1), r1.appenderId());
        try testing.expectEqual(region.RING_FLOOR_BYTES, r1.core.ringBytes());
        try testing.expect(r1.grant.unclaimedCount() >= grant.GRANT_EXTENTS_FLOOR);
        try testing.expect(!r1.seq.raises);
        region_ring = r1.core.table;
        const p1 = (try set.manager.readPageOf(1)).?;
        try testing.expect(p1.state == .live and !p1.is_manager and p1.term == 1 and p1.identity.sameMount(r1.identity));
        // The ring was zeroed: its first page verifies nothing.
        const ring_img = try be.dev.readRange(gpa, region_ring.items[0].start, 4096 * 3);
        defer gpa.free(ring_img);
        try testing.expect(std.mem.allEqual(u8, ring_img, 0));
        // Region 0 (the manager) writes through the conveyor: slot 3's records.
        for (0..6) |i| {
            const t = try be.begin(@src());
            const k = try putInodeInSlot(be, 3, 10 + i);
            try keys.append(gpa, k);
            try t.stagePut(key.TREE_INODES, k, &value);
            try be.commit(t);
        }
        // Region 1 appends into ITS ring: slot 5's records.
        for (0..6) |i| {
            const k = try putInodeInSlot(be, 5, 100 + i);
            try keys.append(gpa, k);
            try set.appendPut(r1, key.TREE_INODES, k, &value);
        }
        try testing.expectEqual(@as(u64, 6), r1.entries);
        try testing.expect(r1.core.core.headPos() > r1.core.page.ledger_tail_seq);
        for (keys.items) |k| {
            const got = try be.lookup(gpa, key.TREE_INODES, k);
            defer if (got) |g| gpa.free(g);
            try testing.expect(got != null);
        }
        // A checkpoint: both pages written; the region's tail reaches its
        // head at the fixpoint (one cycle to flush, the next to name it).
        try be.checkpointNow();
        try be.checkpointNow();
        try testing.expectEqual(r1.core.core.headPos(), r1.core.page.ledger_tail_seq);
        const p1b = (try set.manager.readPageOf(1)).?;
        try testing.expectEqual(r1.core.core.headPos(), p1b.ledger_tail_seq);
        try testing.expect(p1b.ckpt_seq > 0 and p1b.generation > p1.generation);
        var buf: [1024]u8 = undefined;
        var w: std.Io.Writer = .fixed(&buf);
        try set.writeStatsJson(&w);
        try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"manager_lease\":\"held\",\"appenders_live\":2") != null);
        be.close();
    }
    {
        const be = try v.open();
        defer be.close();
        try testing.expect(be.replayStats().clean_unmount);
        const set = be.sym.?;
        try testing.expectEqual(@as(u64, 2), set.region0().core.page.term);
        try testing.expectEqual(@as(usize, 0), set.unrecovered.items.len);
        for (keys.items) |k| {
            const got = try be.lookup(gpa, key.TREE_INODES, k);
            defer if (got) |g| gpa.free(g);
            try testing.expect(got != null);
        }
        // The second page is Free, term 1 kept; its ring's extents are free again.
        const p1 = (try set.manager.readPageOf(1)).?;
        try testing.expect(p1.state == .free and p1.term == 1 and p1.segments.len == 0);
        for (region_ring.slice()) |s| {
            var e = (s.start - be.sb.heap.start) / be.sb.node_size;
            const end = e + s.len / be.sb.node_size;
            while (e < end) : (e += 1) try testing.expect(!be.alloc.isAllocated(e));
        }
        try testing.expectEqual(refusals_before, manager.stats.manager_verb_refusals.load());
        try testing.expectEqual(conflicts_before, manager.stats.extent_grant_conflicts.load());
        try testing.expectEqual(rejected_before, join.stats.joined_wire_words_rejected.load());
    }
}

test "kill -9 with two regions live (the image copied mid-life): the remount recovers its own ring and every record region 0 acked; the second region's page shows Live under another mount slot — unrecovered, loud, counted" {
    const gpa = testing.allocator;
    const v = try TestVol.create(gpa, "kill", .symmetric);
    defer v.destroy();
    var dst_buf: [160]u8 = undefined;
    const dst = try std.fmt.bufPrint(&dst_buf, "{s}.cut", .{v.path()});
    defer unlinkPath(dst);
    const iv: record.InodeValue = .{ .mode = 0o100644, .nlink = 1 };
    const value = iv.encode();
    var keys: std.ArrayList([]const u8) = .empty;
    defer {
        for (keys.items) |k| gpa.free(k);
        keys.deinit(gpa);
    }
    {
        const be = try v.open();
        defer be.close();
        const set = be.sym.?;
        const r1 = try set.joinInProcess(join.HARNESS_MOUNT_SLOT_BASE + 1);
        for (0..4) |i| {
            const t = try be.begin(@src());
            const k = try putInodeInSlot(be, 7, 10 + i);
            try keys.append(gpa, k);
            try t.stagePut(key.TREE_INODES, k, &value);
            try be.commit(t);
        }
        const k1 = try putInodeInSlot(be, 9, 500);
        defer gpa.free(k1);
        try set.appendPut(r1, key.TREE_INODES, k1, &value);
        try be.fsync();
        try snapshotImage(v.path(), dst);
    }
    log.testing.expectErrorLogs(0);
    const before = sym.stats.appender_live_pages_at_mount.load();
    const copy = try v.openAt(dst, 1);
    defer copy.close();
    const set = copy.sym.?;
    try testing.expectEqual(before + 1, sym.stats.appender_live_pages_at_mount.load());
    try testing.expect(set.isUnrecovered(1));
    try testing.expect(!set.isUnrecovered(0));
    try testing.expectEqual(@as(usize, 1), set.unrecovered.items.len);
    const p1 = (try set.manager.readPageOf(1)).?;
    try testing.expect(p1.state == .live and p1.identity.mount_slot == join.HARNESS_MOUNT_SLOT_BASE + 1);
    // The manager's own ring replayed: every acked record of region 0.
    for (keys.items) |k| {
        const got = try copy.lookup(gpa, key.TREE_INODES, k);
        defer if (got) |g| gpa.free(g);
        try testing.expect(got != null);
    }
    try testing.expectEqual(@as(u64, 2), set.region0().core.page.term);
    try testing.expectEqual(manager.Lease.held, set.manager.lease);
}

test "the single-writer class still mounts: no appender set, no lease, a flat ring" {
    const gpa = testing.allocator;
    const v = try TestVol.create(gpa, "flat", .single_writer);
    defer v.destroy();
    const be = try v.open();
    defer be.close();
    try testing.expect(!be.sb.symmetricPlaneStamped());
    try testing.expect(be.sym == null);
    try testing.expectEqual(be.sb.journal.start, be.ring_layout.base);
}

test "SQUEEZEFS_SYMMETRIC_META=0 on a stamped volume refuses the writable open (once armed, stays armed)" {
    const gpa = testing.allocator;
    const v = try TestVol.create(gpa, "armed", .symmetric);
    defer v.destroy();
    try v.env.put("SQUEEZEFS_SYMMETRIC_META", "0");
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.UnsupportedFormat, v.open());
    try log.testing.expectAllFired();
}
