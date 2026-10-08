//! The set-wide `dir_rename` LEASE and the exact ancestry walk (charter
//! §5.5 "every directory-source rename takes the set-wide `dir_rename`
//! lease at volume 0's manager"; `docs/porting/sym_tokens_intents_
//! striping.md` §2.10): ONE record in tree 0 of VOLUME 0 (`control_
//! records.DirRenameWire`, 24 B — the expiry law is the holder's
//! membership lease, never a TTL on the record); the verbs
//! `DirRenameLock → Locked{already} | Busy{holder}` ("the caller WAITS,
//! never a refusal — a set renames one directory at a time by design")
//! and `DirRenameUnlock` (the holder's record deleted; `already` when
//! free; the in-process parkers woken); the SCREEN (volume 0 only, a
//! non-own appender id, an unlock naming the HOLDER); ONE identity's
//! directory renames SERIALIZE under a one-permit semaphore so the law
//! never leans on the kernel's `s_vfs_rename_mutex`; a DEAD holder's
//! record is released by the recovery driver off the death ledger.
//!
//! The ancestor check on EXACT data: under the lease, walk the new
//! parent's chain to the root refusing EINVAL if the moved directory is
//! on it; each hop's link `(p, name) → cur` comes from the parent MEMO
//! (O(1)) and is CONFIRMED at `p`'s slot holder through a GUARD-FREE exact
//! lookup (a shared guard here could park behind the initiator's own
//! exclusive one on a stripe collision); a denied/absent link re-reads ≤
//! 3 times (EAGAIN past the bound, never a guess); > 4096 hops = a cycle
//! already exists (ELOOP). PORTABLE.

const std = @import("std");
const Allocator = std.mem.Allocator;
const Io = std.Io;
const counters = @import("../../../core/counters.zig");
const errors = @import("../../../core/errors.zig");
const clock = @import("../../../core/clock.zig");
const sync = @import("../../../sync/sync.zig");
const verbs = @import("../../../wire/verbs.zig");
const service = @import("../../../wire/service.zig");
const screen = @import("../../../wire/screen.zig");
const cr = @import("../control_records.zig");
const inos = @import("../../inos.zig");
const token_cache = @import("../tokens/token_cache.zig");

pub const Link = token_cache.Link;

pub const stats = struct {
    /// Leases taken (every directory-source rename; never a file's).
    pub var dir_rename_lock_acquires: counters.ShardedCounter = .{};
    /// The wait for the lease (the uncontended take IS one ring-0 control
    /// entry + barrier).
    pub var dir_rename_lock_wait_ns: counters.ShardedLatencyHistogram = .{};
    /// A memo miss on the ancestry walk answered by the reverse scan seam.
    pub var dir_rename_parent_scans: counters.ShardedCounter = .{};
    /// A rename refused: the destination lies inside the moved directory.
    pub var dir_rename_subtree_refusals: counters.ShardedCounter = .{};
    /// The walk met its hop bound: a cycle already exists (must stay 0).
    pub var dir_rename_cycles_found: counters.Tripwire = .init("dir_rename_cycles_found");
    /// A lock verb's word the screen refused (must stay 0).
    pub var dir_rename_verb_rejected: counters.Tripwire = .init("dir_rename_verb_rejected");
    /// A dead holder's record released by recovery.
    pub var dir_rename_dead_releases: counters.ShardedCounter = .{};
};

/// Deeper than any legal tree: a chain this long is a cycle already.
pub const MAX_HOPS: u32 = 4096;
/// A taker WAITS on `Busy` (never a refusal) — bounded by the membership
/// TTL: a holder dead longer than that has its record released by
/// recovery, so a wait past it names a wedge, answered EAGAIN.
pub const TAKE_WAIT_NS: u64 = @import("../tokens/token_table.zig").T_OWNER_NS;
/// A denied/absent link re-reads this many times (a stale memo hint costs
/// no wait; past it EAGAIN, never a guess).
pub const RESCAN_BOUND: u32 = 3;

