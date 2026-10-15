//! The slot-lease PLANE bound to a writable backend (Linux; charter §5.5;
//! `docs/porting/sym_slot_leases.md` §2.1 the arm, §3.1 the door at step
//! 0, §4.5–§4.8 the accept / the transfer / the cadence, §5 the page
//! homes): ONE per `KvBackend`, created BEFORE its conveyor so the door is
//! the conveyor's step 0 from the first commit, ARMED by the appender set
//! once the manager lease is held (a single-writer volume's plane stays
//! unarmed: no lease verdicts, the cutover state alone parks).
//!
//! In one process every region over a volume shares this plane: region
//! 0's commits enter the door through the conveyor seam (`me = 0`), an
//! in-process region's through `enterFor` around its own append, and the
//! table is the ONE truth the requester's refusal and the holder's offer
//! read — the refused foreign touch IS the requester's ship until the
//! cross-owner intents (wave C) ship it for real.

const std = @import("std");
const builtin = @import("builtin");
const Allocator = std.mem.Allocator;
const Io = std.Io;

const counters = @import("../../../core/counters.zig");
const clock = @import("../../../core/clock.zig");
const log = @import("../../../core/log.zig");
const knobs = @import("../../../core/knobs.zig");
const sync = @import("../../../sync/sync.zig");
const backend = @import("../../backend.zig");
const conveyor = @import("../../conveyor.zig");
const tx = @import("../../tx.zig");
const key = @import("../../key.zig");
const inos = @import("../../inos.zig");
const tree = @import("../../tree.zig");
const journal = @import("../../journal.zig");
const route_table = @import("../../routing/route_table.zig");
const jc = @import("../../journal_core.zig");
const cr = @import("../control_records.zig");
const page = @import("../appender_page.zig");
const bind = @import("../bind.zig");
const joined = @import("../joined.zig");
const region_mod = @import("../region.zig");
const shard_mod = @import("../membership/shard.zig");
const join_ladder_mod = @import("../membership/join_ladder.zig");
const manager = @import("../manager.zig");
const tails_spill = @import("tails_spill.zig");
const appender_dir = @import("../appender_dir.zig");
const slots = @import("../slots.zig");
const verbs_wire = @import("../../../wire/verbs.zig");
const lt = slots.lease_table;
const door_mod = slots.door;
const acquire = slots.acquire;
const handover = slots.handover;
const page_budget = slots.page_budget;
const resolve = slots.resolve;
const fencing = @import("../membership/fencing.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("sym/slots/plane is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

const KvBackend = backend.KvBackend;
const AppenderId = lt.AppenderId;

/// The cold-start barrier term: one device barrier's order on NVMe.
pub const COLD_BARRIER_NS: u64 = std.time.ns_per_ms;
/// The cold-start ship term: the fleet's measured ≈ 0.3 ms served ship.
pub const COLD_SHIP_NS: u64 = 300 * std.time.ns_per_us;

pub const Plane = struct {
    gpa: Allocator,
    io: Io,
    be: *KvBackend,
    table: *lt.LeaseTable,
    door: door_mod.Door,
    judge: handover.Judge,
    cache: resolve.SlotHolderCache,
    seam: handover.Seam = .{},
    /// The manager's verbs, valid once armed by an appender set.
    verbs: acquire.Verbs = undefined,
    set: ?*bind.AppenderSet = null,
    /// The joined posture (charter §5.7): the lessee's verbs go over the
    /// wire to the manager; `set` is null.
    joined: ?*joined.JoinedSet = null,
    /// The committing appender of this volume's own ring: 0 on the
    /// manager, the joined region's id on a joiner.
    me: AppenderId = journal.FIXED_RING_APPENDER,
    /// `M` and `A_max` in force (the manager's `verbs` carry theirs; a
    /// joiner has no verbs object).
    m: u64 = 0,
    a_max: u64 = 0,
    /// One handover at a time per volume on a joiner (the manager's verbs
    /// carry their own lock).
    handover_lock: Io.Mutex = .init,
    /// A joiner's wire-grant installs, one at a time (`installWireGrant`).
    install_lock: Io.Mutex = .init,
    armed: bool = false,
    /// The process-wide `slot_leases_held` gauge carries this plane's share
    /// from the arm on: `gauge_share` is the count last folded in, re-read
    /// from the table at every lease change (`syncLeaseGauge`), so the
    /// gauge is always Σ of the planes' shares — never a running sum of
    /// per-event deltas that two event classes can disagree on.
    gauge_counted: bool = false,
    gauge_share: u64 = 0,
    gauge_lock: jc.SpinLock = .{},
    /// Per held slot: what is durably named where.
    pub_lock: jc.SpinLock = .{},
    publications: std.AutoHashMapUnmanaged(u16, page_budget.Publication) = .empty,
    /// The directory-parent memo the filesystem binds (null = the shipped
    /// per-slot count: a miss credits nothing); global inos.
    memo: ?handover.ParentMemo = null,
    /// The filesystem's parked-times drain the handover's step 0 runs
    /// (null = no filesystem over this plane: a test rig's bare set).
    times_drain: ?handover.TimesDrain = null,
    /// The cadence's single flight after a tick.
    cadence: sync.TaskSet = .init,
    cadence_busy: std.atomic.Value(bool) = .init(false),
    t_idle_ns: u64,
    cooldown_ns: u64,
    affinity_mib: ?u64,
    /// The region ids this process holds (the own seam reads it latch-free).
    own_ids: [8]std.atomic.Value(u32) = @splat(.init(0)),
    own_n: std.atomic.Value(u8) = .init(1),
    /// The frame screen's and stamp's ONE source over the table, PRIMED
    /// here — before the open's first frame writer (the ring-recovery
    /// preflight inside the claim gate) — and installed into the cache.
    frames: fencing.FramePlane,

    pub const InitError = error{ OutOfMemory, Io, Corrupt };

    pub fn init(gpa: Allocator, io: Io, be: *KvBackend, env: *const std.process.Environ.Map) InitError!*Plane {
        const self = try gpa.create(Plane);
        errdefer gpa.destroy(self);
        const table = try lt.LeaseTable.init(gpa);
        errdefer table.deinit();
        const t_idle_ns: u64 = (knobs.optIntKnob("SQUEEZEFS_SYM_T_IDLE_MS", u64, env) orelse (handover.T_IDLE_DEFAULT_NS / std.time.ns_per_ms)) * std.time.ns_per_ms;
        const cooldown_ns = handover.recallCooldownNs(knobs.optIntKnob("SQUEEZEFS_DLM_RECALL_COOLDOWN_MS", u64, env), t_idle_ns);
        self.* = .{
            .gpa = gpa,
            .io = io,
            .be = be,
            .table = table,
            .door = undefined,
            .judge = handover.Judge.init(gpa, table, t_idle_ns, cooldown_ns),
            .cache = undefined,
            .t_idle_ns = t_idle_ns,
            .cooldown_ns = cooldown_ns,
            .affinity_mib = knobs.optIntKnob("SQUEEZEFS_SYM_AFFINITY_MAX_MB", u64, env),
            .frames = fencing.FramePlane.init(gpa, table, be.stamp.native_slot),
        };
        errdefer self.frames.deinit();
        self.seam.hold = std.meta.stringToEnum(handover.Hold, knobs.enumKnob("SQUEEZEFS_TEST_HANDOVER_HOLD", env)) orelse .off;
        self.door = .{ .table = table, .io = io, .armed = false, .hosted = .{ .ctx = self, .hosts = hostsFn }, .foreign_touch = .{ .ctx = self, .note = foreignTouchFn }, .t_idle_ns = t_idle_ns };
        self.cache = resolve.SlotHolderCache.init(gpa, table, self, ownsFn);
        // The priming law (charter §5.5 "Every writer's open primes its
        // frame stamps at its FIRST frame writer"): on a symmetric volume
        // the seam is in the cache before the preflight can flush a frame.
        if (be.sb.symmetricPlaneStamped()) try self.primeFrames();
        return self;
    }

    /// `prime_frame_stamps`: tree 0's `slot_state` population into the
    /// table and its `slot_tails` into the recorded-tail index, the
    /// manager's stamp overrides (a re-adopted `Leased{0, g}` stamps `g`,
    /// an `Unleased` native slot the arm's `g + 1`), the seam installed.
    /// The test seam `fencing.test_unprimed_first_writer` reproduces the
    /// reference's loss class by leaving the writer unprimed until the arm.
    fn primeFrames(self: *Plane) InitError!void {
        const be = self.be;
        const gpa = self.gpa;
        const control = be.trees.find(.control) orelse return error.Corrupt;
        var kvs: std.ArrayList(tree.KeyValue) = .empty;
        defer tree.deinitKeyValues(gpa, &kvs);
        const states = cr.rangeOf(.slot_state);
        control.range(gpa, states.start.slice(), states.end.slice(), std.math.maxInt(usize), &kvs) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
            else => error.Corrupt,
        };
        var held: std.ArrayList(u16) = .empty;
        defer held.deinit(gpa);
        var native_unleased: [1]u16 = undefined;
        var n_native: usize = 0;
        for (kvs.items) |kv| {
            const slot = cr.slotOfKey(kv.key) orelse continue;
            const s = cr.SlotStateWire.decode(kv.value) catch return error.Corrupt;
            try self.table.load(slot, s);
            if (s.variant != .unleased and s.appender_id == journal.FIXED_RING_APPENDER) try held.append(gpa, slot);
            if (s.variant == .unleased and be.stamp.native_slot == slot) {
                native_unleased[0] = slot;
                n_native = 1;
            }
        }
        // A native slot tree 0 never recorded is the arm's first grant at
        // g + 1 over the table's 0 — the same word.
        if (n_native == 0) if (be.stamp.native_slot) |ns| {
            if (self.table.view(ns).state == .unleased) {
                native_unleased[0] = ns;
                n_native = 1;
            }
        };
        tree.deinitKeyValues(gpa, &kvs);
        kvs = .empty;
        const tails = cr.rangeOf(.slot_tails);
        control.range(gpa, tails.start.slice(), tails.end.slice(), std.math.maxInt(usize), &kvs) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
            else => error.Corrupt,
        };
        var entries: std.ArrayList(cr.TailEntry) = .empty;
        defer entries.deinit(gpa);
        for (kvs.items) |kv| {
            const slot = cr.slotOfKey(kv.key) orelse continue;
            const d = cr.decodeSlotTails(kv.value) catch return error.Corrupt;
            entries.clearRetainingCapacity();
            if (d.spilled) {
                // The set folded back from its spill extents; images that
                // fail their hash leave the slot's leaves to screen by the
                // lease alone (counted there).
                if (!try tails_spill.read(gpa, .{ .ctx = self, .read = &readPageFn }, d, &entries)) continue;
            } else {
                var i: usize = 0;
                while (i < d.count) : (i += 1) try entries.append(gpa, d.tailEntryAt(i));
            }
            try self.table.recordTails(slot, d.g, entries.items);
        }
        if (!fencing.test_unprimed_first_writer.load(.acquire)) try self.frames.prime(held.items, native_unleased[0..n_native]);
        be.cache.installFrameSeam(self.frames.seam());
    }

    fn readPageFn(ctx: *anyopaque, offset: u64, out: *[appender_dir.PAGE_LEN]u8) error{Io}!void {
        const self = self_(ctx);
        const img = self.be.dev.readRange(self.gpa, offset, appender_dir.PAGE_LEN) catch return error.Io;
        defer self.gpa.free(img);
        @memcpy(out, img[0..appender_dir.PAGE_LEN]);
    }

    /// Before the backend's task and lane go: the cadence's flight joined
    /// (a transfer in progress completes — it is uncancelable inside its
    /// cycles), no later flight spawned.
    pub fn quiesce(self: *Plane) void {
        self.cadence.cancel(self.io);
    }

    pub fn deinit(self: *Plane) void {
        self.cadence.deinit(self.io);
        self.retireLeaseGauge();
        self.cache.deinit();
        self.judge.deinit();
        self.publications.deinit(self.gpa);
        self.frames.deinit();
        self.table.deinit();
        self.gpa.destroy(self);
    }

    pub fn doorSeam(self: *Plane) conveyor.DoorSeam {
        return .{ .ctx = self, .enter = doorEnterFn, .frontier = frontierFn };
    }

    fn frontierFn(ctx: *anyopaque, tokens: *const tx.DoorTokens, pos: u64) void {
        self_(ctx).noteRegionFrontier(tokens, pos);
    }

    /// An in-process region's append landed at `pos` of ITS ring: the
    /// frontier in the lessee's ring (the region's own `reusable_upto` is
    /// what the cover loop compares against).
    pub fn noteRegionFrontier(self: *Plane, tokens: *const tx.DoorTokens, pos: u64) void {
        for (tokens.slots[0..tokens.len]) |s| self.table.noteFrontier(s, pos);
    }

    fn self_(ctx: *anyopaque) *Plane {
        return @ptrCast(@alignCast(ctx));
    }

    // ---- the seams the door reads ----

    fn ownsFn(ctx: *anyopaque, id: AppenderId) bool {
        const self = self_(ctx);
        const n = self.own_n.load(.acquire);
        for (self.own_ids[0..n]) |*o| if (o.load(.acquire) == id) return true;
        return false;
    }

    fn hostsFn(ctx: *anyopaque, slot: u16) bool {
        return self_(ctx).be.stamp.hosts(slot);
    }

    /// A refused foreign touch: the requester's op on the holder's window
    /// (one process, one table); an offer when the rule says so.
    fn foreignTouchFn(ctx: *anyopaque, slot: u16, holder: AppenderId, requester: AppenderId) void {
        const self = self_(ctx);
        _ = self.noteShip(slot, holder, requester, false);
    }

    /// A served ship over the wire by `requester` on a slot this process
    /// holds (charter §5.5 "the holder OFFERS"): the requester's op noted
    /// on the window, the rule, the offer with the WIRE stand. Answers the
    /// holder when an offer was made (the caller's carriage push), null
    /// otherwise.
    pub fn noteWireShip(self: *Plane, slot: u16, requester: AppenderId) ?AppenderId {
        const holder = self.table.view(slot).holder;
        return if (self.noteShip(slot, holder, requester, true)) holder else null;
    }

    fn noteShip(self: *Plane, slot: u16, holder: AppenderId, requester: AppenderId, wire: bool) bool {
        if (!self.armed or !ownsFn(self, holder) or requester == holder) return false;
        const now = clock.monotonicNs();
        const d = self.judge.noteShip(slot, requester, now, wire) catch return false;
        if (d == .serve) return false;
        const expires = now + self.verbs.offerStandNs(wire);
        const r = self.verbs.offerSlot(holder, slot, requester, expires) catch return false;
        switch (r) {
            .offered => {
                handover.stats.slot_offers.inc();
                if (d == .offer_idle) handover.stats.slot_offers_idle.inc() else handover.stats.slot_offers_dominated.inc();
                return true;
            },
            .busy => {
                handover.stats.slot_offers_busy.inc();
                return false;
            },
        }
    }

    /// The region ids this process holds (the set keeps it current).
    pub fn setOwn(self: *Plane, ids: []const AppenderId) void {
        for (ids, 0..) |id, i| self.own_ids[i].store(id, .release);
        self.own_n.store(@intCast(ids.len), .release);
    }

    /// The leases this process's own regions hold on this volume
    /// (`own_ids`): a wire lessee's leases are its own process's share, so
    /// the fleet's processes sum every lease exactly once.
    fn ownLeaseCount(self: *Plane) u64 {
        const n = self.own_n.load(.acquire);
        var total: u64 = 0;
        for (self.own_ids[0..n]) |*o| total += self.table.heldCount(o.load(.acquire));
        return total;
    }

    /// The gauge's share re-read from the table: the difference since the
    /// last fold goes in (add before sub keeps Σ ≥ 0 across planes).
    /// Serialized per plane, so the last fold is the newest count.
    fn syncLeaseGauge(self: *Plane) void {
        if (!self.gauge_counted) return;
        self.gauge_lock.lock();
        defer self.gauge_lock.unlock();
        const now = self.ownLeaseCount();
        if (now >= self.gauge_share) slots.stats.slot_leases_held.add(now - self.gauge_share) else slots.stats.slot_leases_held.sub(self.gauge_share - now);
        self.gauge_share = now;
    }

    /// The plane leaves: its whole share comes out.
    fn retireLeaseGauge(self: *Plane) void {
        self.gauge_lock.lock();
        defer self.gauge_lock.unlock();
        slots.stats.slot_leases_held.sub(self.gauge_share);
        self.gauge_share = 0;
        self.gauge_counted = false;
    }

    // ---- the conveyor's step 0 ----

    fn doorEnterFn(ctx: *anyopaque, recs: []const tx.Rec, out: *tx.DoorTokens) conveyor.DoorError!void {
        const self = self_(ctx);
        return self.enterFor(self.me, recs, out);
    }

    /// The door for one entry's records committed by region `me`: the
    /// distinct slots the slot-tree records name (control records have no
    /// door), the subtree credit for every dentry-bearing record's parent.
    pub fn enterFor(self: *Plane, me: AppenderId, recs: []const tx.Rec, out: *tx.DoorTokens) conveyor.DoorError!void {
        var set: [door_mod.MAX_SLOTS]u16 = undefined;
        var n: usize = 0;
        var parents: [door_mod.MAX_SLOTS]u64 = undefined;
        var np: usize = 0;
        for (recs) |r| {
            if (!key.isSlotTreeKind(r.tree_id)) continue;
            const slot = route_table.slotOfKey(r.key) catch return error.Corrupt;
            var dup = false;
            for (set[0..n]) |s| dup = dup or s == slot;
            if (!dup) {
                if (n == set.len) return error.Corrupt;
                set[n] = slot;
                n += 1;
            }
            if (r.tree_id == key.TREE_DENTRIES and r.key.len == key.DENTRY_KEY_LEN and np < parents.len) {
                const parent = std.mem.readInt(u64, r.key[0..8], .big);
                var pd = false;
                for (parents[0..np]) |p| pd = pd or p == parent;
                if (!pd) {
                    parents[np] = parent;
                    np += 1;
                }
            }
        }
        if (n == 0) return;
        var tokens: door_mod.Tokens = .{};
        self.door.enter(set[0..n], me, &tokens) catch |e| return switch (e) {
            error.SlotBusy => error.SlotBusy,
            error.SlotMoved => error.SlotMoved,
            error.Deferred => error.Deferred,
            error.Io => error.Io,
            error.Corrupt => error.Corrupt,
            error.OutOfMemory => error.OutOfMemory,
        };
        out.* = .{ .ctx = self, .release_fn = doorReleaseFn, .slots = tokens.slots, .len = tokens.len };
        if (self.armed and self.memo != null) {
            const now = clock.monotonicNs();
            const m = self.memoView();
            for (parents[0..np]) |p| _ = handover.creditAncestors(self.table, m, route_table.globalOfEffective(p), set[0..n], now, self.t_idle_ns);
        }
    }

    /// The subtree memo over the filesystem's parent map: global inos,
    /// this volume's hosting and leases as the predicates.
    fn memoView(self: *Plane) handover.Memo {
        return .{ .ctx = self, .parentOf = memoParentOf, .slotOf = memoSlotOf, .leasesHere = memoLeasesHere, .onThisVolume = memoOnThisVolume };
    }

    fn memoParentOf(ctx: *anyopaque, ino: u64) ?u64 {
        const m = self_(ctx).memo orelse return null;
        return m.parentOf(m.ctx, ino);
    }

    fn memoSlotOf(_: *anyopaque, ino: u64) u16 {
        return route_table.slotOf(ino);
    }

    fn memoLeasesHere(ctx: *anyopaque, slot: u16) bool {
        const self = self_(ctx);
        const v = self.table.view(slot);
        return v.leasedBy(self.me) and v.state != .releasing;
    }

    fn memoOnThisVolume(ctx: *anyopaque, ino: u64) bool {
        return self_(ctx).be.stamp.hosts(route_table.slotOf(ino));
    }

    fn doorReleaseFn(ctx: *anyopaque, t: *tx.DoorTokens) void {
        const self = self_(ctx);
        var tokens: door_mod.Tokens = .{ .slots = t.slots, .len = t.len };
        self.door.exit(&tokens);
    }

    pub fn bindMemo(self: *Plane, memo: handover.ParentMemo) void {
        self.memo = memo;
    }

    pub fn bindTimesDrain(self: *Plane, drain: handover.TimesDrain) void {
        self.times_drain = drain;
    }

    /// A cutover moved `slot` off this volume: the lease, the view and the
    /// publication go (the engine deletes tree 0's record).
    pub fn slotLeft(self: *Plane, slot: u16) void {
        self.table.evict(slot);
        self.cache.forget(slot);
        self.judge.window.forget(slot);
        self.pub_lock.lock();
        _ = self.publications.remove(slot);
        self.pub_lock.unlock();
        self.syncLeaseGauge();
    }

    // ---- the arm (spec §2.1) ----

    pub const ArmError = error{ Io, Corrupt, OutOfMemory, Deferred, Refused };

    /// Load tree 0's population → settle region 0's page against it
    /// (rows 5/6, stale entries, C14) → re-adopt what tree 0 names as
    /// ours → the native slot → the rotor topped up to `M` → `N_floor`
    /// seeded → armed.
    pub fn arm(self: *Plane, set: *bind.AppenderSet) ArmError!void {
        const be = self.be;
        const gpa = self.gpa;
        self.set = set;
        const m = acquire.resolveMintSlots(knobs.optIntKnob("SQUEEZEFS_SYM_MINT_SLOTS", u64, be.env), set.manager.appenders_known);
        const a_max = acquire.resolveAffinityCeiling(self.affinity_mib, self.usedLeafBytes(), be.sb.node_size, be.sb.heap.len) catch |e| {
            log.err("meta volume {s}: SQUEEZEFS_SYM_AFFINITY_MAX_MB is {s} (node {d} B, heap {d} B) — refusing the mount", .{ be.path, @errorName(e), be.sb.node_size, be.sb.heap.len });
            return error.Refused;
        };
        self.verbs = acquire.Verbs.init(gpa, self.io, &set.manager, self.table, &self.door, .{
            .ctx = self,
            .rotorCandidates = rotorCandidatesFn,
            .nativeSlot = nativeSlotFn,
            .ownsRegion = ownsFn,
            .ring0Tail = ring0TailFn,
            .raiseRing0SeqFloor = raiseRing0SeqFloorFn,
            .install = installFn,
            .released = releasedFn,
            .foreignGranted = foreignGrantedFn,
            .transfer = transferFn,
            .parkForRoom = null,
            .treeImages = treeImagesFn,
            .regionImagesMoved = regionImagesMovedFn,
        }, m, a_max);
        self.m = m;
        self.a_max = a_max;
        set.manager.slot_plane = &self.verbs;
        self.door.first_touch = .{ .ctx = self, .acquire = firstTouchFn };
        // The appender → endpoint table (`PublishEndpoint`'s home) behind
        // the holder cache: a wire lessee's listener resolves from it.
        self.cache.endpoint_source = .{ .ctx = self, .lookup = &managerEndpointFn, .learn = false };
        // 1. Tree 0's population.
        var kvs: std.ArrayList(manager.KeyValue) = .empty;
        defer {
            for (kvs.items) |kv| {
                gpa.free(kv.key);
                gpa.free(kv.value);
            }
            kvs.deinit(gpa);
        }
        _ = try self.loadTree0Population(&kvs, null);
        // 2. Region 0's page settled against tree 0 (the backend adopted
        // the live entries' roots at open; here rows 5/6 and the stale
        // drops take their durable effect).
        const r0 = set.region0();
        var complete: std.ArrayList(struct { slot: u16, g: u32, words: lt.SlotWords }) = .empty;
        defer complete.deinit(gpa);
        for (r0.core.page.slotEntries()) |e| {
            const t0: ?cr.SlotState = blk: {
                for (kvs.items) |kv| if (cr.slotOfKey(kv.key) == e.slot) break :blk cr.SlotStateWire.decode(kv.value) catch return error.Corrupt;
                break :blk null;
            };
            switch (lt.settleEntry(t0, e, journal.FIXED_RING_APPENDER)) {
                // Row 5's words: root/cursor/extents from the entry;
                // `seq_floor = max(the ring's frontier, the recorded floor)`
                // — strictly above the grant's (the screen's law).
                .complete_release => |c| {
                    var words = c.words;
                    words.seq_floor = @max(be.reg.seqFrontier(&be.core), (if (t0) |t| t.seq_floor else 0) + 1);
                    try complete.append(gpa, .{ .slot = e.slot, .g = c.g, .words = words });
                },
                .drop_entry => {},
                .stale_entry => acquire.stats.slot_lease_stale_entries.inc(),
                .conflict => {
                    acquire.stats.slot_lease_conflicts.fire("a Live page entry at tree 0's generation under another lessee (C14)");
                    log.err("meta volume {s}: slot {d} is Live on appender 0's page at g {d} while tree 0 leases it to appender {d} — refusing the mount (C14; `squeezefs appender clear`)", .{ be.path, e.slot, e.g, if (t0) |t| t.appender_id else 0 });
                    return error.Corrupt;
                },
                .adopt => |a| {
                    self.table.noteLiveWords(e.slot, a.words.root, a.words.cursor, a.words.extents);
                    try self.setPublication(e.slot, .{ .root = a.words.root, .cursor = a.words.cursor, .home = .page });
                },
            }
        }
        self.armed = true;
        self.door.armed = true;
        // 3. Row 5's completions: the release the dead holder (us, a
        // previous incarnation) started lands now from the page's words —
        // before the re-adoption counts what we hold.
        for (complete.items) |c| {
            _ = self.verbs.releaseSlot(gpa, .{ .appender_id = journal.FIXED_RING_APPENDER, .slot = c.slot, .g = c.g, .words = c.words }) catch |e| switch (e) {
                error.Rejected, error.Refused => {},
                else => return mapVerb(e),
            };
        }
        // 4. Re-adopt ours (T7, no write): every lease tree 0 names for
        // region 0; a slot under a dead in-process region of a previous
        // incarnation stays refused until the recovery wave.
        var held: std.ArrayList(u16) = .empty;
        defer held.deinit(gpa);
        try self.table.heldBy(journal.FIXED_RING_APPENDER, &held);
        var rotor_now: usize = 0;
        for (held.items) |slot| {
            const v = self.table.view(slot);
            if (v.state == .releasing) continue;
            const words = self.table.wordsOf(slot);
            self.install(slot, journal.FIXED_RING_APPENDER, v.g, words, false) catch |e| return mapVerb(e);
            if (be.stamp.native_slot != slot and rotor_now < m) {
                self.table.setRotor(slot, true);
                rotor_now += 1;
            }
        }
        // 5. The native slot (KD-SYM-2), then the rotor topped up to M.
        if (be.stamp.native_slot) |ns| {
            const out = self.verbs.acquireSlots(gpa, .{ .appender_id = journal.FIXED_RING_APPENDER, .explicit = &.{ns} }) catch |e| return mapVerb(e);
            defer out.deinit(gpa);
            if (out == .refused) return error.Corrupt;
        }
        if (rotor_now < m) {
            const out = self.verbs.acquireSlots(gpa, .{ .appender_id = journal.FIXED_RING_APPENDER, .want = @intCast(m - rotor_now), .rotor = true }) catch |e| switch (e) {
                error.RotorAtCap => acquire.AcquireOutcome{ .granted = .{ .grants = &.{}, .already = 0 } },
                else => return mapVerb(e),
            };
            out.deinit(gpa);
        }
        // 6. N_floor's cold start before any measurement: a device barrier
        // of 1 ms and the fleet's ≈ 0.3 ms ship (spec §4.4) — N_floor 17;
        // the first handover and the first ships replace both.
        self.judge.seed(COLD_BARRIER_NS, COLD_SHIP_NS);
        self.gauge_counted = true;
        self.syncLeaseGauge();
        // The frame plane: the device fence decides rule 1/4's class; the
        // prime's overrides retire now that the table holds every lease.
        self.frames.setFenced(if (be.guard) |g| g.prClient() != null else false);
        self.frames.noteArmed();
        log.info("meta volume {s}: slot leases armed — M {d}, A_max {d} B, {d} leases held, T_idle {d} ms, N_floor {d}", .{ be.path, m, a_max, self.table.leasedCount(), self.t_idle_ns / std.time.ns_per_ms, self.judge.nFloorNow() });
    }

    fn mapBe(e: backend.Error) acquire.Error {
        return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
            else => error.Corrupt,
        };
    }

    fn mapVerb(e: acquire.Error) ArmError {
        return switch (e) {
            error.Io => error.Io,
            error.OutOfMemory => error.OutOfMemory,
            error.Deferred => error.Deferred,
            error.Refused, error.Rejected, error.Busy, error.RotorAtCap => error.Refused,
            error.Corrupt => error.Corrupt,
        };
    }

    /// Tree 0's `slot_state` population into the table (the manager's
    /// live truth; a joiner's checkpointed projection — fresher words
    /// arrive through the wire's replies and redirects, learnt into the
    /// holder cache). `projection_self` is the joined arm's own appender
    /// id: its rows go through `lt.joinedProjectionRow`; the count of rows
    /// that law demoted is returned.
    fn loadTree0Population(self: *Plane, kvs: *std.ArrayList(manager.KeyValue), projection_self: ?AppenderId) ArmError!u64 {
        const gpa = self.gpa;
        const r = cr.rangeOf(.slot_state);
        var raw: std.ArrayList(tree.KeyValue) = .empty;
        defer raw.deinit(gpa);
        self.be.range(gpa, key.TREE_CONTROL, r.start.slice(), r.end.slice(), std.math.maxInt(usize), &raw) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
            else => error.Corrupt,
        };
        errdefer tree.deinitKeyValues(gpa, &raw);
        for (raw.items) |kv| try kvs.append(gpa, .{ .key = kv.key, .value = kv.value });
        raw.items.len = 0;
        var demoted: u64 = 0;
        for (kvs.items) |kv| {
            const slot = cr.slotOfKey(kv.key) orelse continue;
            const row = cr.SlotStateWire.decode(kv.value) catch return error.Corrupt;
            const s = if (projection_self) |me| lt.joinedProjectionRow(row, me) else row;
            demoted += @intFromBool(s.variant != row.variant);
            try self.table.load(slot, s);
            if (s.variant == .leased) try self.cache.learn(slot, .{ .appender_id = s.appender_id, .g = s.g });
        }
        acquire.stats.slot_projection_stale_self_rows.add(demoted);
        return demoted;
    }

    /// The JOINED arm (charter §5.7; spec §5.2 rung 6): tree 0's
    /// projection into the table (every slot the manager and the other
    /// appenders lease is theirs for every door decision), the manager's
    /// endpoint for its slots, the rotor grants over the wire installed
    /// as OURS (the trees at the words' roots), the door's first touch
    /// bound to the wire, `N_floor` seeded, armed. Never a tree-0 write.
    pub fn armJoined(self: *Plane, j: *joined.JoinedSet, manager_ep: resolve.Endpoint) ArmError!void {
        const be = self.be;
        const gpa = self.gpa;
        self.joined = j;
        self.me = j.appenderId();
        self.setOwn(&.{self.me});
        self.m = j.m;
        self.a_max = acquire.resolveAffinityCeiling(self.affinity_mib, self.usedLeafBytes(), be.sb.node_size, be.sb.heap.len) catch |e| {
            log.err("meta volume {s}: SQUEEZEFS_SYM_AFFINITY_MAX_MB is {s} (node {d} B, heap {d} B) — refusing the mount", .{ be.path, @errorName(e), be.sb.node_size, be.sb.heap.len });
            return error.Refused;
        };
        acquire.stats.slot_rotor.set(self.m);
        acquire.stats.affinity_a_max_bytes.set(self.a_max);
        self.door.first_touch = .{ .ctx = self, .acquire = firstTouchFn };
        var kvs: std.ArrayList(manager.KeyValue) = .empty;
        defer {
            for (kvs.items) |kv| {
                gpa.free(kv.key);
                gpa.free(kv.value);
            }
            kvs.deinit(gpa);
        }
        const stale_self = try self.loadTree0Population(&kvs, self.me);
        if (stale_self != 0) log.info("meta volume {s}: {d} row(s) of tree 0's projection name appender {d} — a previous incarnation's leases (the rejoin's id is the hint's); loaded unleased, a first touch asks the manager", .{ be.path, stale_self, self.me });
        // The manager's slots resolve to its listener (the native slot
        // among them: `/`'s dentries are read under its tokens); any other
        // appender's through `ResolveEndpoint` at the manager.
        self.cache.setEndpoint(journal.FIXED_RING_APPENDER, manager_ep) catch return error.OutOfMemory;
        self.cache.endpoint_source = .{ .ctx = self, .lookup = &wireEndpointFn, .learn = true };
        self.armed = true;
        self.door.armed = true;
        // The rotor grants (rung 6): installed as ours, rotor-marked.
        const grants = j.acquireRotor() catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Deferred, error.MembershipPending => error.Deferred,
            error.Rejected, error.Busy, error.NotManager, error.SlotMoved => error.Refused,
            else => error.Io,
        };
        for (grants) |g| {
            try self.installWireGrant(g.slot, g.g, g.words, false);
            self.table.setRotor(g.slot, true);
        }
        self.judge.seed(COLD_BARRIER_NS, COLD_SHIP_NS);
        self.gauge_counted = true;
        self.syncLeaseGauge();
        // The frame plane: a joiner holds no device fence of its own (the
        // manager's WERO is the set's); its stamps are the table's words
        // for its leases from here on.
        self.frames.setFenced(false);
        self.frames.noteArmed();
        log.info("meta volume {s}: slot leases armed (JOINED, appender {d}) — M {d}, A_max {d} B, {d} rotor slot(s) held, {d} lease(s) known on tree 0, T_idle {d} ms, N_floor {d}", .{ be.path, self.me, self.m, self.a_max, grants.len, self.table.leasedCount(), self.t_idle_ns / std.time.ns_per_ms, self.judge.nFloorNow() });
    }

    /// The renewal carriage's receiving end on a joiner (charter §5.5
    /// "Handover … the holder OFFERS slot `s` to requester `q`"): an offer
    /// for a slot this mount touched is accepted with `AcquireSlot` over
    /// the wire; a release notice drops the learnt view.
    pub fn carriageSink(self: *Plane) shard_mod.CarriageSink {
        return .{ .ctx = self, .offered = &carriageOffered, .released = &carriageReleased };
    }

    fn carriageOffered(ctx: ?*anyopaque, slot: u16, _: u32) void {
        const self: *Plane = @ptrCast(@alignCast(ctx.?));
        const j = self.joined orelse return;
        self.firstTouchWire(j, slot) catch return;
        join_ladder_mod.stats.joined_wire_offers_accepted.inc();
    }

    fn carriageReleased(ctx: ?*anyopaque, slot: u16) void {
        const self: *Plane = @ptrCast(@alignCast(ctx.?));
        self.cache.forget(slot);
        const v = self.table.view(slot);
        if (v.state != .unleased and v.holder != self.me) self.table.evict(slot);
    }

    fn managerEndpointFn(ctx: *anyopaque, id: AppenderId) ?resolve.Endpoint {
        const self = self_(ctx);
        const set = self.set orelse return null;
        const rep = set.manager.resolveEndpoint(.{ .appender_id = id }) catch return null;
        return rep.endpoint;
    }

    fn wireEndpointFn(ctx: *anyopaque, id: AppenderId) ?resolve.Endpoint {
        const self = self_(ctx);
        const j = self.joined orelse return null;
        const wire_ep = (j.resolveEndpoint(id) catch return null) orelse return null;
        var ep: resolve.Endpoint = .{};
        const bytes = std.mem.asBytes(&wire_ep);
        @memcpy(ep.bytes[0..bytes.len], bytes);
        ep.len = @intCast(bytes.len);
        return ep;
    }

    /// A lease the wire granted THIS region: the table word (tree 0 names
    /// it from this reply on), the words installed as a fresh grant.
    ///
    /// The tree FIRST, the word LAST (0003 §X.5): the table word is what
    /// every reader of the lease acts on — the resolver's `local`, the
    /// door's admit, the applier's witness — so it is published only once
    /// the transferred root is in place (the word first let the accepter
    /// read its new slot as EMPTY: a create's parent `dying`, a stat
    /// ENOENT, and a commit in the gap minted an empty tree the install
    /// then kept, shadowing every record the holder wrote). `touch`: the
    /// grant answers this mount's first touch, so a virgin slot's tree is
    /// minted here (a rotor grant stays lazy until its first commit).
    /// Idempotent at the generation held (the carriage's accept and the
    /// door's replayed first touch may both land); serialized.
    fn installWireGrant(self: *Plane, slot: u16, g: u32, words: lt.SlotWords, touch: bool) ArmError!void {
        if (!self.install_lock.tryLock()) {
            acquire.stats.slot_install_waits.inc();
            self.install_lock.lockUncancelable(self.io);
        }
        defer self.install_lock.unlock(self.io);
        const v = self.table.view(slot);
        if (v.leasedBy(self.me) and v.g >= g) return;
        // A joiner holds a slot's tree only while it leases the slot (its
        // release forgets it): one here at another root than the grant's is
        // a shadow no path may make any more (a commit's mint for a slot
        // this mount did not lease) — replaced, never written into over the
        // departing holder's records.
        if (self.be.slotTree(slot)) |t| if (words.root.addr != 0) {
            const r = t.rootPtr();
            if (r.addr != words.root.addr or r.seq != words.root.seq) {
                acquire.stats.slot_install_shadow_trees.fire("a wire grant found this joiner holding a tree of the slot at another root");
                self.be.forgetTree(slot);
            }
        };
        // Whatever this mount cached of the slot before (a former lease,
        // its leaves since appended by others in place) goes before the
        // grant's root is opened.
        if (self.be.slotTree(slot) == null) self.discardMovedNodes(slot);
        self.install(slot, self.me, g, words, true) catch |e| return mapVerb(e);
        // The custody move's second half (T1, 0003 §AD.1): the manager's
        // grant entry moved the tree's live images into this appender's
        // record; its grant takes them as held before the word names this
        // mount the holder (no SMO of ours reaches the tree before it).
        if (self.joined) |j| try self.joinedImagesMoved(j, slot, .gain);
        if (touch and words.root.addr == 0 and self.be.slotTree(slot) == null) _ = self.be.mintGuestTree(slot) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
            else => error.Corrupt,
        };
        self.parkMidInstall();
        try self.table.load(slot, .{ .variant = .leased, .appender_id = self.me, .g = g, .root = words.root, .cursor = words.cursor, .slot_tree_extents = words.extents, .seq_floor = words.seq_floor });
        self.syncLeaseGauge();
    }

    /// A joiner whose holder cache names ITSELF for `slot` while its table
    /// does not (the manager granted it — an accept in flight, or one
    /// whose reply was lost): the grant's words installed through a
    /// replayed first touch (the manager answers the holder's own lease
    /// with tree 0's words; a concurrent install is serialized and
    /// idempotent). True once the table names this mount.
    pub fn settleOwnGrant(self: *Plane, slot: u16) bool {
        const j = self.joined orelse return false;
        self.firstTouchWire(j, slot) catch return false;
        const settled = self.table.view(slot).leasedBy(self.me);
        if (settled) acquire.stats.slot_own_grants_settled.inc();
        return settled;
    }

    /// May a commit staging a record of `slot` mint its missing tree before
    /// the door decides (`MetaSet.commit`)? Unarmed: yes (no lease
    /// verdicts). Armed: a slot this mount leases; on the manager an
    /// unleased one too (its own table is the truth and it maintains every
    /// unleased tree). Never a slot another appender leases or a handover
    /// reserved for another, and never a joiner's unleased view (its
    /// first touch's install opens the grant's root or mints the virgin).
    pub fn mayMintAtCommit(self: *Plane, slot: u16) bool {
        if (!self.armed) return true;
        const v = self.table.view(slot);
        if (v.leasedBy(self.me)) return true;
        if (self.joined != null or v.state != .unleased) return false;
        const to = self.table.reservedFor(slot, clock.monotonicNs()) orelse return true;
        return to == self.me;
    }

    /// A joiner's half of a lease's custody move (0003 §AD.1): the images of
    /// its OWN tree of `slot` (the grant's root just installed, or the
    /// root just released) into or out of its grant, walked under the SMO
    /// lock (no SMO of this mount moves a node mid-walk).
    fn joinedImagesMoved(self: *Plane, j: *joined.JoinedSet, slot: u16, dir: manager.Manager.ImagesMove) error{ OutOfMemory, Io, Corrupt }!void {
        const be = self.be;
        const t = be.slotTree(slot) orelse return;
        var images: std.ArrayList(u64) = .empty;
        defer images.deinit(be.gpa);
        be.smo_lock.lockUncancelable(be.io);
        defer be.smo_lock.unlock(be.io);
        be.collectTreeExtents(t, &images) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
            else => error.Corrupt,
        };
        j.transferImages(images.items, dir);
    }

    /// `park_mid_install`: the harness holds the accepter between the two
    /// halves of its install.
    fn parkMidInstall(self: *Plane) void {
        if (self.seam.hold != .park_mid_install) return;
        self.seam.reached.set(self.io);
        self.seam.release.waitUncancelable(self.io);
    }

    /// `used_leaf_bytes`: Σ of every slot tree's node census × node_size.
    fn usedLeafBytes(self: *Plane) u64 {
        var n: u64 = 0;
        for (self.be.trees.load()) |t| if (t.forest_slot != null) {
            n += t.nodeCount();
        };
        return n * self.be.sb.node_size;
    }

    // ---- the verbs' seams ----

    fn rotorCandidatesFn(ctx: *anyopaque, gpa: Allocator) Allocator.Error![]u16 {
        const self = self_(ctx);
        const hosted = try self.be.stamp.hostedSlots(gpa);
        defer gpa.free(hosted);
        var out: std.ArrayList(u16) = .empty;
        errdefer out.deinit(gpa);
        for (hosted) |s| if (self.be.stamp.native_slot != s) try out.append(gpa, s);
        return out.toOwnedSlice(gpa);
    }

    fn nativeSlotFn(ctx: *anyopaque) ?u16 {
        return self_(ctx).be.stamp.native_slot;
    }

    fn ring0TailFn(ctx: *anyopaque) u64 {
        return self_(ctx).be.core.reusableUpto();
    }

    fn raiseRing0SeqFloorFn(ctx: *anyopaque, floor: u64) void {
        const be = self_(ctx).be;
        _ = be.reg.raiseSeqFloor(&be.core, floor);
    }

    fn installFn(ctx: *anyopaque, slot: u16, holder: AppenderId, g: u32, words: lt.SlotWords, fresh: bool) acquire.Error!void {
        return self_(ctx).install(slot, holder, g, words, fresh);
    }

    /// The install: the tree at the words' root (opened when the lease
    /// names one; an empty slot owns no extent until its first touch
    /// mints it), the census seeded, the cursor cell floored, the frontier
    /// reset to the receiving ring's head, the holder cache taught.
    fn install(self: *Plane, slot: u16, holder: AppenderId, g: u32, words: lt.SlotWords, fresh: bool) acquire.Error!void {
        const be = self.be;
        if (be.stamp.native_slot != slot) {
            if (be.slotTree(slot)) |t| {
                t.seedNodeCount(words.extents);
            } else if (words.root.addr != 0) {
                const opened = tree.KvTree.open(self.gpa, be.cache, .{ .slot = key.guestForestSlot(slot) }, .{ .addr = words.root.addr, .seq = words.root.seq }, &be.seq) catch |e| return switch (e) {
                    error.OutOfMemory => error.OutOfMemory,
                    error.Io => error.Io,
                    else => error.Corrupt,
                };
                be.adoptTree(opened) catch return error.OutOfMemory;
                opened.seedNodeCount(words.extents);
            }
            be.stamp.installFloor(slot, words.cursor) catch return error.OutOfMemory;
        }
        // The frontier resets to the RECEIVING ring's head (never max'ed
        // with the departing ring's positions); the receiving ring's seq
        // offset rises above the departing floor.
        if (self.regionOf(holder)) |r| {
            if (fresh) self.table.resetFrontier(slot, r.core.core.headPos());
            _ = r.core.reg.raiseSeqFloor(&r.core.core, words.seq_floor);
        } else {
            if (fresh) self.table.resetFrontier(slot, be.core.headPos());
            _ = be.reg.raiseSeqFloor(&be.core, words.seq_floor);
        }
        self.cache.learn(slot, .{ .appender_id = holder, .g = g }) catch return error.OutOfMemory;
        if (fresh) {
            try self.setPublicationE(slot, .{ .root = words.root, .cursor = words.cursor, .home = .tree0, .tree0_root = words.root });
        }
        self.syncLeaseGauge();
    }

    fn releasedFn(ctx: *anyopaque, slot: u16) void {
        const self = self_(ctx);
        self.cache.forget(slot);
        self.judge.window.forget(slot);
        self.table.resetHolderOps(slot);
        self.syncLeaseGauge();
        // The manager maintains every UNLEASED tree (charter §5.5): a tree
        // a wire lessee returned opens here at the released root — its
        // reads are local from now on (the divert's `local` verdict).
        if (self.joined == null) self.adoptUnleasedTree(slot);
    }

    fn adoptUnleasedTree(self: *Plane, slot: u16) void {
        const be = self.be;
        if (be.stamp.native_slot == slot or !be.stamp.hosts(slot)) return;
        const words = self.table.wordsOf(slot);
        if (be.slotTree(slot) != null) be.forgetTree(slot);
        // The cache barrier (0003 §X.5): the lessee appended into the
        // slot's leaves IN PLACE under the root pointer it returns — every
        // image this process cached before the grant is stale.
        self.discardMovedNodes(slot);
        if (words.root.addr == 0) return;
        const opened = tree.KvTree.open(self.gpa, be.cache, .{ .slot = key.guestForestSlot(slot) }, .{ .addr = words.root.addr, .seq = words.root.seq }, &be.seq) catch |e| {
            log.err("meta volume {s}: the tree of released slot {d} could not open at its recorded root 0x{x} ({s}) — its records serve only after a remount", .{ be.path, slot, words.root.addr, @errorName(e) });
            return;
        };
        be.adoptTree(opened) catch {
            opened.destroy();
            return;
        };
        opened.seedNodeCount(words.extents);
        be.stamp.installFloor(slot, words.cursor) catch {};
    }

    /// `Seams.treeImages` (0003 §AD.1): the images of `slot`'s tree at
    /// `root`, walked under the SMO lock (no SMO of this process moves a
    /// node mid-walk) — this process's own tree when it holds the slot's
    /// tree at that root and wrote it; else a transient tree over the
    /// device's images, the slot's cached nodes dropped first when another
    /// process wrote them (`fresh`: a lessee appends into its leaves and
    /// interiors IN PLACE under an unchanged pointer — the cache barrier).
    fn treeImagesFn(ctx: *anyopaque, gpa: Allocator, slot: u16, root: cr.Root, fresh: bool, out: *std.ArrayList(u64)) acquire.Error!void {
        const self = self_(ctx);
        const be = self.be;
        var images: std.ArrayList(u64) = .empty;
        defer images.deinit(be.gpa);
        {
            be.smo_lock.lockUncancelable(be.io);
            defer be.smo_lock.unlock(be.io);
            const live = if (fresh) null else if (be.slotTree(slot)) |t| blk: {
                const r = t.rootPtr();
                break :blk if (r.addr == root.addr and r.seq == root.seq) t else null;
            } else null;
            if (live) |t| {
                be.collectTreeExtents(t, &images) catch |e| return mapBe(e);
            } else {
                if (fresh) self.discardMovedNodes(slot);
                const t = tree.KvTree.open(self.gpa, be.cache, .{ .slot = key.guestForestSlot(slot) }, .{ .addr = root.addr, .seq = root.seq }, &be.seq) catch |e| return switch (e) {
                    error.OutOfMemory => error.OutOfMemory,
                    error.Io => error.Io,
                    else => error.Corrupt,
                };
                defer t.destroy();
                be.collectTreeExtents(t, &images) catch |e| return mapBe(e);
            }
        }
        try out.appendSlice(gpa, images.items);
    }

    /// `Seams.regionImagesMoved`: an in-process region's RAM grant follows
    /// its record.
    fn regionImagesMovedFn(ctx: *anyopaque, appender_id: AppenderId, extents: []const u64, dir: manager.Manager.ImagesMove) Allocator.Error!void {
        const r = self_(ctx).regionOf(appender_id) orelse return;
        switch (dir) {
            .gain => _ = try r.grant.transferIn(extents),
            .lose => _ = r.grant.transferOut(extents),
        }
    }

    /// A wire appender took `slot`: the unleased tree this process kept
    /// is the grantee's now — out of the cache (its reads go under the
    /// grantee's tokens; its writes refuse at the door).
    fn foreignGrantedFn(ctx: *anyopaque, slot: u16) void {
        const self = self_(ctx);
        self.be.forgetTree(slot);
        self.discardMovedNodes(slot);
    }

    /// THE CACHE BARRIER of a lease that crosses processes (0003 §X.5): a
    /// slot's lessee appends frames into its leaf images IN PLACE under an
    /// unchanged root pointer (the node's address and seq stay), so a node
    /// image cached while another process held — or before it took — the
    /// slot reads as current and is not: every mapping of the slot's nodes
    /// goes (clean ones: the departing holder flushed before its release;
    /// a reader of an evicted image re-resolves through the installed
    /// root), counted.
    fn discardMovedNodes(self: *Plane, slot: u16) void {
        const dropped = self.be.cache.discardSlotNodes(key.guestForestSlot(slot));
        acquire.stats.slot_move_nodes_discarded.add(dropped);
    }

    fn transferFn(ctx: *anyopaque, slot: u16, holder: AppenderId, _: AppenderId) acquire.Error!void {
        const self = self_(ctx);
        const t0 = clock.monotonicNs();
        _ = handover.transfer(self.gpa, &self.door, self.table, self.steps(), self.io, holder, slot) catch |e| return switch (e) {
            error.Busy => error.Busy,
            error.HandoverDeferred => error.Deferred,
            error.HeldAtSeam => error.Io,
            error.Io => error.Io,
            error.Corrupt => error.Corrupt,
            error.OutOfMemory => error.OutOfMemory,
        };
        handover.stats.slot_handovers.inc();
        self.judge.foldHandover(clock.monotonicNs() - t0);
        self.table.noteHandover(slot, clock.monotonicNs()) catch {};
    }

    fn firstTouchFn(ctx: *anyopaque, slot: u16, requester: AppenderId) door_mod.Error!void {
        const self = self_(ctx);
        if (self.joined) |j| return self.firstTouchWire(j, slot);
        const out = self.verbs.acquireSlot(self.gpa, requester, slot, .park) catch |e| return switch (e) {
            error.Deferred => error.Deferred,
            error.Busy, error.RotorAtCap, error.Refused, error.Rejected => error.SlotBusy,
            error.Io => error.Io,
            error.Corrupt => error.Corrupt,
            error.OutOfMemory => error.OutOfMemory,
        };
        defer out.deinit(self.gpa);
        if (out == .refused) return error.SlotBusy;
    }

    /// A joiner's first touch: `AcquireSlot` over the wire. Granted ⇒
    /// the words installed as ours (the door's re-read admits); busy ⇒
    /// the holder's word LOADED into the table (the view the door's
    /// re-read refuses on, the resolver's "ship to the holder").
    fn firstTouchWire(self: *Plane, j: *joined.JoinedSet, slot: u16) door_mod.Error!void {
        const out = j.acquireSlot(slot) catch |e| return switch (e) {
            error.Deferred, error.MembershipPending => error.Deferred,
            error.OutOfMemory => error.OutOfMemory,
            error.Rejected, error.Busy, error.NotManager => error.SlotBusy,
            else => error.Io,
        };
        switch (out) {
            .granted => |g| {
                defer self.gpa.free(g.grants);
                for (g.grants) |grant| {
                    self.installWireGrant(grant.slot, grant.g, grant.words, true) catch |e| return switch (e) {
                        error.OutOfMemory => error.OutOfMemory,
                        error.Io => error.Io,
                        error.Deferred => error.Deferred,
                        error.Refused, error.Corrupt => error.Corrupt,
                    };
                }
            },
            .refused => |r| {
                try self.table.load(slot, .{ .variant = .leased, .appender_id = r.holder, .g = r.g, .root = .{ .addr = 0, .seq = 0 }, .cursor = inos.LOCAL_INO_BASE });
                try self.cache.learn(slot, .{ .appender_id = r.holder, .g = r.g });
                return error.SlotBusy;
            },
        }
    }

    // ---- the transfer's steps ----

    pub fn steps(self: *Plane) handover.Steps {
        return .{ .ctx = self, .drainPendingTimes = drainPendingTimesFn, .cycleOnce = cycleOnceFn, .reusableUpto = reusableUptoFn, .rootPublished = rootPublishedFn, .recallTokens = recallTokensFn, .words = wordsFn, .tails = tailsFn, .pageReleasing = pageReleasingFn, .releaseDurably = releaseDurablyFn, .pageDrop = pageDropFn, .seam = &self.seam };
    }

    /// Step 0 of the handover: the slot's parked stamps (`economy.
    /// PendingTimes`, gated commits routed at the slot's door entry)
    /// journaled through the filesystem while this holder's door still
    /// admits them. A refinement that parks after the gate meets the moved
    /// door at its drain and is dropped counted — never a wedge.
    fn drainPendingTimesFn(ctx: *anyopaque, slot: u16) void {
        const self = self_(ctx);
        const d = self.times_drain orelse return;
        handover.stats.meta_kv_times_handover_drained.add(d.drainSlot(d.ctx, slot));
    }

    fn cycleOnceFn(ctx: *anyopaque, slot: u16) handover.Error!u64 {
        const self = self_(ctx);
        self.be.checkpointNow() catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Corrupt => error.Corrupt,
            else => error.Io,
        };
        return self.lesseeReusableUpto(slot);
    }

    fn reusableUptoFn(ctx: *anyopaque, slot: u16) u64 {
        return self_(ctx).lesseeReusableUpto(slot);
    }

    /// The lessee's ring's `reusable_upto`: ring 0's for region 0, the
    /// region's own core's for an in-process region.
    fn lesseeReusableUpto(self: *Plane, slot: u16) u64 {
        const holder = self.table.view(slot).holder;
        if (holder == self.me) return self.be.core.reusableUpto();
        const r = self.regionOf(holder) orelse return self.be.core.reusableUpto();
        return r.core.core.reusableUpto();
    }

    fn rootPublishedFn(ctx: *anyopaque, slot: u16) bool {
        const self = self_(ctx);
        const t = self.be.slotTree(slot) orelse return true;
        const root = t.rootPtr();
        const p = self.publication(slot) orelse return false;
        return p.home != .unpublished and p.root.addr == root.addr and p.root.seq == root.seq;
    }

    fn recallTokensFn(_: *anyopaque, _: u16) void {}

    fn wordsFn(ctx: *anyopaque, slot: u16) lt.SlotWords {
        return self_(ctx).liveWords(slot);
    }

    /// `slot_words_now`: the live root, the cell's cursor, the census,
    /// `max(the ring's stamp frontier, the recorded floor)`.
    pub fn liveWords(self: *Plane, slot: u16) lt.SlotWords {
        const be = self.be;
        const recorded = self.table.wordsOf(slot);
        var w = recorded;
        if (be.slotTree(slot)) |t| {
            const r = t.rootPtr();
            w.root = .{ .addr = r.addr, .seq = r.seq };
            w.extents = @intCast(@min(t.nodeCount(), std.math.maxInt(u32)));
        }
        if (be.stamp.native_slot == slot) {
            w.cursor = @max(w.cursor, be.ino.snapshot());
        } else if (be.stamp.cursorSnapshot(slot)) |c| w.cursor = @max(w.cursor, c);
        w.seq_floor = @max(recorded.seq_floor, be.reg.seqFrontier(&be.core));
        return w;
    }

    fn tailsFn(ctx: *anyopaque, gpa: Allocator, slot: u16) handover.Error![]cr.TailEntry {
        const self = self_(ctx);
        var out: std.ArrayList(cr.TailEntry) = .empty;
        errdefer out.deinit(gpa);
        if (self.be.slotTree(slot)) |t| self.be.collectLeafTails(t, &out) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
            else => error.Corrupt,
        };
        return out.toOwnedSlice(gpa) catch return error.OutOfMemory;
    }

    fn regionOf(self: *Plane, holder: AppenderId) ?*bind.Region {
        const set = self.set orelse return null;
        for (set.regions.items) |r| if (r.appenderId() == holder) return r;
        return null;
    }

    fn pageReleasingFn(ctx: *anyopaque, slot: u16, g: u32, words: lt.SlotWords) handover.Error!void {
        const self = self_(ctx);
        const holder = self.table.view(slot).holder;
        if (self.joined) |j| {
            if (holder != self.me) return error.Corrupt;
            return self.writeRegionPage(self.me, j.core(), slot, g, words);
        }
        const r = self.regionOf(holder) orelse return error.Corrupt;
        try self.writeRegionPage(r.appenderId(), &r.core, slot, g, words);
    }

    /// The page write of the region whose core this is: the manager's
    /// set for its regions, the joined set for its one.
    fn writeRegionCorePages(self: *Plane, appender_id: AppenderId, from_cycle: bool) handover.Error!void {
        const res = if (self.joined) |j| j.writePages(from_cycle) else blk: {
            const set = self.set orelse return error.Corrupt;
            const r = self.regionOf(appender_id) orelse return error.Corrupt;
            break :blk set.writePages(r, from_cycle);
        };
        res catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Corrupt => error.Corrupt,
            error.Io => error.Io,
        };
    }

    /// The page with `releasing` naming `slot` first (rank 0), the rest by
    /// the plan; publish-before-drop into tree 0; one barrier.
    fn writeRegionPage(self: *Plane, appender_id: AppenderId, core: *region_mod.RegionCore, releasing: ?u16, g: u32, words: lt.SlotWords) handover.Error!void {
        var held = try self.heldOf(appender_id, releasing, g, words);
        defer held.deinit(self.gpa);
        var plan = page_budget.plan(self.gpa, held.items, page.SLOT_PAGE_BUDGET) catch return error.OutOfMemory;
        defer plan.deinit(self.gpa);
        for (plan.demote) |s| try self.publishTree0(s);
        core.page.setSlots(plan.entries) catch return error.Corrupt;
        try self.writeRegionCorePages(appender_id, false);
        for (plan.entries) |e| try self.setPublicationH(e.slot, .{ .root = .{ .addr = e.root_addr, .seq = e.root_seq }, .cursor = e.cursor, .home = .page });
    }

    fn releaseDurablyFn(ctx: *anyopaque, slot: u16, g: u32, words: lt.SlotWords, tails: []const cr.TailEntry) handover.Error!void {
        const self = self_(ctx);
        const holder = self.table.view(slot).holder;
        if (self.joined) |j| {
            // The wire's `ReleaseSlot`: tree 0 `Unleased` under the
            // manager's pen; the table follows the reply.
            _ = j.releaseSlot(slot, g, words, tails) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.Deferred, error.MembershipPending, error.Busy => error.HandoverDeferred,
                error.Rejected, error.NotManager => error.Corrupt,
                else => error.Io,
            };
            // The custody move's second half (T5, 0003 §AD.1): the release's
            // entry took the tree's images off this appender's record. The
            // release has landed — a failed walk is loud, never an abort
            // that would reopen the door of a slot no longer ours.
            self.joinedImagesMoved(j, slot, .lose) catch |e| log.err("slot leases: slot {d} released, but this mount's grant could not drop the tree's images ({s}) — its closure keeps them held", .{ slot, @errorName(e) });
            _ = self.table.release(slot, holder, g, words, clock.monotonicNs()) catch return error.OutOfMemory;
            releasedFn(self, slot);
            // The tree is the manager's now: its nodes leave this cache
            // (a successor rewrites them; nothing here frees an extent).
            self.be.forgetTree(slot);
            self.discardMovedNodes(slot);
            return;
        }
        _ = self.verbs.releaseSlot(self.gpa, .{ .appender_id = holder, .slot = slot, .g = g, .words = words, .tails = tails }) catch |e| return switch (e) {
            error.Io => error.Io,
            error.OutOfMemory => error.OutOfMemory,
            error.Deferred, error.Busy => error.HandoverDeferred,
            error.Rejected, error.Refused, error.RotorAtCap, error.Corrupt => error.Corrupt,
        };
    }

    fn pageDropFn(ctx: *anyopaque, slot: u16) handover.Error!void {
        const self = self_(ctx);
        self.pub_lock.lock();
        _ = self.publications.remove(slot);
        self.pub_lock.unlock();
        // The page without the slot: every region's page that named it.
        if (self.joined) |j| {
            var named = false;
            for (j.core().page.slotEntries()) |e| named = named or e.slot == slot;
            if (named) try self.writeRegionPage(self.me, j.core(), null, 0, .{});
            return;
        }
        const set = self.set orelse return;
        for (set.regions.items) |r| {
            var named = false;
            for (r.core.page.slotEntries()) |e| named = named or e.slot == slot;
            if (named) try self.writeRegionPage(r.appenderId(), &r.core, null, 0, .{});
        }
    }

    // ---- publications and the page plans ----

    fn publication(self: *Plane, slot: u16) ?page_budget.Publication {
        self.pub_lock.lock();
        defer self.pub_lock.unlock();
        return self.publications.get(slot);
    }

    fn setPublication(self: *Plane, slot: u16, p: page_budget.Publication) ArmError!void {
        self.pub_lock.lock();
        defer self.pub_lock.unlock();
        self.publications.put(self.gpa, slot, p) catch return error.OutOfMemory;
    }

    fn setPublicationE(self: *Plane, slot: u16, p: page_budget.Publication) acquire.Error!void {
        self.setPublication(slot, p) catch return error.OutOfMemory;
    }

    fn setPublicationH(self: *Plane, slot: u16, p: page_budget.Publication) handover.Error!void {
        self.setPublication(slot, p) catch return error.OutOfMemory;
    }

    /// Every slot `holder` leases as the page writer sees it; `releasing`
    /// carries the caller's mid-handover entry with its frozen words.
    fn heldOf(self: *Plane, holder: AppenderId, releasing: ?u16, g: u32, words: lt.SlotWords) handover.Error!std.ArrayList(page_budget.Held) {
        var slots_held: std.ArrayList(u16) = .empty;
        defer slots_held.deinit(self.gpa);
        self.table.heldBy(holder, &slots_held) catch return error.OutOfMemory;
        var out: std.ArrayList(page_budget.Held) = .empty;
        errdefer out.deinit(self.gpa);
        for (slots_held.items) |s| {
            const v = self.table.view(s);
            const is_rel = releasing == s;
            const w = if (is_rel) words else self.liveWords(s);
            out.append(self.gpa, .{
                .slot = s,
                .g = if (is_rel) g else v.g,
                .root = w.root,
                .cursor = w.cursor,
                .extents = w.extents,
                .releasing = is_rel or v.state == .releasing,
                .native = self.be.stamp.native_slot == s,
                .pub_ = self.publication(s) orelse .{},
            }) catch return error.OutOfMemory;
        }
        return out;
    }

    /// One tree-0 `Leased` rewrite at the live words (the overflow law's
    /// publication; publish-before-drop), checkpoint-class.
    fn publishTree0(self: *Plane, slot: u16) handover.Error!void {
        const v = self.table.view(slot);
        if (v.state == .unleased or v.state == .migrating) return;
        const w = self.liveWords(slot);
        if (self.joined) |j| {
            // A joiner never writes tree 0: `PublishRoots` to the manager.
            const roots = [_]verbs_wire.SlotRootWire{.{ .root = .{ .addr = w.root.addr, .seq = w.root.seq }, .cursor = w.cursor, .slot_tree_extents = w.extents, .g = v.g, .slot = slot, .reserved = @splat(0) }};
            _ = j.publishRoots(&roots) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.Deferred, error.MembershipPending => error.HandoverDeferred,
                error.Rejected, error.NotManager => error.Corrupt,
                else => error.Io,
            };
        } else {
            const st: cr.SlotState = .{ .variant = if (v.state == .releasing and v.dead) .releasing else .leased, .appender_id = v.holder, .g = v.g, .root = w.root, .cursor = w.cursor, .slot_tree_extents = w.extents, .seq_floor = w.seq_floor };
            _ = self.be.writeSlotStateRecord(slot, st) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.Corrupt => error.Corrupt,
                else => error.Io,
            };
        }
        page_budget.stats.meta_kv_forest_page_publications_demoted.inc();
        page_budget.stats.slot_roots_published_overflow.inc();
        try self.setPublicationH(slot, .{ .root = w.root, .cursor = w.cursor, .home = .tree0, .tree0_root = w.root });
    }

    /// C4b (before the tail rule and barrier #1): for every region this
    /// process holds — every held slot whose root is not named by the
    /// page's LAST WRITTEN image is published into tree 0 (a slot leased
    /// this cycle, an overflow slot, a tree-0 home gone stale: the record's
    /// tail may pass its records only once a durable home names the root
    /// — the unpublished-root law; the C11 page write lands AFTER the
    /// record), then the page REFRESH of the slots the image already names
    /// with their live words (the cursor rides page 0 before barrier #1).
    /// An unarmed plane publishes nothing (the backend's rung-5 law stands).
    pub fn publishBeforeTail(self: *Plane) handover.Error!void {
        if (!self.armed) return;
        if (self.joined) |j| return self.publishRegionBeforeTail(self.me, j.core());
        const set = self.set orelse return;
        for (set.regions.items) |r| try self.publishRegionBeforeTail(r.appenderId(), &r.core);
    }

    fn publishRegionBeforeTail(self: *Plane, appender_id: AppenderId, core: *region_mod.RegionCore) handover.Error!void {
        var held = try self.heldOf(appender_id, null, 0, .{});
        defer held.deinit(self.gpa);
        const named = core.page.slotEntries();
        var will_name = self.gpa.alloc(u16, named.len) catch return error.OutOfMemory;
        defer self.gpa.free(will_name);
        for (named, 0..) |e, i| will_name[i] = e.slot;
        // Every unnamed root THIS cycle (never a bounded subset: a root
        // left for the next cycle is a root the tail may pass).
        const pubs = self.gpa.alloc(u16, held.items.len) catch return error.OutOfMemory;
        defer self.gpa.free(pubs);
        for (page_budget.tree0Publications(held.items, will_name, pubs)) |s| try self.publishTree0(s);
        // The pre-barrier refresh of the already-named slots.
        if (page_budget.cursorRefresh(self.gpa, core.page.slotEntries(), held.items) catch return error.OutOfMemory) |refreshed| {
            defer self.gpa.free(refreshed);
            core.page.setSlots(refreshed) catch return error.Corrupt;
            try self.writeRegionCorePages(appender_id, true);
            for (refreshed) |e| try self.setPublicationH(e.slot, .{ .root = .{ .addr = e.root_addr, .seq = e.root_seq }, .cursor = e.cursor, .home = .page });
        }
    }

    /// C11: the full page plan for the region `appender_id` whose core
    /// this is (the entries set on its page; the caller writes the page).
    /// Publish-before-drop into tree 0 runs here for the cut.
    pub fn planPage(self: *Plane, appender_id: AppenderId, core: *region_mod.RegionCore) handover.Error!void {
        if (!self.armed) return;
        var held = try self.heldOf(appender_id, null, 0, .{});
        defer held.deinit(self.gpa);
        var plan = page_budget.plan(self.gpa, held.items, page.SLOT_PAGE_BUDGET) catch return error.OutOfMemory;
        defer plan.deinit(self.gpa);
        for (plan.demote) |s| try self.publishTree0(s);
        core.page.setSlots(plan.entries) catch return error.Corrupt;
    }

    /// The page landed (C11): every named slot is page-homed at the words
    /// the image carried.
    pub fn pageLanded(self: *Plane, core: *region_mod.RegionCore) void {
        if (!self.armed) return;
        for (core.page.slotEntries()) |e| self.setPublication(e.slot, .{ .root = .{ .addr = e.root_addr, .seq = e.root_seq }, .cursor = e.cursor, .home = .page }) catch {};
    }

    // ---- the mint policy (the set's `allocateIno`) ----

    /// Does a child of `parent_slot` (a slot this volume hosts) mint on
    /// THIS volume rather than on the set's round-robin pick? Charter
    /// §5.5's parent affinity ranks above §5.4's member round-robin
    /// (decision 0003 §AA.1): a child on another member than its parent
    /// is a cross-owner intent — two device barriers per create, unlink,
    /// rename — where one member's single entry would do. Yes iff the
    /// parent is outside the native slot (`/` and the control records:
    /// the root's children are what spreads a set over its members) and,
    /// on an armed volume, this mount leases it (a foreign directory's
    /// child mints in the creator's rotor, §5.5).
    pub fn homesChildren(self: *Plane, parent_slot: u16) bool {
        if (self.be.stamp.native_slot == parent_slot) return false;
        if (!self.armed) return true;
        const pv = self.table.view(parent_slot);
        return pv.leasedBy(self.me) and pv.state != .releasing;
    }

    /// The slot a child of `parent_slot` mints into on this volume: the
    /// gather arm first, then `mint_choice` over the rotor; null = the
    /// caller's rotor (an unarmed plane).
    pub fn mintSlot(self: *Plane, parent_slot: u16, parent_on_this_volume: bool, gather: bool, rr: u64) ?u16 {
        if (!self.armed) return null;
        const be = self.be;
        const native = be.stamp.native_slot == parent_slot;
        if (acquire.gatherMintSlot(parent_on_this_volume, native, gather, parent_slot)) |s| return s;
        const pv = self.table.view(parent_slot);
        // `A_max(t)` is DYNAMIC: the live leaf census at every decision
        // (the static knob, when set, is the whole law).
        const a_max = if (self.affinity_mib) |_| self.a_max else acquire.affinityCeiling(self.usedLeafBytes(), be.sb.node_size);
        const parent_leased_non_native = parent_on_this_volume and !native and pv.leasedBy(self.me) and pv.state != .releasing;
        const parent_tree_bytes = self.treeBytes(parent_slot);
        if (acquire.affinityHolds(parent_leased_non_native, parent_tree_bytes, a_max)) {
            acquire.countChoice(.{ .affinity = parent_slot }, true);
            return parent_slot;
        }
        var rotor: [lt.LeaseTable.PICK_MAX]acquire.RotorTree = undefined;
        var n: usize = 0;
        var held: std.ArrayList(u16) = .empty;
        defer held.deinit(self.gpa);
        self.table.heldBy(self.me, &held) catch return null;
        for (held.items) |s| {
            const v = self.table.view(s);
            if (!v.rotor or v.state == .releasing or n == rotor.len) continue;
            rotor[n] = .{ .slot = s, .bytes = self.treeBytes(s) };
            n += 1;
        }
        const in: acquire.MintInputs = .{
            .parent_slot = parent_slot,
            .parent_leased_non_native = parent_leased_non_native,
            .parent_tree_bytes = parent_tree_bytes,
            .a_max = a_max,
            .node_size = be.sb.node_size,
            .rotor = rotor[0..n],
            .rotor_cap = acquire.rotorCap(self.m),
            .rr = rr,
        };
        const c = acquire.mintChoice(in);
        acquire.countChoice(c, in.parent_leased_non_native);
        return switch (c) {
            .overflow => self.overflowRotor(rotor[0..n]),
            .none => null,
            else => c.slot(),
        };
    }

    /// One more rotor slot (PARK admission in-process; `AcquireSlots(1)`
    /// over the wire on a joiner), else the smallest.
    fn overflowRotor(self: *Plane, rotor: []const acquire.RotorTree) ?u16 {
        if (self.joined) |j| {
            const out = j.acquireSlots(1) catch return smallestOf(rotor);
            switch (out) {
                .granted => |g| {
                    defer self.gpa.free(g.grants);
                    if (g.grants.len == 0) return smallestOf(rotor);
                    const grant = g.grants[0];
                    self.installWireGrant(grant.slot, grant.g, grant.words, false) catch return smallestOf(rotor);
                    self.table.setRotor(grant.slot, true);
                    acquire.stats.affinity_ceiling_overflows.inc();
                    return grant.slot;
                },
                .refused => return smallestOf(rotor),
            }
        }
        const out = self.verbs.acquireSlots(self.gpa, .{ .appender_id = journal.FIXED_RING_APPENDER, .want = 1, .admit = .park }) catch return smallestOf(rotor);
        switch (out) {
            .granted => |g| {
                defer out.deinit(self.gpa);
                if (g.grants.len == 0) return smallestOf(rotor);
                acquire.stats.affinity_ceiling_overflows.inc();
                return g.grants[0].slot;
            },
            .refused => return smallestOf(rotor),
        }
    }

    fn smallestOf(rotor: []const acquire.RotorTree) ?u16 {
        if (rotor.len == 0) return null;
        var best = rotor[0];
        for (rotor[1..]) |r| if (r.bytes < best.bytes) {
            best = r;
        };
        acquire.stats.rotor_mints.inc();
        return best.slot;
    }

    fn treeBytes(self: *Plane, slot: u16) u64 {
        const t = self.be.slotTree(slot) orelse return 0;
        return t.nodeCount() * self.be.sb.node_size;
    }

    // ---- the cadence (spec §4.8), one flight after a tick ----

    /// Spawn the cadence unless one is in flight (the tick never runs it
    /// inline: a release waits on admitted commits, and a commit parked at
    /// ring admission needs the tick).
    pub fn kickCadence(self: *Plane) void {
        if (!self.armed) return;
        if (self.cadence_busy.swap(true, .acq_rel)) return;
        self.cadence.spawn(self.io, cadenceRun, .{self}) catch {
            self.cadence_busy.store(false, .release);
        };
    }

    fn cadenceRun(self: *Plane) Io.Cancelable!void {
        defer self.cadence_busy.store(false, .release);
        self.runCadence();
    }

    /// 1. lapse offers → 2. forced shrink to the re-derived `M` → 3. LRU
    /// release past the page budget → the gauges. (Region release rides
    /// the in-process leave.)
    pub fn runCadence(self: *Plane) void {
        const gpa = self.gpa;
        const be = self.be;
        const now = clock.monotonicNs();
        var lapsed: [16]lt.LapsedOffer = undefined;
        const n_lapsed = self.table.expireOffers(now, &lapsed);
        handover.stats.slot_offers_expired.add(n_lapsed);
        const writers_known: u64 = if (self.set) |set| set.manager.appenders_known else if (self.joined) |j| j.writers_known else return;
        const m = acquire.resolveMintSlots(knobs.optIntKnob("SQUEEZEFS_SYM_MINT_SLOTS", u64, be.env), writers_known);
        const a_max = acquire.resolveAffinityCeiling(self.affinity_mib, self.usedLeafBytes(), be.sb.node_size, be.sb.heap.len) catch self.a_max;
        self.m = m;
        self.a_max = a_max;
        if (self.set != null) {
            self.verbs.setM(m);
            self.verbs.setCeiling(a_max);
        } else {
            acquire.stats.slot_rotor.set(m);
            acquire.stats.affinity_a_max_bytes.set(a_max);
        }
        var ids_buf: [8]AppenderId = undefined;
        var n_ids: usize = 0;
        if (self.set) |set| {
            for (set.regions.items) |r| {
                if (n_ids == ids_buf.len) break;
                ids_buf[n_ids] = r.appenderId();
                n_ids += 1;
            }
        } else {
            ids_buf[0] = self.me;
            n_ids = 1;
        }
        for (ids_buf[0..n_ids]) |id| {
            var held_slots: std.ArrayList(u16) = .empty;
            defer held_slots.deinit(gpa);
            self.table.heldBy(id, &held_slots) catch return;
            var held: std.ArrayList(acquire.HeldSlot) = .empty;
            defer held.deinit(gpa);
            for (held_slots.items) |s| {
                const v = self.table.view(s);
                if (v.state != .leased) continue;
                const t = be.slotTree(s);
                held.append(gpa, .{ .slot = s, .idle = self.table.holderOps(s, now, self.t_idle_ns) == 0, .rotor = v.rotor, .native = be.stamp.native_slot == s, .extents = @intCast(@min(if (t) |tt| tt.nodeCount() else 0, std.math.maxInt(u32))) }) catch return;
            }
            var victims: [lt.LeaseTable.PICK_MAX]u16 = undefined;
            for (acquire.forcedShrinkVictims(held.items, m, &victims)) |s| {
                if (self.releaseForCadence(id, s)) acquire.stats.slot_forced_shrinks.inc();
            }
            for (acquire.lruReleaseVictims(held.items, page.SLOT_PAGE_BUDGET, &victims)) |s| {
                if (self.releaseForCadence(id, s)) acquire.stats.slot_lru_releases.inc();
            }
        }
        self.publishGauges();
    }

    fn handoverLock(self: *Plane) *Io.Mutex {
        return if (self.set != null) &self.verbs.handover_lock else &self.handover_lock;
    }

    /// The handover's own sequence to nobody; `Busy`/deferred = not this tick.
    fn releaseForCadence(self: *Plane, holder: AppenderId, slot: u16) bool {
        const lock = self.handoverLock();
        lock.lockUncancelable(self.io);
        defer lock.unlock(self.io);
        _ = handover.transfer(self.gpa, &self.door, self.table, self.steps(), self.io, holder, slot) catch |e| {
            log.info("slot leases: the cadence's release of slot {d} (appender {d}) did not land this tick ({s})", .{ slot, holder, @errorName(e) });
            return false;
        };
        return true;
    }

    /// The leave: every held slot of `holder` released to nobody (the page
    /// names them Releasing first, each in its own transfer).
    pub fn releaseAllForLeave(self: *Plane, holder: AppenderId) void {
        var held: std.ArrayList(u16) = .empty;
        defer held.deinit(self.gpa);
        self.table.heldBy(holder, &held) catch return;
        for (held.items) |s| _ = self.releaseForCadence(holder, s);
    }

    fn publishGauges(self: *Plane) void {
        const gpa = self.gpa;
        var held: std.ArrayList(u16) = .empty;
        defer held.deinit(gpa);
        self.table.heldBy(self.me, &held) catch return;
        const inos_ = gpa.alloc(u64, held.items.len) catch return;
        defer gpa.free(inos_);
        const bytes = gpa.alloc(u64, held.items.len) catch return;
        defer gpa.free(bytes);
        for (held.items, 0..) |s, i| {
            const w = self.liveWords(s);
            inos_[i] = w.cursor -| inos.LOCAL_INO_BASE;
            bytes[i] = @as(u64, w.extents) * self.be.sb.node_size;
        }
        std.mem.sort(u64, inos_, {}, std.sort.asc(u64));
        std.mem.sort(u64, bytes, {}, std.sort.asc(u64));
        slots.publishTreeGauges(inos_, bytes);
    }

    /// The per-volume words of the `.stats` family.
    pub fn writeStatsJson(self: *Plane, w: *Io.Writer) Io.Writer.Error!void {
        // An unarmed plane (a flat volume) reads 0 throughout — `n_floor`
        // included, whose derivation has a floor of 2 the operator must
        // not mistake for an engaged handover rule.
        try w.print("{{\"armed\":{},\"leases_held\":{d},\"own_leases\":{d},\"rotor_m\":{d},\"a_max_bytes\":{d},\"n_floor\":{d},\"closed_slot\":{?d}}}", .{ self.armed, self.table.leasedCount(), if (self.armed) self.table.heldCount(self.me) else 0, if (self.armed) self.m else 0, if (self.armed) self.a_max else 0, if (self.armed) self.judge.nFloorNow() else 0, self.door.closedSlot() });
    }
};
