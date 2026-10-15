//! TRAVELLING 4a GUARDS (charter §2 lock order, §5.5 "Cross-owner
//! namespace ops"; `docs/porting/sym_tokens_intents_striping.md` §2.4):
//! two laws — (1) ONE table, one canonical `lockMany`: every key whose
//! lock lives in THIS process's table rides the one stripe-canonical
//! acquisition; a key in another holder's table is one `XvGuards` to that
//! holder, served by the same `lockMany` in ITS table and PARKED under the
//! op's scope until `XvRelease`; (2) tables in ASCENDING appender-id
//! order — "a waiter for table T holds only tables < T and the wait-for
//! graph over tables is acyclic". A served STEP under a scope that COVERS
//! its stripes applies with the parked guards (`Covered`); else it takes
//! the step's keys (`Take`) — coverage by `(class, stripe)` SET, never by
//! key (`dlm.Manager.covers`), so a scope that parked unrelated keys never
//! lets a step apply unguarded.
//!
//! The served acquisition NEVER PARKS (decision 0003 §Q): a served ask
//! parked on a stripe another parked scope of the SAME initiator holds
//! waits on that scope's release — when the listener served one frame at
//! a time per connection the release queued behind the parked serve
//! itself (found by the shared hot directory row: the holder's migration
//! and its own creates into one stripe wedged the session until the grace
//! belt); the session's lanes (§AM) bound that to one lane, and a parked
//! ask would still hold its initiator's op for nothing. The holder takes the
//! set in the TRY form and answers `Busy` on contention; the initiator
//! retries the ask on its own thread with a doubling backoff under the
//! wire's reply bound — the patience a parked ask had, spent where it
//! blocks nobody.
//!
//! The holder's lease-expiry sweep is the backstop: a scope is kept while
//! its client is a LIVE member, released the sweep after the owner
//! evicts it (`xv_cross_owner_guards_swept_dead` — a dead member's locks
//! at every holder go with its lease, charter §5.9: before it every op on
//! those stripes waited, and the session serving them with it); a client
//! the owner does not know keeps the grace window `T_owner` as the belt
//! (the tripwire). The verdict is the membership owner's: its roster on
//! the manager, its census on a joiner (`membership/census_view.zig`).
//! PORTABLE.

const std = @import("std");
const Allocator = std.mem.Allocator;
const Io = std.Io;
const counters = @import("../../../core/counters.zig");
const errors = @import("../../../core/errors.zig");
const clock = @import("../../../core/clock.zig");
const checksum = @import("../../../core/checksum.zig");
const sync = @import("../../../sync/sync.zig");
const verbs = @import("../../../wire/verbs.zig");
const service = @import("../../../wire/service.zig");
const codec = @import("../../../wire/codec.zig");
const dlm = @import("../../dlm.zig");
const lock_order = @import("../../../sync/lock_order.zig");
const token_cache = @import("../tokens/token_cache.zig");
const token_table = @import("../tokens/token_table.zig");

const assert = std.debug.assert;

pub const Link = token_cache.Link;
pub const Scope = u64;
pub const AppenderId = u32;

pub const stats = struct {
    /// `XvGuards` asks sent (one per foreign holder table per op).
    pub var xv_cross_owner_guard_rpcs: counters.ShardedCounter = .{};
    /// A holder answered `StaleHolderView`: every key's slot re-resolved.
    pub var xv_cross_owner_guard_stale_reresolves: counters.ShardedCounter = .{};
    /// Scopes parked at this holder (served `XvGuards`).
    pub var xv_cross_owner_guards_parked: counters.ShardedCounter = .{};
    /// A parked scope released by the sweep because the membership owner
    /// lists its client DEAD (evicted, or absent outside the grace window):
    /// the §5.9 path — a dead member's locks go with its lease.
    pub var xv_cross_owner_guards_swept_dead: counters.ShardedCounter = .{};
    /// A parked scope released by the grace belt alone — a client the
    /// owner does not know, aged past `T_owner`, never released by its
    /// client: must stay 0.
    pub var xv_cross_owner_guard_expiries: counters.Tripwire = .init("xv_cross_owner_guard_expiries");
    /// Served steps that applied under the scope's parked guards.
    pub var xv_cross_owner_steps_covered: counters.ShardedCounter = .{};
    /// Served steps that took their own guards (the scope did not cover).
    pub var xv_cross_owner_steps_took_guards: counters.ShardedCounter = .{};
    /// The round trip of a guard ask.
    pub var xv_cross_owner_guard_rtt_ns: counters.ShardedLatencyHistogram = .{};
    /// Served asks answered `Busy` (a stripe held in this table): the
    /// initiator retries, the session serves on.
    pub var xv_cross_owner_guards_contended: counters.ShardedCounter = .{};
    /// Asks the initiator retried after a `Busy`.
    pub var xv_cross_owner_guard_retries: counters.ShardedCounter = .{};
    /// Asks given up after the contention bound (`GuardRefused`).
    pub var xv_cross_owner_guard_contention_refusals: counters.ShardedCounter = .{};
};