// ---------------------------------------------------------------------------
// The lease record at volume 0's manager.
// ---------------------------------------------------------------------------

/// Tree 0 of volume 0 behind a seam: the lookup and ONE barriered control
/// entry (the manager's `Volume` in the product; a RAM map in the
/// contracts).
pub const ControlStore = struct {
    ctx: *anyopaque,
    lookup: *const fn (ctx: *anyopaque, k: []const u8) error{Io}!?cr.DirRename,
    put: *const fn (ctx: *anyopaque, k: []const u8, v: []const u8) error{ Io, Deferred }!void,
    delete: *const fn (ctx: *anyopaque, k: []const u8) error{ Io, Deferred }!void,
};

pub const LockOutcome = union(enum) {
    locked: struct { already: bool },
    busy: struct { holder: u32 },
};

pub const LeaseError = error{ Io, Deferred, Rejected };

/// The manager side: the record and the in-process parkers' wake.
pub const Lease = struct {
    io: Io,
    store: ControlStore,
    /// The serving era (PROVENANCE on the record, never a check).
    term: u64,
    /// The manager's own appender id (a wire ask naming it is rejected).
    own_id: u32,
    released: sync.Notify = .init,

    fn k() []const u8 {
        return cr.dirRenameKey().slice();
    }

    pub fn holder(self: *Lease) error{Io}!?cr.DirRename {
        return self.store.lookup(self.store.ctx, k());
    }

    /// `DirRenameLock{appender}`: `Locked{already}` when the caller holds
    /// it, `Busy{holder}` otherwise, else ONE control entry.
    pub fn lock(self: *Lease, appender: u32) LeaseError!LockOutcome {
        if (try self.holder()) |h| {
            return if (h.holder == appender) .{ .locked = .{ .already = true } } else .{ .busy = .{ .holder = h.holder } };
        }
        const w = cr.DirRenameWire.init(.{ .holder = appender, .term = self.term, .since_realtime_ns = @bitCast(clock.realtimeCoarseNs()) });
        try self.store.put(self.store.ctx, k(), &w.encode());
        stats.dir_rename_lock_acquires.inc();
        return .{ .locked = .{ .already = false } };
    }

    /// `DirRenameUnlock{appender}`: the holder's record deleted (`already`
    /// when free); an unlock by a non-holder is rejected.
    pub fn unlock(self: *Lease, appender: u32) LeaseError!bool {
        const h = (try self.holder()) orelse return true;
        if (h.holder != appender) return error.Rejected;
        try self.store.delete(self.store.ctx, k());
        self.released.notifyWaiters(self.io);
        return false;
    }

    /// The recovery driver's release of a DEAD holder's record.
    pub fn releaseDead(self: *Lease, dead_holder: u32) LeaseError!bool {
        const h = (try self.holder()) orelse return false;
        if (h.holder != dead_holder) return false;
        try self.store.delete(self.store.ctx, k());
        stats.dir_rename_dead_releases.inc();
        self.released.notifyWaiters(self.io);
        return true;
    }
};

