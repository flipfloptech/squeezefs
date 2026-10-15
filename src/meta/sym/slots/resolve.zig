//! SLOT → HOLDER RESOLUTION (charter §5.5 "Slot → holder resolution: tree
//! 0's lessee + the appender→endpoint table through a per-mount
//! `SlotHolderCache`; `ResolveSlot` is the fallback; `NotHolder{current}`
//! redirects are followed once; a redirect to a lessee the membership
//! owner lists DEAD is `HolderDead` (never dialled)"; `docs/porting/
//! sym_slot_leases.md` §6): the client side every later wave (tokens,
//! intents, record ships) resolves through. PORTABLE.
//!
//! The cache is a VIEW of the lease table (spec §9 Q2: one source of the
//! `(holder, g)` truth, read latch-free) plus the ONE extra map — appender
//! id → endpoint, fed by the manager's `PublishEndpoint` table and the
//! join ladder, invalidated by a `g` change (`learn` ignores a lower `g`),
//! a release (`forget`) and a redirect's answer learnt into it. A mount
//! whose view is warm never issues a `ResolveSlot`.

const std = @import("std");
const clock = @import("../../../core/clock.zig");
const Allocator = std.mem.Allocator;
const counters = @import("../../../core/counters.zig");
const errors = @import("../../../core/errors.zig");
const verbs = @import("../../../wire/verbs.zig");
const manager = @import("../manager.zig");
const jc = @import("../../journal_core.zig");
const lt = @import("lease_table.zig");

pub const AppenderId = lt.AppenderId;
pub const Endpoint = manager.Endpoint;

pub const stats = struct {
    /// A reader followed a `NotHolder{current}` redirect once.
    pub var slot_resolve_redirects_followed: counters.ShardedCounter = .{};
    /// A redirect named a lessee the membership owner lists dead: refused
    /// retryable, never dialled.
    pub var slot_resolve_dead_redirects: counters.ShardedCounter = .{};
    /// A "known unleased" hint refused for memory: the slot reads as
    /// unknown, so the resolver asks the manager once more.
    pub var slot_resolve_hint_drops: counters.ShardedCounter = .{};
    /// `ResolveSlot` issued by this mount (a cold view).
    pub var slot_resolve_calls: counters.ShardedCounter = .{};
};

pub const SlotHolder = struct { appender_id: AppenderId, g: u32 };

/// Where an op's step runs (`step_home`).
pub const StepHome = union(enum) {
    local,
    foreign: struct { holder: AppenderId, g: u32, endpoint: Endpoint },
    /// The holder has no published endpoint (or is dead): EAGAIN-class,
    /// the intent stays open, the roll-forward cadence retries.
    unreachable_: struct { holder: AppenderId },
};

/// The dial seam: ONE wire call to the volume's manager.
pub const Call = struct {
    ctx: *anyopaque,
    call: *const fn (ctx: *anyopaque, req: verbs.Request) errors.Error!verbs.Reply,
};

/// The membership owner's word on a holder (dead = never dialled).
pub const Liveness = struct {
    ctx: *anyopaque,
    isDead: *const fn (ctx: *anyopaque, appender_id: AppenderId) bool,
};

/// The appender → endpoint table's authority behind the cache's map: the
/// manager's RAM table (`PublishEndpoint`'s home) in the manager's
/// process, `ResolveEndpoint` over the wire on a joiner. Consulted on a
/// map miss, the answer learnt.
pub const EndpointSource = struct {
    ctx: *anyopaque,
    lookup: *const fn (ctx: *anyopaque, id: AppenderId) ?Endpoint,
    /// Learn the answer into the map (a wire source); false = the source
    /// is a RAM table consulted on every miss (a moved listener's new row
    /// is seen at once).
    learn: bool,
};