/// The initiator's backoff after a `Busy` ask: the first retry one
/// millisecond on (a stripe is held for a plan's local steps — tens of
/// microseconds to a few milliseconds), doubling to the cap (a holder's
/// commit under device pressure), under the wire's reply bound — the
/// patience a parked ask had before the holder stopped parking.
pub const GUARD_RETRY_BACKOFF_MIN_MS: u64 = 1;
pub const GUARD_RETRY_BACKOFF_MAX_MS: u64 = 16;
pub const GUARD_CONTENTION_BOUND_MS: u64 = @import("../../../wire/session.zig").DIAL_TIMEOUT_MS;

/// `mint_guard_scope = seq & 2^40−1 | 2^40`: nonzero by construction (0 on
/// the wire = "no scope travelled").
pub fn mintScope(seq: u64) Scope {
    return (seq & ((1 << 40) - 1)) | (1 << 40);
}

/// `stuck_grace = T_owner`: a client the owner does not know keeps the
/// grace window as the belt.
pub const SCOPE_GRACE_NS: u64 = token_table.T_OWNER_NS;

/// A holder re-resolves on a stale view at most this many times: one
/// legal move + one re-move; a third stale answer is the retryable class.
pub const STALE_HOLDER_RETRIES: u32 = 2;

pub const Error = error{ OutOfMemory, PlanTooLarge, GuardRefused, HolderUnreachable, StaleHolderView };

/// The served acquisition's refusal: a stripe of the ask is held in this
/// table (`Busy` on the wire; the initiator retries).
pub const ServeError = error{ OutOfMemory, PlanTooLarge, Contended };

// ---------------------------------------------------------------------------
// The holder's parked scopes.
// ---------------------------------------------------------------------------

const Parked = struct {
    client: u64,
    set: dlm.WideGuardSet,
    since_ns: u64,
};

pub const ServeGuards = enum { covered, take };

/// The membership owner's word on a scope's client: `live` keeps the
/// scope, `dead` (evicted, or absent outside the owner's grace window)
/// releases it now, `unknown` (no owner view) leaves the grace belt.
pub const Verdict = enum { live, dead, unknown };

/// Who the holder asks whether a client is a live member (the membership
/// owner's verdict: its roster on the manager, its census on a joiner;
/// null = in-process contracts: every client is unknown). `since_ns` is
/// the instant the scope parked — the client was in contact then, so
/// only a word NEWER than it may call the client's absence a death.
pub const Liveness = struct {
    ctx: *anyopaque,
    verdict: *const fn (ctx: *anyopaque, client: u64, since_ns: u64, now_ns: u64) Verdict,
};

/// A scope's client key: the peer's token client id (`hash64` of its 16 B
/// member id — the identity's 12 B peer id, zero-padded), the SAME key the
/// token holder's lease oracle is asked about, so one membership verdict
/// serves both sweeps (`wire_plane.clientIdOf(shard.writerMemberId(..))`
/// ties to it). A session without a proven peer is client 0.
pub fn clientOf(peer: ?verbs.WireIdentity) u64 {
    const p = peer orelse return 0;
    var id: [16]u8 = @splat(0);
    @memcpy(id[0..12], &p.peerId());
    return checksum.hash64(&id);
}