/// The wire face (volume 0's manager).
pub const LeaseService = struct {
    lease: *Lease,

    pub fn register(self: *LeaseService, svc: *service.Service) void {
        svc.register(.dir_rename_lock, .{ .ctx = self, .serve = &serveLock });
        svc.register(.dir_rename_unlock, .{ .ctx = self, .serve = &serveUnlock });
    }

    fn screenWords(self: *LeaseService, r: verbs.DirRenameReq) ?service.Answer {
        switch (screen.dirRename(r)) {
            .ok => {},
            .rejected => |why| {
                stats.dir_rename_verb_rejected.fire(@tagName(why));
                return service.Answer.rejected(@tagName(why));
            },
        }
        if (r.appender_id == self.lease.own_id) {
            stats.dir_rename_verb_rejected.fire("the manager's own appender id on the wire");
            return service.Answer.rejected("the manager's own appender id on the wire");
        }
        return null;
    }

    fn leaseAnswer(e: LeaseError) service.Answer {
        return switch (e) {
            error.Io => service.Answer.refused(error.InputOutput, "the dir_rename record could not be read or written"),
            error.Deferred => service.Answer.deferred("ring 0's window is full"),
            error.Rejected => blk: {
                stats.dir_rename_verb_rejected.fire("an unlock by a non-holder");
                break :blk service.Answer.rejected("an unlock by a non-holder");
            },
        };
    }

    fn serveLock(ctx: *anyopaque, req: *const service.Request, _: []u8) service.Answer {
        const self: *LeaseService = @ptrCast(@alignCast(ctx));
        const r = req.payload.dir_rename_lock;
        if (self.screenWords(r)) |a| return a;
        const out = self.lease.lock(r.appender_id) catch |e| return leaseAnswer(e);
        return .{ .ok = .{ .dir_rename_lock = switch (out) {
            .locked => |l| .{ .kind = @backingInt(verbs.DirRenameKind.locked), .already = @intFromBool(l.already), .reserved = 0, .holder = r.appender_id },
            .busy => |b| .{ .kind = @backingInt(verbs.DirRenameKind.busy), .already = 0, .reserved = 0, .holder = b.holder },
        } } };
    }

    fn serveUnlock(ctx: *anyopaque, req: *const service.Request, _: []u8) service.Answer {
        const self: *LeaseService = @ptrCast(@alignCast(ctx));
        const r = req.payload.dir_rename_unlock;
        if (self.screenWords(r)) |a| return a;
        const already = self.lease.unlock(r.appender_id) catch |e| return leaseAnswer(e);
        return .{ .ok = .{ .dir_rename_unlock = if (already) .replay else .fresh } };
    }
};

// ---------------------------------------------------------------------------
// The taker: an initiator's hold.
// ---------------------------------------------------------------------------

/// How this mount reaches volume 0's manager: in-process (the manager is
/// ours) or the wire link.
pub const Manager = union(enum) {
    local: *Lease,
    remote: Link,
};

pub const TakeError = error{ Io, Deferred, Rejected, HolderUnreachable };

