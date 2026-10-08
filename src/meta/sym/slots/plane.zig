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
const manager = @import("../manager.zig");
const slots = @import("../slots.zig");
const lt = slots.lease_table;
const door_mod = slots.door;
const acquire = slots.acquire;
const handover = slots.handover;
const page_budget = slots.page_budget;
const resolve = slots.resolve;

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
    /// The manager's verbs, valid once armed.
    verbs: acquire.Verbs = undefined,
    set: ?*bind.AppenderSet = null,
    armed: bool = false,
    /// The process-wide `slot_leases_held` gauge counts this plane's leases
    /// once the arm folded them in (the arm's own grants are in the fold).
    gauge_counted: bool = false,
    /// Per held slot: what is durably named where.
    pub_lock: jc.SpinLock = .{},
    publications: std.AutoHashMapUnmanaged(u16, page_budget.Publication) = .empty,
    /// The directory-parent memo the filesystem binds (null = the shipped
    /// per-slot count: a miss credits nothing); global inos.
    memo: ?handover.ParentMemo = null,
    /// The cadence's single flight after a tick.
    cadence: sync.TaskSet = .init,
    cadence_busy: std.atomic.Value(bool) = .init(false),
    t_idle_ns: u64,
    cooldown_ns: u64,
    affinity_mib: ?u64,
    /// The region ids this process holds (the own seam reads it latch-free).
    own_ids: [8]std.atomic.Value(u32) = @splat(.init(0)),
    own_n: std.atomic.Value(u8) = .init(1),

    pub fn init(gpa: Allocator, io: Io, be: *KvBackend, env: *const std.process.Environ.Map) Allocator.Error!*Plane {
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
        };
        self.seam.hold = std.meta.stringToEnum(handover.Hold, knobs.enumKnob("SQUEEZEFS_TEST_HANDOVER_HOLD", env)) orelse .off;
        self.door = .{ .table = table, .io = io, .armed = false, .hosted = .{ .ctx = self, .hosts = hostsFn }, .foreign_touch = .{ .ctx = self, .note = foreignTouchFn }, .t_idle_ns = t_idle_ns };
        self.cache = resolve.SlotHolderCache.init(gpa, table, self, ownsFn);
        return self;
    }

    /// Before the backend's task and lane go: the cadence's flight joined
    /// (a transfer in progress completes — it is uncancelable inside its
    /// cycles), no later flight spawned.
    pub fn quiesce(self: *Plane) void {
        self.cadence.cancel(self.io);
    }

    pub fn deinit(self: *Plane) void {
        self.cadence.deinit(self.io);
        if (self.gauge_counted) slots.stats.slot_leases_held.sub(self.table.leasedCount());
        self.cache.deinit();
        self.judge.deinit();
        self.publications.deinit(self.gpa);
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
        if (!self.armed or !ownsFn(ctx, holder)) return;
        const now = clock.monotonicNs();
        const d = self.judge.noteShip(slot, requester, now) catch return;
        if (d == .serve) return;
        const expires = now + self.verbs.offerStandNs(false);
        const r = self.verbs.offerSlot(holder, slot, requester, expires) catch return;
        switch (r) {
            .offered => {
                handover.stats.slot_offers.inc();
                if (d == .offer_idle) handover.stats.slot_offers_idle.inc() else handover.stats.slot_offers_dominated.inc();
            },
            .busy => handover.stats.slot_offers_busy.inc(),
        }
    }

    /// The region ids this process holds (the set keeps it current).
    pub fn setOwn(self: *Plane, ids: []const AppenderId) void {
        for (ids, 0..) |id, i| self.own_ids[i].store(id, .release);
        self.own_n.store(@intCast(ids.len), .release);
    }

    // ---- the conveyor's step 0 ----

    fn doorEnterFn(ctx: *anyopaque, recs: []const tx.Rec, out: *tx.DoorTokens) conveyor.DoorError!void {
        return self_(ctx).enterFor(journal.FIXED_RING_APPENDER, recs, out);
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
        const v = self_(ctx).table.view(slot);
        return v.leasedBy(journal.FIXED_RING_APPENDER) and v.state != .releasing;
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

    /// A cutover moved `slot` off this volume: the lease, the view and the
    /// publication go (the engine deletes tree 0's record).
    pub fn slotLeft(self: *Plane, slot: u16) void {
        const was_leased = self.table.view(slot).state == .leased;
        self.table.evict(slot);
        self.cache.forget(slot);
        self.judge.window.forget(slot);
        self.pub_lock.lock();
        _ = self.publications.remove(slot);
        self.pub_lock.unlock();
        if (was_leased and self.gauge_counted) slots.stats.slot_leases_held.sub(1);
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
            .transfer = transferFn,
            .parkForRoom = null,
        }, m, a_max);
        set.manager.slot_plane = &self.verbs;
        self.door.first_touch = .{ .ctx = self, .acquire = firstTouchFn };
        // 1. Tree 0's population.
        var kvs: std.ArrayList(manager.KeyValue) = .empty;
        defer {
            for (kvs.items) |kv| {
                gpa.free(kv.key);
                gpa.free(kv.value);
            }
            kvs.deinit(gpa);
        }
        const r = cr.rangeOf(.slot_state);
        set.manager.vol.vt.range(set.manager.vol.ctx, gpa, r.start.slice(), r.end.slice(), &kvs) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
        };
        for (kvs.items) |kv| {
            const slot = cr.slotOfKey(kv.key) orelse continue;
            const s = cr.SlotStateWire.decode(kv.value) catch return error.Corrupt;
            try self.table.load(slot, s);
            if (s.variant == .leased) try self.cache.learn(slot, .{ .appender_id = s.appender_id, .g = s.g });
        }
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
            switch (out) {
                .granted => |g| gpa.free(g.grants),
                .refused => return error.Corrupt,
            }
        }
        if (rotor_now < m) {
            const out = self.verbs.acquireSlots(gpa, .{ .appender_id = journal.FIXED_RING_APPENDER, .want = @intCast(m - rotor_now), .rotor = true }) catch |e| switch (e) {
                error.RotorAtCap => acquire.AcquireOutcome{ .granted = .{ .grants = &.{}, .already = 0 } },
                else => return mapVerb(e),
            };
            switch (out) {
                .granted => |g| if (g.grants.len != 0) gpa.free(g.grants),
                .refused => {},
            }
        }
        // 6. N_floor's cold start before any measurement: a device barrier
        // of 1 ms and the fleet's ≈ 0.3 ms ship (spec §4.4) — N_floor 17;
        // the first handover and the first ships replace both.
        self.judge.seed(COLD_BARRIER_NS, COLD_SHIP_NS);
        slots.stats.slot_leases_held.add(self.table.leasedCount());
        self.gauge_counted = true;
        log.info("meta volume {s}: slot leases armed — M {d}, A_max {d} B, {d} leases held, T_idle {d} ms, N_floor {d}", .{ be.path, m, a_max, self.table.leasedCount(), self.t_idle_ns / std.time.ns_per_ms, self.judge.nFloorNow() });
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
            if (self.gauge_counted) slots.stats.slot_leases_held.add(1);
        }
    }

    fn releasedFn(ctx: *anyopaque, slot: u16) void {
        const self = self_(ctx);
        self.cache.forget(slot);
        self.judge.window.forget(slot);
        self.table.resetHolderOps(slot);
        if (self.gauge_counted) slots.stats.slot_leases_held.sub(1);
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
        const out = self.verbs.acquireSlot(self.gpa, requester, slot, .park) catch |e| return switch (e) {
            error.Deferred => error.Deferred,
            error.Busy, error.RotorAtCap, error.Refused, error.Rejected => error.SlotBusy,
            error.Io => error.Io,
            error.Corrupt => error.Corrupt,
            error.OutOfMemory => error.OutOfMemory,
        };
        switch (out) {
            .granted => |g| if (g.grants.len != 0) self.gpa.free(g.grants),
            .refused => return error.SlotBusy,
        }
    }

    // ---- the transfer's steps ----

    pub fn steps(self: *Plane) handover.Steps {
        return .{ .ctx = self, .drainPendingTimes = drainPendingTimesFn, .cycleOnce = cycleOnceFn, .reusableUpto = reusableUptoFn, .rootPublished = rootPublishedFn, .recallTokens = recallTokensFn, .words = wordsFn, .tails = tailsFn, .pageReleasing = pageReleasingFn, .releaseDurably = releaseDurablyFn, .pageDrop = pageDropFn, .seam = &self.seam };
    }

    /// The port's pending-times refinements are gated commits routed at
    /// their own door entry (`economy.PendingTimes`): a refinement whose
    /// slot moved parks, then refuses `SlotBusy` and is dropped by its
    /// drain — never a wedge; nothing to drain here.
    fn drainPendingTimesFn(_: *anyopaque, _: u16) void {}

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
        if (holder == journal.FIXED_RING_APPENDER) return self.be.core.reusableUpto();
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
        const r = self.regionOf(holder) orelse return error.Corrupt;
        try self.writeRegionPage(r, slot, g, words);
    }

    /// The page with `releasing` naming `slot` first (rank 0), the rest by
    /// the plan; publish-before-drop into tree 0; one barrier.
    fn writeRegionPage(self: *Plane, r: *bind.Region, releasing: ?u16, g: u32, words: lt.SlotWords) handover.Error!void {
        var held = try self.heldOf(r.appenderId(), releasing, g, words);
        defer held.deinit(self.gpa);
        var plan = page_budget.plan(self.gpa, held.items, page.SLOT_PAGE_BUDGET) catch return error.OutOfMemory;
        defer plan.deinit(self.gpa);
        for (plan.demote) |s| try self.publishTree0(s);
        r.core.page.setSlots(plan.entries) catch return error.Corrupt;
        const set = self.set orelse return error.Corrupt;
        set.writePages(r, false) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Corrupt => error.Corrupt,
            error.Io => error.Io,
        };
        for (plan.entries) |e| try self.setPublicationH(e.slot, .{ .root = .{ .addr = e.root_addr, .seq = e.root_seq }, .cursor = e.cursor, .home = .page });
    }

    fn releaseDurablyFn(ctx: *anyopaque, slot: u16, g: u32, words: lt.SlotWords, tails: []const cr.TailEntry) handover.Error!void {
        const self = self_(ctx);
        const holder = self.table.view(slot).holder;
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
        const set = self.set orelse return;
        for (set.regions.items) |r| {
            var named = false;
            for (r.core.page.slotEntries()) |e| named = named or e.slot == slot;
            if (named) try self.writeRegionPage(r, null, 0, .{});
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
        const st: cr.SlotState = .{ .variant = if (v.state == .releasing and v.dead) .releasing else .leased, .appender_id = v.holder, .g = v.g, .root = w.root, .cursor = w.cursor, .slot_tree_extents = w.extents, .seq_floor = w.seq_floor };
        _ = self.be.writeSlotStateRecord(slot, st) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Corrupt => error.Corrupt,
            else => error.Io,
        };
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
        const set = self.set orelse return;
        for (set.regions.items) |r| {
            var held = try self.heldOf(r.appenderId(), null, 0, .{});
            defer held.deinit(self.gpa);
            const named = r.core.page.slotEntries();
            var will_name = self.gpa.alloc(u16, named.len) catch return error.OutOfMemory;
            defer self.gpa.free(will_name);
            for (named, 0..) |e, i| will_name[i] = e.slot;
            // Every unnamed root THIS cycle (never a bounded subset: a root
            // left for the next cycle is a root the tail may pass).
            const pubs = self.gpa.alloc(u16, held.items.len) catch return error.OutOfMemory;
            defer self.gpa.free(pubs);
            for (page_budget.tree0Publications(held.items, will_name, pubs)) |s| try self.publishTree0(s);
            // The pre-barrier refresh of the already-named slots.
            if (page_budget.cursorRefresh(self.gpa, r.core.page.slotEntries(), held.items) catch return error.OutOfMemory) |refreshed| {
                defer self.gpa.free(refreshed);
                r.core.page.setSlots(refreshed) catch return error.Corrupt;
                set.writePages(r, true) catch |e| return switch (e) {
                    error.OutOfMemory => error.OutOfMemory,
                    error.Corrupt => error.Corrupt,
                    error.Io => error.Io,
                };
                for (refreshed) |e| try self.setPublicationH(e.slot, .{ .root = .{ .addr = e.root_addr, .seq = e.root_seq }, .cursor = e.cursor, .home = .page });
            }
        }
    }

    /// C11: the full page plan for region `r` (the entries set on
    /// `r.core.page`; the caller writes the page). Publish-before-drop
    /// into tree 0 runs here for the cut.
    pub fn planPage(self: *Plane, r: *bind.Region) handover.Error!void {
        if (!self.armed) return;
        var held = try self.heldOf(r.appenderId(), null, 0, .{});
        defer held.deinit(self.gpa);
        var plan = page_budget.plan(self.gpa, held.items, page.SLOT_PAGE_BUDGET) catch return error.OutOfMemory;
        defer plan.deinit(self.gpa);
        for (plan.demote) |s| try self.publishTree0(s);
        r.core.page.setSlots(plan.entries) catch return error.Corrupt;
    }

    /// The page landed (C11): every named slot is page-homed at the words
    /// the image carried.
    pub fn pageLanded(self: *Plane, r: *bind.Region) void {
        if (!self.armed) return;
        for (r.core.page.slotEntries()) |e| self.setPublication(e.slot, .{ .root = .{ .addr = e.root_addr, .seq = e.root_seq }, .cursor = e.cursor, .home = .page }) catch {};
    }

    // ---- the mint policy (the set's `allocateIno`) ----

    /// The slot a child of `parent_slot` mints into on this volume: the
    /// gather arm first, then `mint_choice` over the rotor; null = the
    /// caller's rotor (an unarmed plane).
    pub fn mintSlot(self: *Plane, parent_slot: u16, parent_on_this_volume: bool, gather: bool, rr: u64) ?u16 {
        if (!self.armed) return null;
        const be = self.be;
        const native = be.stamp.native_slot == parent_slot;
        if (acquire.gatherMintSlot(parent_on_this_volume, native, gather, parent_slot)) |s| return s;
        var rotor: [lt.LeaseTable.PICK_MAX]acquire.RotorTree = undefined;
        var n: usize = 0;
        var held: std.ArrayList(u16) = .empty;
        defer held.deinit(self.gpa);
        self.table.heldBy(journal.FIXED_RING_APPENDER, &held) catch return null;
        for (held.items) |s| {
            const v = self.table.view(s);
            if (!v.rotor or v.state == .releasing or n == rotor.len) continue;
            rotor[n] = .{ .slot = s, .bytes = self.treeBytes(s) };
            n += 1;
        }
        const pv = self.table.view(parent_slot);
        // `A_max(t)` is DYNAMIC: the live leaf census at every decision
        // (the static knob, when set, is the whole law).
        const a_max = if (self.affinity_mib) |_| self.verbs.a_max else acquire.affinityCeiling(self.usedLeafBytes(), be.sb.node_size);
        const in: acquire.MintInputs = .{
            .parent_slot = parent_slot,
            .parent_leased_non_native = parent_on_this_volume and !native and pv.leasedBy(journal.FIXED_RING_APPENDER) and pv.state != .releasing,
            .parent_tree_bytes = self.treeBytes(parent_slot),
            .a_max = a_max,
            .node_size = be.sb.node_size,
            .rotor = rotor[0..n],
            .rotor_cap = acquire.rotorCap(self.verbs.m),
            .rr = rr,
        };
        const c = acquire.mintChoice(in);
        acquire.countChoice(c, in.parent_leased_non_native);
        return switch (c) {
            .overflow => blk: {
                // One more rotor slot (PARK admission), else the smallest.
                const out = self.verbs.acquireSlots(self.gpa, .{ .appender_id = journal.FIXED_RING_APPENDER, .want = 1, .admit = .park }) catch break :blk smallestOf(rotor[0..n]);
                switch (out) {
                    .granted => |g| {
                        defer if (g.grants.len != 0) self.gpa.free(g.grants);
                        if (g.grants.len == 0) break :blk smallestOf(rotor[0..n]);
                        acquire.stats.affinity_ceiling_overflows.inc();
                        break :blk g.grants[0].slot;
                    },
                    .refused => break :blk smallestOf(rotor[0..n]),
                }
            },
            .none => null,
            else => c.slot(),
        };
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
        const set = self.set orelse return;
        const m = acquire.resolveMintSlots(knobs.optIntKnob("SQUEEZEFS_SYM_MINT_SLOTS", u64, be.env), set.manager.appenders_known);
        self.verbs.setM(m);
        self.verbs.setCeiling(acquire.resolveAffinityCeiling(self.affinity_mib, self.usedLeafBytes(), be.sb.node_size, be.sb.heap.len) catch self.verbs.a_max);
        for (set.regions.items) |r| {
            const id = r.appenderId();
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

    /// The handover's own sequence to nobody; `Busy`/deferred = not this tick.
    fn releaseForCadence(self: *Plane, holder: AppenderId, slot: u16) bool {
        self.verbs.handover_lock.lockUncancelable(self.io);
        defer self.verbs.handover_lock.unlock(self.io);
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
        self.table.heldBy(journal.FIXED_RING_APPENDER, &held) catch return;
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
        try w.print("{{\"armed\":{},\"leases_held\":{d},\"rotor_m\":{d},\"a_max_bytes\":{d},\"n_floor\":{d},\"closed_slot\":{?d}}}", .{ self.armed, self.table.leasedCount(), if (self.armed) self.verbs.m else 0, if (self.armed) self.verbs.a_max else 0, self.judge.nFloorNow(), self.door.closedSlot() });
    }
};