pub const ParkedScopes = struct {
    gpa: Allocator,
    io: Io,
    locks: *dlm.Manager,
    lock: sync.Mutex = .init,
    map: std.AutoHashMapUnmanaged(Scope, Parked) = .empty,
    liveness: ?Liveness = null,
    /// The grace window an UNKNOWN client's scope keeps: the plane's
    /// `T_owner` once bound (the owner's eviction clock — a client it
    /// never listed cannot be evicted, so its scope lives one lease).
    grace_ns: u64 = SCOPE_GRACE_NS,

    pub fn init(gpa: Allocator, io: Io, locks: *dlm.Manager) ParkedScopes {
        return .{ .gpa = gpa, .io = io, .locks = locks };
    }

    pub fn deinit(self: *ParkedScopes) void {
        var it = self.map.valueIterator();
        while (it.next()) |p| p.set.release(self.io);
        self.map.deinit(self.gpa);
    }

    /// The owner's word bound (the wire plane's start) or withdrawn (its
    /// stop, before the oracle goes) under the sweep's lock.
    pub fn bindLiveness(self: *ParkedScopes, liveness: ?Liveness, grace_ns: u64) void {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        self.liveness = liveness;
        self.grace_ns = grace_ns;
    }

    /// Serve `XvGuards`: the TRY `lockMany` in THIS table of the requests
    /// the scope does not already cover (a second call under one scope
    /// must never wait behind itself), parked under `scope`; a held stripe
    /// is `Contended` — nothing taken, the session serves on.
    pub fn serve(self: *ParkedScopes, scope: Scope, client: u64, inodes: []const dlm.InodeReq, dentries: []const dlm.DentryReq) ServeError!void {
        assert(scope != 0);
        // Which requests are already covered? Judged under the map lock,
        // taken OUTSIDE it (the acquisition parks).
        var need_i: [dlm.wide_requests]dlm.InodeReq = undefined;
        var ni: usize = 0;
        var need_d: [dlm.wide_requests]dlm.DentryReq = undefined;
        var nd: usize = 0;
        {
            self.lock.lockUncancelable(self.io);
            defer self.lock.unlock(self.io);
            const existing = self.map.getPtr(scope);
            for (inodes) |r| {
                if (existing) |p| if (self.locks.covers(dlm.wide_requests, &p.set, &.{r}, &.{})) continue;
                if (ni == need_i.len) return error.PlanTooLarge;
                need_i[ni] = r;
                ni += 1;
            }
            for (dentries) |r| {
                if (existing) |p| if (self.locks.covers(dlm.wide_requests, &p.set, &.{}, &.{r})) continue;
                if (nd == need_d.len) return error.PlanTooLarge;
                need_d[nd] = r;
                nd += 1;
            }
        }
        if (ni == 0 and nd == 0) return;
        var set = (self.locks.tryLockManyWide(self.io, need_i[0..ni], need_d[0..nd]) catch return error.PlanTooLarge) orelse {
            stats.xv_cross_owner_guards_contended.inc();
            return error.Contended;
        };
        // The set is the parked scope's from here: `XvRelease` or the
        // sweep releases it on whichever serving lane or thread runs them,
        // so this frame's lock-order ledger lets go of it now (the
        // conveyor's hand-off; a release on a thread that never recorded
        // the acquisition is a no-op).
        lock_order.handedOff(.object);
        errdefer set.release(self.io);
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        const gop = self.map.getOrPut(self.gpa, scope) catch return error.OutOfMemory;
        if (!gop.found_existing) {
            gop.value_ptr.* = .{ .client = client, .set = set, .since_ns = clock.monotonicNs() };
            stats.xv_cross_owner_guards_parked.inc();
            return;
        }
        // Merge: the parked set grows by the stripes just taken (bounded
        // by the wide capacity — the plan bound shared with the intent).
        const p = gop.value_ptr;
        for (set.slice()) |g| {
            if (p.set.len == p.set.guards.len) return error.PlanTooLarge;
            p.set.guards[p.set.len] = g;
            p.set.len += 1;
        }
    }

    /// `XvRelease`: the scope's guards drop; false when nothing was parked.
    pub fn release(self: *ParkedScopes, scope: Scope) bool {
        self.lock.lockUncancelable(self.io);
        const entry = self.map.fetchRemove(scope);
        self.lock.unlock(self.io);
        var e = entry orelse return false;
        e.value.set.release(self.io);
        return true;
    }

    /// The served step's verdict: the scope covers every request ⇒ apply
    /// with the parked guards; else take the step's own.
    pub fn verdict(self: *ParkedScopes, scope: Scope, inodes: []const dlm.InodeReq, dentries: []const dlm.DentryReq) ServeGuards {
        if (scope == 0) return .take;
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        const p = self.map.getPtr(scope) orelse return .take;
        return if (self.locks.covers(dlm.wide_requests, &p.set, inodes, dentries)) .covered else .take;
    }

    /// The expiry sweep: a scope whose client the owner lists DEAD is
    /// released now (the §5.9 path); one whose client is unknown and aged
    /// past the grace window is released loud (the backstop); a live
    /// client's scope is kept however long its plan runs.
    pub fn sweepExpired(self: *ParkedScopes, now_ns: u64) u32 {
        const Doomed = struct { scope: Scope, dead: bool };
        var doomed: [64]Doomed = undefined;
        var n: usize = 0;
        {
            self.lock.lockUncancelable(self.io);
            defer self.lock.unlock(self.io);
            var it = self.map.iterator();
            while (it.next()) |e| {
                if (n == doomed.len) break;
                const v: Verdict = if (self.liveness) |l| l.verdict(l.ctx, e.value_ptr.client, e.value_ptr.since_ns, now_ns) else .unknown;
                switch (v) {
                    .live => {},
                    .dead => {
                        doomed[n] = .{ .scope = e.key_ptr.*, .dead = true };
                        n += 1;
                    },
                    .unknown => if (now_ns -| e.value_ptr.since_ns > self.grace_ns) {
                        doomed[n] = .{ .scope = e.key_ptr.*, .dead = false };
                        n += 1;
                    },
                }
            }
        }
        for (doomed[0..n]) |d| if (self.release(d.scope)) {
            if (d.dead) stats.xv_cross_owner_guards_swept_dead.inc() else stats.xv_cross_owner_guard_expiries.fire("a parked guard scope outlived its client");
        };
        return @intCast(n);
    }

    pub fn count(self: *ParkedScopes) usize {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        return self.map.count();
    }

    pub fn isParked(self: *ParkedScopes, scope: Scope) bool {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        return self.map.contains(scope);
    }
};