/// One mount's taker: the per-identity one-permit semaphore + the ask.
pub const Taker = struct {
    io: Io,
    appender_id: u32,
    manager: Manager,
    /// ONE identity's directory renames serialize here.
    permits: sync.Semaphore = sync.Semaphore.init(1),
    /// Deterministic park observable: asks that met `Busy`.
    busy_waits: std.atomic.Value(u64) = .init(0),

    pub const Held = struct {
        taker: *Taker,
        permit: sync.Semaphore.Permit,

        pub fn release(self: *Held) void {
            self.taker.unlockAsk() catch {};
            self.permit.release(self.taker.io);
        }
    };

    /// Take the lease: the identity permit first, then `Lock` until
    /// `Locked` — a `Busy` answer WAITS (register-then-check on the
    /// in-process notify; the wire arm re-asks on the holder's wake, bounded
    /// by `deadline_ns`).
    pub fn take(self: *Taker, deadline_ns: u64) TakeError!Held {
        const t0 = clock.monotonicNs();
        const permit = self.permits.acquireUncancelable(self.io, 1);
        errdefer permit.release(self.io);
        while (true) {
            var n: ?sync.Notify.Notified = null;
            if (self.manager == .local) {
                n = self.manager.local.released.notified();
                _ = n.?.enable(self.io);
            }
            const out = self.lockAsk() catch |e| {
                if (n) |*nn| nn.cancel(self.io);
                return e;
            };
            switch (out) {
                .locked => {
                    if (n) |*nn| nn.cancel(self.io);
                    stats.dir_rename_lock_wait_ns.record(clock.monotonicNs() -| t0);
                    return .{ .taker = self, .permit = permit };
                },
                .busy => {
                    _ = self.busy_waits.fetchAdd(1, .monotonic);
                    if (n) |*nn| {
                        switch (nn.waitUntilUncancelable(self.io, deadline_ns)) {
                            .notified => {},
                            .deadline => return error.Deferred,
                        }
                    } else {
                        // The wire arm: the holder's release is observed by
                        // re-asking; the lease is uncontended by design, so
                        // the re-ask cadence is the landing ceiling's.
                        if (clock.monotonicNs() >= deadline_ns) return error.Deferred;
                        std.Thread.yield() catch {};
                    }
                },
            }
        }
    }

    fn lockAsk(self: *Taker) TakeError!LockOutcome {
        switch (self.manager) {
            .local => |l| return l.lock(self.appender_id) catch |e| switch (e) {
                error.Io => error.Io,
                error.Deferred => error.Deferred,
                error.Rejected => error.Rejected,
            },
            .remote => |link| {
                const rep = link.call(link.ctx, .{ .dir_rename_lock = .{ .volume = 0, .reserved = 0, .appender_id = self.appender_id } }) catch |e| return switch (e) {
                    error.Deferred => error.Deferred,
                    error.Rejected => error.Rejected,
                    error.ConnectionLost, error.HolderUnreachable, error.NotManager => error.HolderUnreachable,
                    else => error.Io,
                };
                const d = rep.dir_rename_lock;
                return if (d.kind == @backingInt(verbs.DirRenameKind.locked)) .{ .locked = .{ .already = d.already != 0 } } else .{ .busy = .{ .holder = d.holder } };
            },
        }
    }

    fn unlockAsk(self: *Taker) TakeError!void {
        switch (self.manager) {
            .local => |l| _ = l.unlock(self.appender_id) catch |e| return switch (e) {
                error.Io => error.Io,
                error.Deferred => error.Deferred,
                error.Rejected => error.Rejected,
            },
            .remote => |link| _ = link.call(link.ctx, .{ .dir_rename_unlock = .{ .volume = 0, .reserved = 0, .appender_id = self.appender_id } }) catch |e| return switch (e) {
                error.Deferred => error.Deferred,
                error.Rejected => error.Rejected,
                else => error.HolderUnreachable,
            },
        }
    }
};

// ---------------------------------------------------------------------------
// The exact ancestry walk.
// ---------------------------------------------------------------------------

/// A hop of the chain: `(parent, name) → child`.
pub const ParentLink = struct { parent: u64, name: []const u8 };

/// The walk's seams: the parent memo (O(1); a miss falls to the reverse
/// scan, counted) and the guard-free exact lookup at `parent`'s holder.
pub const Ancestry = struct {
    ctx: *anyopaque,
    /// The memo's link for `dir`, written into `name_buf`.
    linkOf: *const fn (ctx: *anyopaque, dir: u64, name_buf: *[255]u8) ?ParentLink,
    /// The reverse scan on a memo miss (the slow belt; null = unknown).
    scanParent: *const fn (ctx: *anyopaque, dir: u64, name_buf: *[255]u8) ?ParentLink,
    /// `LookupExact(parent, name)` at the parent's holder: the child ino.
    lookupExact: *const fn (ctx: *anyopaque, parent: u64, name: []const u8) errors.Error!?u64,
    /// A stripe on the chain continues to its directory (null = not a
    /// stripe).
    stripeOwner: *const fn (ctx: *anyopaque, dir: u64) ?u64,
};

pub const WalkError = error{
    /// The destination lies inside the moved directory (EINVAL).
    IntoOwnSubtree,
    /// The hop bound: a cycle already exists (ELOOP).
    Cycle,
    /// A link could not be confirmed inside the rescan bound (EAGAIN).
    Unconfirmed,
    Io,
};

pub fn walkError(e: WalkError) errors.Error {
    return switch (e) {
        error.IntoOwnSubtree => error.InvalidArgument,
        error.Cycle => error.TooManySymlinks,
        error.Unconfirmed => error.Deferred,
        error.Io => error.InputOutput,
    };
}