pub const SlotHolderCache = struct {
    gpa: Allocator,
    table: *lt.LeaseTable,
    /// This process's regions: a holder among them is `local`.
    own: *const fn (ctx: *anyopaque, id: AppenderId) bool,
    own_ctx: *anyopaque,
    endpoints: std.AutoHashMapUnmanaged(AppenderId, Endpoint) = .empty,
    endpoint_source: ?EndpointSource = null,
    /// A view learnt from a redirect or a reply ahead of tree 0 (a grant
    /// reaches the ledger only at the next cycle); a LOWER `g` than the
    /// table's is a stale view and ignored.
    learnt: std.AutoHashMapUnmanaged(u16, SlotHolder) = .empty,
    /// Slots the manager answered UNLEASED to a joiner's `ResolveSlot`
    /// (the table knows nothing of them): remembered so the one ask per
    /// slot is not an ask per op; a grant seen later (`learn`) or a
    /// redirect supersedes it, a release notice re-marks it.
    known_unleased: std.AutoHashMapUnmanaged(u16, void) = .empty,
    lock: jc.SpinLock = .{},

    pub fn init(gpa: Allocator, table: *lt.LeaseTable, own_ctx: *anyopaque, own: *const fn (ctx: *anyopaque, id: AppenderId) bool) SlotHolderCache {
        return .{ .gpa = gpa, .table = table, .own = own, .own_ctx = own_ctx };
    }

    pub fn deinit(self: *SlotHolderCache) void {
        self.endpoints.deinit(self.gpa);
        self.learnt.deinit(self.gpa);
        self.known_unleased.deinit(self.gpa);
    }

    /// Does this cache know anything of `slot` beyond its table — a learnt
    /// holder or a manager's "unleased"? A joiner's resolver asks the
    /// manager ONCE for a slot it knows nothing of.
    pub fn knows(self: *SlotHolderCache, slot: u16) bool {
        self.lock.lock();
        defer self.lock.unlock();
        return self.learnt.contains(slot) or self.known_unleased.contains(slot);
    }

    /// The holder of `slot` as this mount sees it: the table's word, or a
    /// learnt view at a higher `g`; null = unleased.
    pub fn holderOf(self: *SlotHolderCache, slot: u16) ?SlotHolder {
        const v = self.table.view(slot);
        var out: ?SlotHolder = if (v.state == .unleased or v.state == .migrating) null else .{ .appender_id = v.holder, .g = v.g };
        // A handover's released slot is its accepter's while the stand
        // holds (only the manager's table keeps stands): every touch goes
        // where the accepter's first touch takes it.
        if (v.state == .unleased) if (self.table.reservedFor(slot, clock.monotonicNs())) |to| {
            out = .{ .appender_id = to, .g = v.g };
        };
        self.lock.lock();
        defer self.lock.unlock();
        if (self.learnt.get(slot)) |l| {
            if (out == null or l.g > out.?.g) out = l;
        }
        return out;
    }

    /// A grant observed (a reply, a redirect, tree 0's refresh): kept iff
    /// not below the known `g`.
    pub fn learn(self: *SlotHolderCache, slot: u16, h: SlotHolder) Allocator.Error!void {
        defer self.table.noteMoved();
        self.lock.lock();
        defer self.lock.unlock();
        if (self.learnt.get(slot)) |l| if (l.g > h.g) return;
        if (self.table.gOf(slot) > h.g) return;
        _ = self.known_unleased.remove(slot);
        try self.learnt.put(self.gpa, slot, h);
    }

    /// A release observed (or the manager's "unleased"): the view drops
    /// (the table is the truth again) and the slot is known unleased.
    pub fn forget(self: *SlotHolderCache, slot: u16) void {
        defer self.table.noteMoved();
        self.lock.lock();
        defer self.lock.unlock();
        _ = self.learnt.remove(slot);
        self.known_unleased.put(self.gpa, slot, {}) catch stats.slot_resolve_hint_drops.inc();
    }

    pub fn setEndpoint(self: *SlotHolderCache, id: AppenderId, ep: Endpoint) Allocator.Error!void {
        self.lock.lock();
        defer self.lock.unlock();
        try self.endpoints.put(self.gpa, id, ep);
    }

    pub fn forgetEndpoint(self: *SlotHolderCache, id: AppenderId) void {
        self.lock.lock();
        defer self.lock.unlock();
        _ = self.endpoints.remove(id);
    }

    pub fn endpointOf(self: *SlotHolderCache, id: AppenderId) ?Endpoint {
        {
            self.lock.lock();
            defer self.lock.unlock();
            if (self.endpoints.get(id)) |ep| return ep;
        }
        // A miss: the table's authority, learnt (never under the spin
        // lock — the wire source blocks).
        const src = self.endpoint_source orelse return null;
        const ep = src.lookup(src.ctx, id) orelse return null;
        if (src.learn) self.setEndpoint(id, ep) catch {};
        return ep;
    }

    /// `step_home` (spec §6.2): the LEASE TABLE decides (a declared
    /// region's slot is another appender's for every cross-owner decision,
    /// whatever the gate says); the endpoint from the map; none ⇒
    /// unreachable; unleased ⇒ local (the first touch is the committer's).
    pub fn stepHome(self: *SlotHolderCache, slot: u16) StepHome {
        const h = self.holderOf(slot) orelse return .local;
        if (self.own(self.own_ctx, h.appender_id)) return .local;
        const ep = self.endpointOf(h.appender_id) orelse return .{ .unreachable_ = .{ .holder = h.appender_id } };
        return .{ .foreign = .{ .holder = h.appender_id, .g = h.g, .endpoint = ep } };
    }

    /// Does any of `slots` belong to another appender (the arms' "this op
    /// is cross-owner" predicate)?
    pub fn spansForeign(self: *SlotHolderCache, slots: []const u16) bool {
        for (slots) |s| if (self.stepHome(s) != .local) return true;
        return false;
    }

    /// The fallback: ONE `ResolveSlot` to the manager, the answer learnt.
    pub fn resolveViaManager(self: *SlotHolderCache, call: Call, volume: u16, slot: u16) errors.Error!?SlotHolder {
        stats.slot_resolve_calls.inc();
        const rep = try call.call(call.ctx, .{ .resolve_slot = .{ .volume = volume, .slot = slot, .reserved = 0 } });
        const r = rep.resolve_slot;
        if (r.kind == @backingInt(verbs.SlotHolderKind.unleased)) {
            self.forget(slot);
            // The table's own projection of a foreign holder (a refused
            // first touch loads it) goes too, else `holderOf` keeps
            // answering a holder the manager has released — a dead one
            // forever.
            const v = self.table.view(slot);
            if (v.state != .unleased and v.state != .migrating and !self.own(self.own_ctx, v.holder)) self.table.dropForeign(slot, v.holder, r.g);
            return null;
        }
        const h: SlotHolder = .{ .appender_id = r.appender_id, .g = r.g };
        try self.learn(slot, h);
        return h;
    }

    pub const Redirect = union(enum) {
        /// Dial this holder (the one follow).
        follow: struct { holder: AppenderId, endpoint: Endpoint },
        /// The holder named is dead: refused retryable, never dialled.
        dead,
        /// No endpoint for the holder named.
        unreachable_,
    };

    /// `NotHolder{current}` from a holder or the manager for an object
    /// whose slot it does not lease: followed ONCE at the holder named,
    /// its word learnt into the view; a holder the membership owner lists
    /// DEAD is `HolderDead` — the storm leg's reader parked 24 minutes
    /// behind a dead holder's address before this law.
    pub fn followRedirect(self: *SlotHolderCache, slot: u16, current: SlotHolder, liveness: ?Liveness) Allocator.Error!Redirect {
        if (liveness) |l| if (l.isDead(l.ctx, current.appender_id)) {
            stats.slot_resolve_dead_redirects.inc();
            return .dead;
        };
        try self.learn(slot, current);
        const ep = self.endpointOf(current.appender_id) orelse return .unreachable_;
        stats.slot_resolve_redirects_followed.inc();
        return .{ .follow = .{ .holder = current.appender_id, .endpoint = ep } };
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

fn ownsOne(_: *anyopaque, id: AppenderId) bool {
    return id == 1;
}

fn endpointOfByte(byte: u8) Endpoint {
    var e: Endpoint = .{};
    e.bytes[0] = byte;
    e.len = 1;
    return e;
}

test "the cache is a view: the table's word answers; learn keeps only a g not below the known one and a release forgets it; step_home is local for own and unleased slots, foreign with the endpoint, unreachable without one; a redirect is followed once, learnt, and a dead holder is never dialled" {
    const gpa = testing.allocator;
    const t = try lt.LeaseTable.init(gpa);
    defer t.deinit();
    var dummy: u8 = 0;
    var c = SlotHolderCache.init(gpa, t, &dummy, ownsOne);
    defer c.deinit();
    try testing.expectEqual(@as(?SlotHolder, null), c.holderOf(5));
    try testing.expectEqual(StepHome.local, c.stepHome(5));
    _ = try t.acquire(5, 2, false);
    try testing.expectEqual(@as(u32, 2), c.holderOf(5).?.appender_id);
    try testing.expectEqual(StepHome{ .unreachable_ = .{ .holder = 2 } }, c.stepHome(5));
    try c.setEndpoint(2, endpointOfByte(0xAB));
    try testing.expectEqual(@as(u32, 2), c.stepHome(5).foreign.holder);
    try testing.expect(c.spansForeign(&.{ 4, 5 }));
    try testing.expect(!c.spansForeign(&.{4}));
    // A learnt view ahead of the table (g 2 > 1); a lower g is ignored.
    try c.learn(5, .{ .appender_id = 3, .g = 2 });
    try testing.expectEqual(@as(u32, 3), c.holderOf(5).?.appender_id);
    try c.learn(5, .{ .appender_id = 9, .g = 1 });
    try testing.expectEqual(@as(u32, 3), c.holderOf(5).?.appender_id);
    c.forget(5);
    try testing.expectEqual(@as(u32, 2), c.holderOf(5).?.appender_id);
    // Own holder ⇒ local.
    _ = try t.acquire(6, 1, false);
    try testing.expectEqual(StepHome.local, c.stepHome(6));
    // The redirect: followed once with the endpoint; dead ⇒ never dialled.
    const Dead = struct {
        fn isDead(_: *anyopaque, id: AppenderId) bool {
            return id == 7;
        }
    };
    const f0 = stats.slot_resolve_redirects_followed.load();
    const r = try c.followRedirect(5, .{ .appender_id = 2, .g = 1 }, .{ .ctx = &dummy, .isDead = Dead.isDead });
    try testing.expectEqual(@as(u32, 2), r.follow.holder);
    try testing.expectEqual(f0 + 1, stats.slot_resolve_redirects_followed.load());
    const d0 = stats.slot_resolve_dead_redirects.load();
    try testing.expectEqual(SlotHolderCache.Redirect.dead, try c.followRedirect(5, .{ .appender_id = 7, .g = 9 }, .{ .ctx = &dummy, .isDead = Dead.isDead }));
    try testing.expectEqual(d0 + 1, stats.slot_resolve_dead_redirects.load());
    try testing.expectEqual(SlotHolderCache.Redirect.unreachable_, try c.followRedirect(5, .{ .appender_id = 8, .g = 9 }, null));
    c.forgetEndpoint(2);
    try testing.expectEqual(@as(?Endpoint, null), c.endpointOf(2));
}

test "ResolveSlot fallback: the manager's word is learnt (holder) or forgotten (unleased); counted on the caller" {
    const gpa = testing.allocator;
    const t = try lt.LeaseTable.init(gpa);
    defer t.deinit();
    var dummy: u8 = 0;
    var c = SlotHolderCache.init(gpa, t, &dummy, ownsOne);
    defer c.deinit();
    const Mgr = struct {
        var answer: verbs.SlotHolderReply = .{ .kind = 0, .reserved = @splat(0), .appender_id = 4, .g = 1, .reserved2 = 0 };
        fn call(_: *anyopaque, req: verbs.Request) errors.Error!verbs.Reply {
            if (req.resolve_slot.slot != 9) return error.Rejected;
            return .{ .resolve_slot = answer };
        }
    };
    const n0 = stats.slot_resolve_calls.load();
    const h = (try c.resolveViaManager(.{ .ctx = &dummy, .call = Mgr.call }, 0, 9)).?;
    try testing.expectEqual(@as(u32, 4), h.appender_id);
    try testing.expectEqual(@as(u32, 4), c.holderOf(9).?.appender_id);
    try testing.expectEqual(n0 + 1, stats.slot_resolve_calls.load());
    Mgr.answer = .{ .kind = 1, .reserved = @splat(0), .appender_id = 0, .g = 1, .reserved2 = 0 };
    try testing.expectEqual(@as(?SlotHolder, null), try c.resolveViaManager(.{ .ctx = &dummy, .call = Mgr.call }, 0, 9));
    try testing.expectEqual(@as(?SlotHolder, null), c.holderOf(9));
}

test "a joiner's knowledge of a slot beyond its table (found by the shared hot directory row: a third appender's slot read as the manager's on a joiner — every step bounced `not_holder` and the striping census saw one creator for all): unknown until the manager answered once; a holder learnt or an `unleased` answer makes it known; a later grant supersedes the unleased mark, a release re-marks it" {
    const gpa = testing.allocator;
    const t = try lt.LeaseTable.init(gpa);
    defer t.deinit();
    var dummy: u8 = 0;
    var c = SlotHolderCache.init(gpa, t, &dummy, ownsOne);
    defer c.deinit();
    const Mgr = struct {
        var answer: verbs.SlotHolderReply = .{ .kind = 1, .reserved = @splat(0), .appender_id = 0, .g = 1, .reserved2 = 0 };
        fn call(_: *anyopaque, _: verbs.Request) errors.Error!verbs.Reply {
            return .{ .resolve_slot = answer };
        }
    };
    try testing.expect(!c.knows(7));
    // The manager says unleased: remembered (the next op asks nobody).
    try testing.expectEqual(@as(?SlotHolder, null), try c.resolveViaManager(.{ .ctx = &dummy, .call = Mgr.call }, 0, 7));
    try testing.expect(c.knows(7));
    try testing.expectEqual(StepHome.local, c.stepHome(7));
    // A third appender leases it since: the grant learnt supersedes.
    try c.setEndpoint(3, endpointOfByte(3));
    try c.learn(7, .{ .appender_id = 3, .g = 2 });
    try testing.expect(c.knows(7));
    try testing.expectEqual(@as(u32, 3), c.stepHome(7).foreign.holder);
    // A release observed: unleased again, and still known.
    c.forget(7);
    try testing.expect(c.knows(7));
    try testing.expectEqual(StepHome.local, c.stepHome(7));
    // A slot the manager names a holder for on the first ask.
    Mgr.answer = .{ .kind = 0, .reserved = @splat(0), .appender_id = 3, .g = 4, .reserved2 = 0 };
    try testing.expect(!c.knows(8));
    try testing.expectEqual(@as(u32, 3), (try c.resolveViaManager(.{ .ctx = &dummy, .call = Mgr.call }, 0, 8)).?.appender_id);
    try testing.expect(c.knows(8));
}

test "a joiner's table drops a FOREIGN lease the manager answers unleased (found by the shared hot directory row: a refused first touch had loaded the dead holder B into C's table, and after the manager recovered B's slot every read of the striped directory at C resolved to the corpse — the answer dropped only the learnt view): the table's word goes unleased at the manager's generation; this process's own lease and a newer generation are left" {
    const gpa = testing.allocator;
    const t = try lt.LeaseTable.init(gpa);
    defer t.deinit();
    var dummy: u8 = 0;
    var c = SlotHolderCache.init(gpa, t, &dummy, ownsOne);
    defer c.deinit();
    try c.setEndpoint(3, endpointOfByte(3));
    const Mgr = struct {
        var g: u32 = 3;
        fn call(_: *anyopaque, _: verbs.Request) errors.Error!verbs.Reply {
            return .{ .resolve_slot = .{ .kind = @backingInt(verbs.SlotHolderKind.unleased), .reserved = @splat(0), .appender_id = 0, .g = g, .reserved2 = 0 } };
        }
    };
    // A refused acquire loaded B (3) as slot 7's holder at g 2.
    try t.load(7, .{ .variant = .leased, .appender_id = 3, .g = 2, .root = .{ .addr = 0, .seq = 0 }, .cursor = 2 });
    try testing.expectEqual(@as(u32, 3), c.stepHome(7).foreign.holder);
    try testing.expectEqual(@as(?SlotHolder, null), try c.resolveViaManager(.{ .ctx = &dummy, .call = Mgr.call }, 0, 7));
    try testing.expectEqual(StepHome.local, c.stepHome(7));
    try testing.expectEqual(@as(u32, 3), t.gOf(7));
    // This process's own lease is never dropped by the answer.
    try t.load(8, .{ .variant = .leased, .appender_id = 1, .g = 2, .root = .{ .addr = 0, .seq = 0 }, .cursor = 2 });
    _ = try c.resolveViaManager(.{ .ctx = &dummy, .call = Mgr.call }, 0, 8);
    try testing.expect(t.view(8).leasedBy(1));
    // An answer older than the table's word leaves it.
    try t.load(9, .{ .variant = .leased, .appender_id = 3, .g = 5, .root = .{ .addr = 0, .seq = 0 }, .cursor = 2 });
    _ = try c.resolveViaManager(.{ .ctx = &dummy, .call = Mgr.call }, 0, 9);
    try testing.expectEqual(@as(u32, 3), c.stepHome(9).foreign.holder);
}

test "a slot a handover released stays its ACCEPTER's for routing while the stand holds (found by the full gate's handover row: with the release reserved for B, A's own touch and every other appender's were refused, and B's chmods shipped to the manager met that refusal — EAGAIN): `stepHome` names the accepter until its grant lands; a lapsed stand routes as unleased again" {
    const gpa = testing.allocator;
    const t = try lt.LeaseTable.init(gpa);
    defer t.deinit();
    var dummy: u8 = 0;
    var c = SlotHolderCache.init(gpa, t, &dummy, ownsOne);
    defer c.deinit();
    try c.setEndpoint(5, endpointOfByte(5));
    // Appender 2 holds slot 4, offers it to 5, and releases it (the
    // flush-then-transfer); 5's grant has not landed yet.
    _ = try t.acquire(4, 2, false);
    _ = try t.offer(4, 2, 5, std.math.maxInt(u64));
    const g = t.beginRelease(4, 2, false).?;
    _ = try t.release(4, 2, g, .{}, 1);
    try testing.expectEqual(lt.State.unleased, t.view(4).state);
    try testing.expectEqual(@as(u32, 5), c.stepHome(4).foreign.holder);
    // The accepter's grant lands: still the accepter, now by its lease.
    _ = try t.acquire(4, 5, false);
    try testing.expectEqual(@as(u32, 5), c.stepHome(4).foreign.holder);
    // A stand that lapses: the slot routes as unleased (local first touch).
    _ = try t.acquire(6, 2, false);
    _ = try t.offer(6, 2, 5, 2);
    const g6 = t.beginRelease(6, 2, false).?;
    _ = try t.release(6, 2, g6, .{}, 1);
    try testing.expectEqual(StepHome.local, c.stepHome(6));
}