/// The holder's wire face for the two guard verbs.
pub const GuardsService = struct {
    parked: *ParkedScopes,
    volume: u16,

    pub fn register(self: *GuardsService, svc: *service.Service) void {
        svc.register(.xv_guards, .{ .ctx = self, .serve = &serveGuards });
        svc.register(.xv_release, .{ .ctx = self, .serve = &serveRelease });
    }

    fn serveGuards(ctx: *anyopaque, req: *const service.Request, _: []u8) service.Answer {
        const self: *GuardsService = @ptrCast(@alignCast(ctx));
        const r = req.payload.xv_guards;
        if (r.head.volume != self.volume) return service.Answer.rejected("xv_guards names a volume this node does not hold");
        var inodes: [dlm.wide_requests]dlm.InodeReq = undefined;
        for (r.inodes, 0..) |w, i| inodes[i] = .{ .ino = w.ino, .mode = if (w.exclusive != 0) .exclusive else .shared };
        var dents: [dlm.wide_requests]dlm.DentryReq = undefined;
        var nd: usize = 0;
        var it = r.dentries();
        while (it.next()) |d| : (nd += 1) dents[nd] = .{ .parent = d.head.parent, .name = d.name, .mode = if (d.head.exclusive != 0) .exclusive else .shared };
        self.parked.serve(r.head.scope, clientOf(req.peer), inodes[0..r.inodes.len], dents[0..nd]) catch |e| return switch (e) {
            error.PlanTooLarge => service.Answer.rejected("xv_guards plan past the wide bound"),
            error.OutOfMemory => service.Answer.refused(error.OutOfMemory, "out of memory parking the scope"),
            error.Contended => .{ .status = .{ .status = .busy, .err = error.Busy, .reason = "a stripe of the ask is held in this table" } },
        };
        return .{ .ok = .{ .xv_guards = .fresh } };
    }

    fn serveRelease(ctx: *anyopaque, req: *const service.Request, _: []u8) service.Answer {
        const self: *GuardsService = @ptrCast(@alignCast(ctx));
        const r = req.payload.xv_release;
        const was = self.parked.release(r.scope);
        return .{ .ok = .{ .xv_release = if (was) .fresh else .replay } };
    }
};