/// Refuse a rename of directory `moved` into `new_parent`'s subtree:
/// walk `new_parent` to the root; every hop's memo link confirmed exact.
pub fn refuseRenameIntoOwnSubtree(anc: Ancestry, moved: u64, new_parent: u64) WalkError!void {
    var cur = new_parent;
    var hops: u32 = 0;
    while (true) : (hops += 1) {
        if (cur == moved) {
            stats.dir_rename_subtree_refusals.inc();
            return error.IntoOwnSubtree;
        }
        if (cur == inos.ROOT_INO) return;
        if (hops > MAX_HOPS) {
            stats.dir_rename_cycles_found.fire("the ancestry walk exceeded 4096 hops");
            return error.Cycle;
        }
        if (anc.stripeOwner(anc.ctx, cur)) |owner| {
            cur = owner;
            continue;
        }
        var name_buf: [255]u8 = undefined;
        var rescans: u32 = 0;
        const next = while (true) : (rescans += 1) {
            if (rescans > RESCAN_BOUND) return error.Unconfirmed;
            const link = anc.linkOf(anc.ctx, cur, &name_buf) orelse blk: {
                stats.dir_rename_parent_scans.inc();
                break :blk anc.scanParent(anc.ctx, cur, &name_buf) orelse continue;
            };
            const confirmed = (anc.lookupExact(anc.ctx, link.parent, link.name) catch |e| switch (e) {
                error.Deferred, error.SlotMoved, error.StaleHolderView, error.HolderUnreachable => continue,
                else => return error.Io,
            }) orelse continue;
            if (confirmed != cur) continue;
            break link.parent;
        };
        cur = next;
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

/// Tree 0 of volume 0 in RAM.
const RamControl = struct {
    rec: ?cr.DirRename = null,
    puts: u32 = 0,
    defer_next: bool = false,

    fn lookup(ctx: *anyopaque, _: []const u8) error{Io}!?cr.DirRename {
        return @as(*RamControl, @ptrCast(@alignCast(ctx))).rec;
    }
    fn put(ctx: *anyopaque, _: []const u8, v: []const u8) error{ Io, Deferred }!void {
        const self: *RamControl = @ptrCast(@alignCast(ctx));
        if (self.defer_next) {
            self.defer_next = false;
            return error.Deferred;
        }
        self.rec = cr.DirRenameWire.decode(v) catch return error.Io;
        self.puts += 1;
    }
    fn delete(ctx: *anyopaque, _: []const u8) error{ Io, Deferred }!void {
        @as(*RamControl, @ptrCast(@alignCast(ctx))).rec = null;
    }
    fn store(self: *RamControl) ControlStore {
        return .{ .ctx = self, .lookup = &lookup, .put = &put, .delete = &delete };
    }
};

test "the lease: Lock → Locked (one control entry), the holder's re-ask is a replay, another identity reads Busy{holder}; Unlock by the holder frees it (a non-holder is rejected), `already` when free; a dead holder's record is released by recovery; a deferred entry writes nothing" {
    var ram: RamControl = .{};
    var lease: Lease = .{ .io = testing.io, .store = ram.store(), .term = 3, .own_id = 0 };
    const acq0 = stats.dir_rename_lock_acquires.load();
    try testing.expectEqual(LockOutcome{ .locked = .{ .already = false } }, try lease.lock(7));
    try testing.expectEqual(@as(u32, 1), ram.puts);
    try testing.expectEqual(@as(u64, 3), (try lease.holder()).?.term);
    try testing.expectEqual(LockOutcome{ .locked = .{ .already = true } }, try lease.lock(7));
    try testing.expectEqual(LockOutcome{ .busy = .{ .holder = 7 } }, try lease.lock(9));
    try testing.expectError(error.Rejected, lease.unlock(9));
    try testing.expectEqual(false, try lease.unlock(7));
    try testing.expectEqual(true, try lease.unlock(7));
    try testing.expectEqual(acq0 + 1, stats.dir_rename_lock_acquires.load());
    _ = try lease.lock(9);
    try testing.expect(!try lease.releaseDead(7));
    try testing.expect(try lease.releaseDead(9));
    try testing.expect((try lease.holder()) == null);
    ram.defer_next = true;
    try testing.expectError(error.Deferred, lease.lock(7));
    try testing.expect((try lease.holder()) == null);
}

const Parker = struct {
    taker: *Taker,
    held: ?Taker.Held = null,
    res: TakeError!void = {},
    fn run(self: *Parker) void {
        self.held = self.taker.take(std.math.maxInt(u64)) catch |e| {
            self.res = e;
            return;
        };
    }
};

test "two directory renames of two identities serialize under the lease: the loser WAITS on Busy (never a refusal) and proceeds at the holder's unlock; one identity's two renames serialize under its one permit; a file rename takes nothing (the caller's law — no take, no acquire)" {
    var ram: RamControl = .{};
    var lease: Lease = .{ .io = testing.io, .store = ram.store(), .term = 1, .own_id = 0 };
    var a: Taker = .{ .io = testing.io, .appender_id = 1, .manager = .{ .local = &lease } };
    var b: Taker = .{ .io = testing.io, .appender_id = 2, .manager = .{ .local = &lease } };
    var held_a = try a.take(std.math.maxInt(u64));
    try testing.expectEqual(@as(u32, 1), (try lease.holder()).?.holder);
    var pb: Parker = .{ .taker = &b };
    const tb = try std.Thread.spawn(.{}, Parker.run, .{&pb});
    var spins: u32 = 0;
    while (b.busy_waits.load(.monotonic) == 0) : (spins += 1) {
        try testing.expect(spins < 10_000_000);
        std.Thread.yield() catch {};
    }
    try testing.expect(pb.held == null);
    held_a.release();
    tb.join();
    try pb.res;
    try testing.expectEqual(@as(u32, 2), (try lease.holder()).?.holder);
    // One identity's second rename parks on the permit, not the record.
    var pb2: Parker = .{ .taker = &b };
    const tb2 = try std.Thread.spawn(.{}, Parker.run, .{&pb2});
    spins = 0;
    while (b.permits.waiters(testing.io) == 0) : (spins += 1) {
        try testing.expect(spins < 10_000_000);
        std.Thread.yield() catch {};
    }
    try testing.expectEqual(@as(u32, 2), (try lease.holder()).?.holder);
    pb.held.?.release();
    tb2.join();
    try pb2.res;
    pb2.held.?.release();
    try testing.expect((try lease.holder()) == null);
}

/// A RAM tree for the walk: `child → (parent, name)` plus the exact
/// dentries `(parent, name) → child`.
const RamTree = struct {
    links: [16]struct { child: u64, parent: u64, name: []const u8 } = undefined,
    n: usize = 0,
    /// A link the memo lacks (the scan answers it).
    memo_miss: u64 = 0,
    /// A link whose exact lookup denies it (the stale memo).
    deny: u64 = 0,
    stripe_of: ?struct { stripe: u64, dir: u64 } = null,
    scans: u32 = 0,
    exact_calls: u32 = 0,

    fn add(self: *RamTree, child: u64, parent: u64, name: []const u8) void {
        self.links[self.n] = .{ .child = child, .parent = parent, .name = name };
        self.n += 1;
    }
    fn find(self: *RamTree, dir: u64) ?ParentLink {
        for (self.links[0..self.n]) |l| if (l.child == dir) return .{ .parent = l.parent, .name = l.name };
        return null;
    }
    fn linkOf(ctx: *anyopaque, dir: u64, _: *[255]u8) ?ParentLink {
        const self: *RamTree = @ptrCast(@alignCast(ctx));
        if (dir == self.memo_miss) return null;
        return self.find(dir);
    }
    fn scanParent(ctx: *anyopaque, dir: u64, _: *[255]u8) ?ParentLink {
        const self: *RamTree = @ptrCast(@alignCast(ctx));
        self.scans += 1;
        return self.find(dir);
    }
    fn lookupExact(ctx: *anyopaque, parent: u64, name: []const u8) errors.Error!?u64 {
        const self: *RamTree = @ptrCast(@alignCast(ctx));
        self.exact_calls += 1;
        for (self.links[0..self.n]) |l| if (l.parent == parent and std.mem.eql(u8, l.name, name)) {
            if (l.child == self.deny) return null;
            return l.child;
        };
        return null;
    }
    fn stripeOwner(ctx: *anyopaque, dir: u64) ?u64 {
        const self: *RamTree = @ptrCast(@alignCast(ctx));
        if (self.stripe_of) |s| if (s.stripe == dir) return s.dir;
        return null;
    }
    fn anc(self: *RamTree) Ancestry {
        return .{ .ctx = self, .linkOf = &linkOf, .scanParent = &scanParent, .lookupExact = &lookupExact, .stripeOwner = &stripeOwner };
    }
};

test "the ancestry walk: a destination inside the moved directory refuses EINVAL on exact data; an unrelated chain passes with one exact lookup per hop; a memo miss takes the scan belt; a denied link past the rescan bound is EAGAIN never a guess; a stripe continues to its directory; a cyclic chain is ELOOP" {
    var t: RamTree = .{};
    // / (1) ─ a (10) ─ b (11) ─ c (12);  / ─ x (20)
    t.add(10, 1, "a");
    t.add(11, 10, "b");
    t.add(12, 11, "c");
    t.add(20, 1, "x");
    const anc = t.anc();
    const ref0 = stats.dir_rename_subtree_refusals.load();
    try testing.expectError(error.IntoOwnSubtree, refuseRenameIntoOwnSubtree(anc, 10, 12));
    try testing.expectEqual(ref0 + 1, stats.dir_rename_subtree_refusals.load());
    try testing.expectError(error.IntoOwnSubtree, refuseRenameIntoOwnSubtree(anc, 11, 11));
    t.exact_calls = 0;
    try refuseRenameIntoOwnSubtree(anc, 10, 20);
    try testing.expectEqual(@as(u32, 1), t.exact_calls);
    try refuseRenameIntoOwnSubtree(anc, 20, 12);
    try refuseRenameIntoOwnSubtree(anc, 20, 1);
    // The memo miss.
    t.memo_miss = 11;
    const scans0 = stats.dir_rename_parent_scans.load();
    try refuseRenameIntoOwnSubtree(anc, 20, 12);
    try testing.expectEqual(scans0 + 1, stats.dir_rename_parent_scans.load());
    t.memo_miss = 0;
    // A stale memo: the exact lookup denies the link every time.
    t.deny = 11;
    try testing.expectError(error.Unconfirmed, refuseRenameIntoOwnSubtree(anc, 20, 12));
    try testing.expectEqual(errors.Error.Deferred, walkError(error.Unconfirmed));
    t.deny = 0;
    // A stripe on the chain continues to its directory.
    t.stripe_of = .{ .stripe = 99, .dir = 12 };
    try testing.expectError(error.IntoOwnSubtree, refuseRenameIntoOwnSubtree(anc, 10, 99));
    // A cycle: 30 ↔ 31 (a corrupt tree) — ELOOP, the tripwire.
    t.add(30, 31, "p");
    t.add(31, 30, "q");
    stats.dir_rename_cycles_found.resetLogBudgetForTest();
    @import("../../../core/log.zig").testing.expectErrorLogs(1);
    try testing.expectError(error.Cycle, refuseRenameIntoOwnSubtree(anc, 10, 30));
    try @import("../../../core/log.zig").testing.expectAllFired();
    try testing.expectEqual(errors.Error.TooManySymlinks, walkError(error.Cycle));
}