// ---------------------------------------------------------------------------
// The initiator's acquisition.
// ---------------------------------------------------------------------------

/// Whose table a key's lock lives in.
pub const Table = union(enum) {
    local,
    remote: struct { holder: AppenderId, link: Link },
    /// The holder has no bound endpoint: the ask is refused retryable.
    unreachable_: AppenderId,
};

/// The resolver of a GLOBAL ino's table (the slot's holder).
pub const Tables = struct {
    ctx: *anyopaque,
    tableOf: *const fn (ctx: *anyopaque, global_ino: u64) Table,
};

const Remote = struct { holder: AppenderId, link: Link };

/// A held travelling set: the local canonical set (the WIDE shape — the
/// one-intent striped `rmdir` holds `I{D}` + every `I{D_i}` + every
/// marker here when the stripes are ours) plus one parked scope per
/// remote table; released in reverse (the remote releases are
/// fire-and-forget — the holder's sweep is the belt).
pub const Travelling = struct {
    io: Io,
    scope: Scope,
    volume: u16,
    local: dlm.WideGuardSet = .{},
    remotes: std.ArrayListUnmanaged(Remote) = .empty,

    pub fn release(self: *Travelling, gpa: Allocator) void {
        self.local.release(self.io);
        for (self.remotes.items) |r| {
            _ = r.link.call(r.link.ctx, .{ .xv_release = .{ .volume = self.volume, .reserved = 0, .reserved2 = 0, .scope = self.scope, .ino = 0 } }) catch {};
        }
        self.remotes.deinit(gpa);
    }

    pub fn remoteCount(self: *const Travelling) usize {
        return self.remotes.items.len;
    }
};

pub const AcquireArgs = struct {
    own_id: AppenderId,
    volume: u16,
    scope: Scope,
    inodes: []const dlm.InodeReq,
    dentries: []const dlm.DentryReq,
    /// Unlink's discovery phase acquires NO foreign table (the op pays ONE
    /// round trip per holder, at its full acquisition).
    discovery: bool = false,
};

/// Acquire the op's set across tables in ascending appender-id order: the
/// local table at `own_id`'s rank, each remote set ONE `XvGuards`. A
/// refused/unreachable ask releases everything taken so far.
pub fn acquire(gpa: Allocator, io: Io, locks: *dlm.Manager, tables: Tables, a: AcquireArgs) Error!Travelling {
    assert(a.scope != 0);
    var t: Travelling = .{ .io = io, .scope = a.scope, .volume = a.volume };
    errdefer t.release(gpa);
    // The distinct tables, ascending by appender id.
    var ids: [dlm.wide_requests * 2]AppenderId = undefined;
    var links: [dlm.wide_requests * 2]?Link = undefined;
    var n: usize = 0;
    var has_local = false;
    for (a.inodes) |r| try noteTable(tables, r.ino, a.own_id, &ids, &links, &n, &has_local);
    for (a.dentries) |r| try noteTable(tables, r.parent, a.own_id, &ids, &links, &n, &has_local);
    sortTables(ids[0..n], links[0..n]);
    var local_taken = false;
    for (ids[0..n], links[0..n]) |id, link_opt| {
        if (a.discovery and id != a.own_id) continue;
        if (has_local and !local_taken and a.own_id < id) {
            t.local = try lockLocal(locks, io, tables, a);
            local_taken = true;
        }
        if (id == a.own_id) continue;
        const link = link_opt orelse return error.HolderUnreachable;
        try askRemote(gpa, io, &t, tables, a, id, link);
    }
    if (has_local and !local_taken) t.local = try lockLocal(locks, io, tables, a);
    return t;
}

fn noteTable(tables: Tables, ino: u64, own: AppenderId, ids: []AppenderId, links: []?Link, n: *usize, has_local: *bool) Error!void {
    switch (tables.tableOf(tables.ctx, ino)) {
        .local => {
            has_local.* = true;
            try pushId(ids, links, n, own, null);
        },
        .remote => |r| try pushId(ids, links, n, r.holder, r.link),
        .unreachable_ => return error.HolderUnreachable,
    }
}

fn pushId(ids: []AppenderId, links: []?Link, n: *usize, id: AppenderId, link: ?Link) Error!void {
    for (ids[0..n.*]) |x| if (x == id) return;
    if (n.* == ids.len) return error.PlanTooLarge;
    ids[n.*] = id;
    links[n.*] = link;
    n.* += 1;
}

fn sortTables(ids: []AppenderId, links: []?Link) void {
    // Insertion sort: ≤ a handful of tables per op.
    var i: usize = 1;
    while (i < ids.len) : (i += 1) {
        var j = i;
        while (j > 0 and ids[j - 1] > ids[j]) : (j -= 1) {
            std.mem.swap(AppenderId, &ids[j - 1], &ids[j]);
            std.mem.swap(?Link, &links[j - 1], &links[j]);
        }
    }
}

fn lockLocal(locks: *dlm.Manager, io: Io, tables: Tables, a: AcquireArgs) Error!dlm.WideGuardSet {
    var inodes: [dlm.wide_requests]dlm.InodeReq = undefined;
    var ni: usize = 0;
    var dents: [dlm.wide_requests]dlm.DentryReq = undefined;
    var nd: usize = 0;
    for (a.inodes) |r| if (tables.tableOf(tables.ctx, r.ino) == .local) {
        if (ni == inodes.len) return error.PlanTooLarge;
        inodes[ni] = r;
        ni += 1;
    };
    for (a.dentries) |r| if (tables.tableOf(tables.ctx, r.parent) == .local) {
        if (nd == dents.len) return error.PlanTooLarge;
        dents[nd] = r;
        nd += 1;
    };
    return locks.lockManyWide(io, inodes[0..ni], dents[0..nd]) catch return error.PlanTooLarge;
}

fn askRemote(gpa: Allocator, io: Io, t: *Travelling, tables: Tables, a: AcquireArgs, holder: AppenderId, link: Link) Error!void {
    var inodes: [dlm.wide_requests]verbs.GuardInodeWire = undefined;
    var ni: usize = 0;
    var dbuf: [dlm.wide_requests * (16 + 256)]u8 = undefined;
    var db: codec.Builder = .init(&dbuf);
    var nd: u16 = 0;
    for (a.inodes) |r| switch (tables.tableOf(tables.ctx, r.ino)) {
        .remote => |x| if (x.holder == holder) {
            inodes[ni] = .{ .ino = r.ino, .exclusive = @intFromBool(r.mode == .exclusive), .reserved = @splat(0) };
            ni += 1;
        },
        else => {},
    };
    for (a.dentries) |r| switch (tables.tableOf(tables.ctx, r.parent)) {
        .remote => |x| if (x.holder == holder) {
            verbs.XvGuardsReq.encodeDentry(&db, .{ .head = .{ .parent = r.parent, .exclusive = @intFromBool(r.mode == .exclusive), .name_len = @intCast(r.name.len), .reserved = @splat(0) }, .name = r.name }) catch return error.PlanTooLarge;
            nd += 1;
        },
        else => {},
    };
    const t0 = clock.monotonicNs();
    stats.xv_cross_owner_guard_rpcs.inc();
    const deadline = t0 + GUARD_CONTENTION_BOUND_MS * std.time.ns_per_ms;
    var backoff_ms: u64 = GUARD_RETRY_BACKOFF_MIN_MS;
    const rep: verbs.Reply = blk: while (true) {
        const r = link.call(link.ctx, .{ .xv_guards = .{ .head = .{ .volume = a.volume, .n_inodes = @intCast(ni), .n_dentries = nd, .reserved = 0, .scope = a.scope }, .inodes = inodes[0..ni], .dentries_bytes = db.written() } }) catch |e| {
            if (e == error.Busy) {
                // A held stripe at the holder: wait HERE, under the bound.
                const now = clock.monotonicNs();
                if (now < deadline) {
                    stats.xv_cross_owner_guard_retries.inc();
                    io.sleep(.fromMilliseconds(@intCast(@min(backoff_ms, (deadline - now) / std.time.ns_per_ms + 1))), .awake) catch {};
                    backoff_ms = @min(backoff_ms * 2, GUARD_RETRY_BACKOFF_MAX_MS);
                    continue;
                }
                stats.xv_cross_owner_guard_contention_refusals.inc();
            }
            // A refused or timed-out ask gets a fire-and-forget release behind it.
            _ = link.call(link.ctx, .{ .xv_release = .{ .volume = a.volume, .reserved = 0, .reserved2 = 0, .scope = a.scope, .ino = 0 } }) catch {};
            return switch (e) {
                error.StaleHolderView, error.SlotMoved => error.StaleHolderView,
                error.OutOfMemory => error.OutOfMemory,
                else => error.GuardRefused,
            };
        };
        break :blk r;
    };
    stats.xv_cross_owner_guard_rtt_ns.record(clock.monotonicNs() -| t0);
    _ = rep.xv_guards;
    t.remotes.append(gpa, .{ .holder = holder, .link = link }) catch return error.OutOfMemory;
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the parked scopes: a served XvGuards parks the holder's canonical set under the scope; a second ask under the scope skips covered stripes; the verdict is `covered` by (class, stripe) with the mode floor and `take` otherwise; release drops it; the sweep releases a DEAD client's scope at once (swept_dead, quiet), keeps a live client's however old, and lets an unknown client's go only past the grace window (the tripwire)" {
    const gpa = testing.allocator;
    var locks = try dlm.Manager.init(gpa, 64);
    defer locks.deinit();
    var parked = ParkedScopes.init(gpa, testing.io, &locks);
    defer parked.deinit();
    const scope = mintScope(5);
    try testing.expect(scope != 0 and scope & (1 << 40) != 0);
    try parked.serve(scope, 7, &.{.{ .ino = 10, .mode = .exclusive }}, &.{.{ .parent = 10, .name = "a", .mode = .exclusive }});
    try testing.expect(parked.isParked(scope));
    try testing.expectEqual(ServeGuards.covered, parked.verdict(scope, &.{.{ .ino = 10, .mode = .shared }}, &.{}));
    try testing.expectEqual(ServeGuards.take, parked.verdict(scope, &.{.{ .ino = 11, .mode = .shared }}, &.{}));
    try testing.expectEqual(ServeGuards.take, parked.verdict(0, &.{}, &.{}));
    // A second ask under the scope: the covered stripe is not re-taken
    // (it would wait behind itself), the new one is.
    try parked.serve(scope, 7, &.{ .{ .ino = 10, .mode = .exclusive }, .{ .ino = 11, .mode = .shared } }, &.{});
    try testing.expectEqual(ServeGuards.covered, parked.verdict(scope, &.{ .{ .ino = 10, .mode = .exclusive }, .{ .ino = 11, .mode = .shared } }, &.{}));
    try testing.expectEqual(ServeGuards.take, parked.verdict(scope, &.{.{ .ino = 11, .mode = .exclusive }}, &.{}));
    try testing.expect(parked.release(scope));
    try testing.expect(!parked.release(scope));
    try testing.expectEqual(@as(usize, 0), parked.count());
    // The stripes are free again.
    var s = (try locks.tryLockMany(testing.io, &.{.{ .ino = 10, .mode = .exclusive }}, &.{})).?;
    s.release(testing.io);
    // The sweep by the owner's word: client 7 live, 9 dead, 11 unknown.
    const Owner = struct {
        fn verdict(_: *anyopaque, client: u64, _: u64, _: u64) Verdict {
            return switch (client) {
                7 => .live,
                9 => .dead,
                else => .unknown,
            };
        }
    };
    var marker: u8 = 0;
    parked.liveness = .{ .ctx = &marker, .verdict = &Owner.verdict };
    try parked.serve(mintScope(6), 7, &.{.{ .ino = 20, .mode = .exclusive }}, &.{});
    try parked.serve(mintScope(7), 9, &.{.{ .ino = 21, .mode = .exclusive }}, &.{});
    try parked.serve(mintScope(8), 11, &.{.{ .ino = 22, .mode = .exclusive }}, &.{});
    const exp0 = stats.xv_cross_owner_guard_expiries.load();
    const dead0 = stats.xv_cross_owner_guards_swept_dead.load();
    const now = clock.monotonicNs();
    // The dead client's scope goes at once and quietly; the live and the
    // unknown stay (both fresh).
    try testing.expectEqual(@as(u32, 1), parked.sweepExpired(now));
    try testing.expectEqual(dead0 + 1, stats.xv_cross_owner_guards_swept_dead.load());
    try testing.expectEqual(exp0, stats.xv_cross_owner_guard_expiries.load());
    try testing.expect(parked.isParked(mintScope(6)) and !parked.isParked(mintScope(7)) and parked.isParked(mintScope(8)));
    // Past the grace window the unknown client's scope is released loud;
    // the live client's is kept however old.
    stats.xv_cross_owner_guard_expiries.resetLogBudgetForTest();
    @import("../../../core/log.zig").testing.expectErrorLogs(1);
    try testing.expectEqual(@as(u32, 1), parked.sweepExpired(now + parked.grace_ns + 1));
    try @import("../../../core/log.zig").testing.expectAllFired();
    try testing.expectEqual(exp0 + 1, stats.xv_cross_owner_guard_expiries.load());
    try testing.expect(parked.isParked(mintScope(6)) and !parked.isParked(mintScope(8)));
    // Without an owner's view every client is unknown: the grace belt alone.
    parked.liveness = null;
    try testing.expectEqual(@as(u32, 0), parked.sweepExpired(now));
    try testing.expect(parked.isParked(mintScope(6)));
}

test "a served XvGuards hands its set to the parked scope (0003 §AM; the move matrix under serving lanes fired `lock_order_ledger_overflows`): the serving thread's lock-order ledger holds no 4a entry once the scope parked, and an `XvRelease` served on ANOTHER thread drops the stripes" {
    const gpa = testing.allocator;
    var locks = try dlm.Manager.init(gpa, 64);
    defer locks.deinit();
    var parked = ParkedScopes.init(gpa, testing.io, &locks);
    defer parked.deinit();
    const scope = mintScope(6);
    try parked.serve(scope, 7, &.{.{ .ino = 20, .mode = .exclusive }}, &.{.{ .parent = 20, .name = "b", .mode = .exclusive }});
    try testing.expect(parked.isParked(scope));
    try testing.expect(!lock_order.enabled or !lock_order.holds(.object));
    const Release = struct {
        fn run(p: *ParkedScopes, sc: Scope, was: *bool) void {
            was.* = p.release(sc);
        }
    };
    var was = false;
    const t = try std.Thread.spawn(.{}, Release.run, .{ &parked, scope, &was });
    t.join();
    try testing.expect(was);
    var again = (try locks.tryLockMany(testing.io, &.{.{ .ino = 20, .mode = .exclusive }}, &.{})).?;
    again.release(testing.io);
}

test "clientOf: the token client id of the peer's member id — stable, distinct across two members, 0 for an unproven session" {
    const a: verbs.WireIdentity = .{ .node_token = 0x1111, .writer_id = 5, .mount_slot = 1, .reserved = 0 };
    const b: verbs.WireIdentity = .{ .node_token = 0x1111, .writer_id = 5, .mount_slot = 2, .reserved = 0 };
    // The incarnation (writer id) is not the member: one key per (node, mount slot).
    const a2: verbs.WireIdentity = .{ .node_token = 0x1111, .writer_id = 9, .mount_slot = 1, .reserved = 0 };
    try testing.expect(clientOf(a) != 0);
    try testing.expectEqual(clientOf(a), clientOf(a2));
    try testing.expect(clientOf(a) != clientOf(b));
    try testing.expectEqual(@as(u64, 0), clientOf(null));
}
