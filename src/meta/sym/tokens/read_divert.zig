//! The read divert and the token plane's wire binding (charter §5.5
//! "Every writer is also a token client of the holders it writes to (the
//! read divert)", "Readers' planes FOLLOW a holder that moved its
//! listener"; `docs/porting/sym_tokens_intents_striping.md` §1.2 the frame
//! law, §1.3 G3/G5, §1.6 planes, §1.9, §1.10, §3.4): `tokenServe(object,
//! wants)` is the ONE serve the five read verbs take — `null` = read
//! locally (unarmed, a slot this mount leases, an unleased slot on the
//! manager: decided on the resolver's bits alone, no table lock, no
//! allocation), else the served token entry or "gone at its holder". The
//! armed body resolves the per-holder plane (dialed lazily, ONCE, off
//! durable state), follows a `NotHolder` redirect ONCE inside the same op,
//! and never dials a holder the owner lists DEAD.
//!
//! The wire binding: `HolderService` registers the holder's verbs
//! (`Grant`, `RecallAck`, `Release`, `CustodyGrant`) on the mount's ONE
//! service under the frame law — the lessee check, the membership lease
//! FIRST, then dispatch; `ReaderService` registers the PUSHED `Recall` on
//! the reader's service; `LinkPusher` is the holder's push over the
//! readers' sessions; `KernelInvalidator` is the FUSE-layer sink that
//! invalidates a recalled object's kernel view (`FUSE_NOTIFY_INVAL_INODE`
//! + `PRUNE` through the session). PORTABLE.

const std = @import("std");
const Allocator = std.mem.Allocator;
const Io = std.Io;
const counters = @import("../../../core/counters.zig");
const errors = @import("../../../core/errors.zig");
const log = @import("../../../core/log.zig");
const sync = @import("../../../sync/sync.zig");
const verbs = @import("../../../wire/verbs.zig");
const service = @import("../../../wire/service.zig");
const screen = @import("../../../wire/screen.zig");
const codec = @import("../../../wire/codec.zig");
const record = @import("../../record.zig");
const route_table = @import("../../routing/route_table.zig");
const inos = @import("../../inos.zig");
const economy = @import("../../economy.zig");
const table = @import("token_table.zig");
pub const cache = @import("token_cache.zig");

pub const Object = table.Object;
pub const ClientId = table.ClientId;
pub const Link = cache.Link;
pub const Answer = service.Answer;

pub const stats = struct {
    /// Per-holder reader planes bound by this mount.
    pub var dlm_token_reader_holder_planes: counters.Gauge = .{};
    /// A foreign object whose holder could not be bound (no endpoint yet):
    /// refused EAGAIN — the bounded-staleness projection is never served
    /// in its place.
    pub var dlm_token_reader_unbound_holders: counters.ShardedCounter = .{};
    /// Resolutions of a foreign object's holder (every armed serve).
    pub var dlm_token_reader_holder_resolves: counters.ShardedCounter = .{};
    /// A `NotHolder` redirect followed ONCE inside the same op.
    pub var dlm_token_reader_redirects_followed: counters.ShardedCounter = .{};
    /// A fail-closed channel re-pointed and re-probed ONCE inside the
    /// same op (the serve then retries on a fresh grant).
    pub var dlm_token_reader_channel_reprobes: counters.ShardedCounter = .{};
    /// A holder that could not be reached: the slot re-resolved ONCE
    /// inside the same op (a dead holder's recovered slot is served from
    /// its new home).
    pub var dlm_token_reader_unreachable_resolves: counters.ShardedCounter = .{};
    /// Serves answered locally because the slot is this mount's (or
    /// unleased on the manager): the writer's "reads locally" verdict.
    pub var dlm_token_local_reads: counters.ShardedCounter = .{};
    /// `getattr(1)` served off the LAST root entry this mount held while
    /// the root's holder was unreachable (a parked joiner under a dead
    /// manager, `.stats` reachable): a stale root attribute misleads no
    /// reader — the root pin never moves.
    pub var dlm_token_root_served_last: counters.ShardedCounter = .{};
    /// The kernel view invalidated for a recalled object (+ the prune).
    pub var dlm_token_kernel_invals: counters.ShardedCounter = .{};
    pub var dlm_token_kernel_prunes: counters.ShardedCounter = .{};
    /// A recalled name dropped from this kernel's dentry cache, and a
    /// recall that named none of its changed dentries (the whole cache
    /// dropped) — decision 0003 §AB.2.
    pub var dlm_token_kernel_name_invals: counters.ShardedCounter = .{};
    pub var dlm_token_kernel_epochs: counters.ShardedCounter = .{};
    /// A notify the sideband could not queue (the kernel re-walks on the
    /// zero TTL regardless).
    pub var dlm_token_kernel_notify_dropped: counters.ShardedCounter = .{};
};

// ---------------------------------------------------------------------------
// The resolver seam (the slots agent's `SlotHolderCache` binds it).
// ---------------------------------------------------------------------------

/// Where an object's slot is served from, as this mount sees it.
pub const Home = union(enum) {
    /// Read locally: a slot this mount leases, or an unleased slot on the
    /// manager (which maintains every unleased tree itself).
    local,
    /// Another appender leases it: its identity word and appender id.
    foreign: struct { holder: u32, identity: ClientId },
    /// The lessee has no published endpoint yet.
    unreachable_: struct { holder: u32 },
    /// The lessee the membership owner lists DEAD: never dialled.
    dead: struct { holder: u32 },
};

pub const Resolver = struct {
    ctx: *anyopaque,
    /// The home of routing slot `slot`.
    homeOf: *const fn (ctx: *anyopaque, slot: u16) Home,
    /// Re-resolve `slot` off DURABLE state after a redirect naming
    /// `current` (the manager's word is fresher than the projection the
    /// ledger cadence hands a joiner).
    refresh: *const fn (ctx: *anyopaque, slot: u16, current: u32) Home,
    /// Dial (or re-point) the plane to `identity`'s listener: the link, or
    /// null when no endpoint is published.
    dial: *const fn (ctx: *anyopaque, identity: ClientId, holder: u32) ?Link,

    /// A flat / single-writer mount: every slot is local.
    pub const unarmed: Resolver = .{ .ctx = @ptrFromInt(1), .homeOf = &allLocal, .refresh = &allLocalRefresh, .dial = &noDial };

    fn allLocal(_: *anyopaque, _: u16) Home {
        return .local;
    }
    fn allLocalRefresh(_: *anyopaque, _: u16, _: u32) Home {
        return .local;
    }
    fn noDial(_: *anyopaque, _: ClientId, _: u32) ?Link {
        return null;
    }
};

// ---------------------------------------------------------------------------
// The divert.
// ---------------------------------------------------------------------------

pub const Error = cache.Error || error{ReaderUnbound};

pub fn coreError(e: Error) errors.Error {
    return switch (e) {
        error.ReaderUnbound => error.HolderUnreachable,
        else => |x| cache.coreError(x),
    };
}

/// The per-holder reader planes of one mount and the ONE serve.
pub const Divert = struct {
    gpa: Allocator,
    io: Io,
    resolver: Resolver,
    client_id: ClientId,
    volume: u16,
    sink: cache.RecallSink,
    lease: cache.SelfLease,
    budget_bytes: u64,
    /// The mount kernel's longest TTL (decision 0003 §AG.2): every plane's
    /// serves pin their entries this long (`setKernelPin`).
    kernel_pin_ns: u64 = std.math.maxInt(u64),
    /// `SQUEEZEFS_TEST_TOKEN_EVICT_AFTER_SERVE`: every foreign serve's entry
    /// is evicted under the room law right after it is served — a reader
    /// that holds no token when the next change lands.
    evict_after_serve: bool = false,
    lock: sync.Mutex = .init,
    planes: std.AutoHashMapUnmanaged(ClientId, *cache.Reader) = .empty,
    /// The root's attributes as last served under a token (kept past the
    /// cache's shed on a lapsed lease: the parked joiner's `getattr(1)`).
    last_root: ?record.InodeValue = null,
    last_root_lock: sync.Mutex = .init,

    pub fn init(gpa: Allocator, io: Io, resolver: Resolver, client_id: ClientId, volume: u16, sink: cache.RecallSink, lease: cache.SelfLease, budget_bytes: u64) Divert {
        return .{ .gpa = gpa, .io = io, .resolver = resolver, .client_id = client_id, .volume = volume, .sink = sink, .lease = lease, .budget_bytes = budget_bytes };
    }

    /// The clean leave: every plane releases its tokens and dies.
    pub fn deinit(self: *Divert) void {
        var it = self.planes.valueIterator();
        while (it.next()) |r| {
            r.*.stop(self.io);
            r.*.deinit();
            self.gpa.destroy(r.*);
            stats.dlm_token_reader_holder_planes.sub(1);
        }
        self.planes.deinit(self.gpa);
    }

    /// The kernel's longest TTL for answers served under a token: from here
    /// on (and on every plane already bound) a token whose entry served the
    /// kernel within it is never released, only dropped (decision 0003
    /// §AG.2). A mount sets it before it serves.
    pub fn setKernelPin(self: *Divert, io: Io, ns: u64) void {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        self.kernel_pin_ns = ns;
        var it = self.planes.valueIterator();
        while (it.next()) |r| {
            r.*.cache.lock.lockUncancelable(io);
            r.*.cache.kernel_pin_ns = ns;
            r.*.cache.lock.unlock(io);
        }
    }

    /// The plane for `identity`, bound ONCE on first touch off the
    /// resolver's endpoint.
    pub fn planeFor(self: *Divert, io: Io, identity: ClientId, holder: u32) Error!*cache.Reader {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        if (self.planes.get(identity)) |r| return r;
        const link = self.resolver.dial(self.resolver.ctx, identity, holder) orelse {
            stats.dlm_token_reader_unbound_holders.inc();
            return error.ReaderUnbound;
        };
        const r = self.gpa.create(cache.Reader) catch return error.OutOfMemory;
        r.* = cache.Reader.init(self.gpa, io, self.client_id, self.volume, link, self.sink, self.lease, self.budget_bytes);
        r.cache.kernel_pin_ns = self.kernel_pin_ns;
        self.planes.put(self.gpa, identity, r) catch {
            self.gpa.destroy(r);
            return error.OutOfMemory;
        };
        stats.dlm_token_reader_holder_planes.add(1);
        return r;
    }

    /// A pushed recall's changed names into the sink (decision 0003 §AB.2).
    pub fn noteRecallNames(self: *Divert, r: verbs.TokenRecallReq) void {
        var it = r.names();
        while (it.next()) |n| self.sink.noteName(self.sink.ctx, n.parent, n.name);
        if (r.head.unnamed != 0) self.sink.noteUnnamed(self.sink.ctx);
    }

    /// The plane already bound for `identity` (the pushed recall's route).
    pub fn planeOf(self: *Divert, io: Io, identity: ClientId) ?*cache.Reader {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        return self.planes.get(identity);
    }

    /// A holder MOVED its listener: the plane re-points in place (every
    /// token dropped, the channel fail-closed until a round completes).
    /// The drop never waits on the dial: a successor whose listener does
    /// not answer yet (THE KILL ROWS (b) under gate load) must leave no
    /// token of the dead holder's grant table served, so the plane
    /// re-points onto its OLD link — the probe round fails on the dead
    /// session and the serve re-dials through the resolver (`reprobe`).
    pub fn repoint(self: *Divert, io: Io, identity: ClientId, holder: u32) bool {
        const r = self.planeOf(io, identity) orelse return false;
        const link = self.resolver.dial(self.resolver.ctx, identity, holder) orelse blk: {
            log.warn("token plane of holder {d}: its listener did not dial at the re-point — every token dropped, the channel re-probed at the next serve", .{holder});
            break :blk r.link;
        };
        r.repoint(io, link);
        return true;
    }

    /// THE RE-JOIN IN PLACE (decision 0003 §BS): the home shard evicted
    /// this member and every holder swept its grants — every plane drops
    /// every token and re-probes its channel at the next serve (onto its
    /// own link: the holders' listeners did not move). Returns the planes
    /// dropped.
    pub fn dropAll(self: *Divert, io: Io) usize {
        var readers: std.ArrayList(*cache.Reader) = .empty;
        defer readers.deinit(self.gpa);
        {
            self.last_root_lock.lockUncancelable(io);
            defer self.last_root_lock.unlock(io);
            self.last_root = null;
        }
        {
            self.lock.lockUncancelable(io);
            defer self.lock.unlock(io);
            var it = self.planes.valueIterator();
            while (it.next()) |r| readers.append(self.gpa, r.*) catch break;
        }
        for (readers.items) |r| r.repoint(io, r.link);
        return readers.items.len;
    }

    /// A fail-closed channel re-probed inside the serve that needs a
    /// fetch: re-dialled through the resolver, the probe round deciding
    /// (`cache.Reader.redial`). Returns whether the channel re-opened.
    fn reprobe(self: *Divert, io: Io, identity: ClientId, holder: u32) bool {
        const r = self.planeOf(io, identity) orelse return false;
        const link = self.resolver.dial(self.resolver.ctx, identity, holder) orelse return false;
        return r.redial(io, link);
    }

    /// Is the object's slot another appender's (a token's business), by
    /// the resolver's word alone — no plane, no dial, no serve.
    pub fn isForeign(self: *Divert, object: Object) bool {
        return self.resolver.homeOf(self.resolver.ctx, route_table.slotOfEffective(object)) != .local;
    }

    /// `tokenServe(object, wants)`: null = read locally; else the served
    /// entry (`Serve.gone()` = gone at its holder).
    pub fn serve(self: *Divert, io: Io, object: Object, wants: cache.Wants) Error!?cache.Serve {
        const slot = route_table.slotOfEffective(object);
        var home = self.resolver.homeOf(self.resolver.ctx, slot);
        var followed = false;
        var reprobed = false;
        while (true) {
            switch (home) {
                .local => {
                    stats.dlm_token_local_reads.inc();
                    return null;
                },
                .unreachable_ => |u| {
                    home = self.reresolveUnreachable(slot, u.holder, &followed) orelse return error.HolderUnreachable;
                    continue;
                },
                .dead => return error.HolderUnreachable,
                .foreign => |f| {
                    stats.dlm_token_reader_holder_resolves.inc();
                    const plane = try self.planeFor(io, f.identity, f.holder);
                    const served = plane.serve(io, object, wants) catch |e| switch (e) {
                        error.StaleHolderView => {
                            // The holder's word is fresher than our view: a
                            // WRITER re-resolves ONCE inside the same op.
                            if (followed) return e;
                            followed = true;
                            stats.dlm_token_reader_redirects_followed.inc();
                            home = self.resolver.refresh(self.resolver.ctx, slot, plane.last_redirect_holder.load(.acquire));
                            continue;
                        },
                        error.TokenChannelStale => {
                            // A failed round closed the channel and this
                            // serve needs a fetch (a hit was served above
                            // it): the plane re-probes ONCE inside the same
                            // op — the link re-resolved and re-dialled, the
                            // probe round deciding — and the serve retries
                            // on a fresh grant; a probe that fails
                            // re-resolves the slot (below), else leaves the
                            // typed EAGAIN and the cache intact.
                            if (!reprobed) {
                                reprobed = true;
                                stats.dlm_token_reader_channel_reprobes.inc();
                                if (self.reprobe(io, f.identity, f.holder)) continue;
                            }
                            home = self.reresolveUnreachable(slot, f.holder, &followed) orelse return e;
                            continue;
                        },
                        error.HolderUnreachable, error.Wire => {
                            home = self.reresolveUnreachable(slot, f.holder, &followed) orelse return e;
                            continue;
                        },
                        else => return e,
                    };
                    if (object == inos.ROOT_INO) if (served.entry) |e| self.rememberRoot(e.attrs);
                    if (self.evict_after_serve) plane.evict(io, object);
                    return served;
                },
            }
        }
    }

    /// The holder `holder` could not be reached (refused, no endpoint, a
    /// lost session): the slot's home is re-resolved off durable state
    /// ONCE per op (shared with the redirect's follow) — a DEAD holder's
    /// slot the manager has since recovered is served from its new home
    /// instead of answering EAGAIN until this mount's view ages out. Null
    /// = already followed, or the manager names the same holder.
    fn reresolveUnreachable(self: *Divert, slot: u16, holder: u32, followed: *bool) ?Home {
        if (followed.*) return null;
        followed.* = true;
        stats.dlm_token_reader_unreachable_resolves.inc();
        const h = self.resolver.refresh(self.resolver.ctx, slot, holder);
        return switch (h) {
            .foreign => |f| if (f.holder == holder) null else h,
            .unreachable_ => |u| if (u.holder == holder) null else h,
            .dead => null,
            .local => h,
        };
    }

    fn rememberRoot(self: *Divert, attrs: record.InodeValue) void {
        self.last_root_lock.lockUncancelable(self.io);
        defer self.last_root_lock.unlock(self.io);
        self.last_root = attrs;
    }

    /// The root's attributes as last served under a token, if ever (the
    /// parked joiner's `getattr(1)` under a dead manager; counted).
    pub fn lastRoot(self: *Divert) ?record.InodeValue {
        self.last_root_lock.lockUncancelable(self.io);
        defer self.last_root_lock.unlock(self.io);
        const v = self.last_root orelse return null;
        stats.dlm_token_root_served_last.inc();
        return v;
    }
};

// ---------------------------------------------------------------------------
// The holder's wire face.
// ---------------------------------------------------------------------------

/// The data plane's custody arbitration behind `CustodyGrant` (step G5):
/// the S9 write-custody acquire served by the SLOT HOLDER, its reply
/// carrying the file's read token.
pub const CustodyVerdict = union(enum) {
    granted: struct { epoch: u64 },
    /// The custody status word (the data plane's).
    refused: struct { status: u32 },
    /// A handover or quarantine stands: `Deferred`.
    deferred,
};

pub const CustodySeam = struct {
    ctx: *anyopaque,
    acquire: *const fn (ctx: *anyopaque, client: ClientId, object: Object, span_start: u64, span_len: u64, concurrent_write: bool, lease_epoch: u64) CustodyVerdict,
    /// A token that cannot be served RELEASES the custody: the caller
    /// adopts both words or neither.
    release: *const fn (ctx: *anyopaque, client: ClientId, object: Object, epoch: u64) void,
};

pub const HolderService = struct {
    io: Io,
    holder: *table.Holder,
    source: table.RecordSource,
    resolver: Resolver,
    custody: ?CustodySeam,

    pub fn register(self: *HolderService, svc: *service.Service) void {
        svc.register(.token_grant, .{ .ctx = self, .serve = &serveGrant });
        svc.register(.token_recall_ack, .{ .ctx = self, .serve = &serveAck });
        svc.register(.token_release, .{ .ctx = self, .serve = &serveRelease });
        svc.register(.custody_grant, .{ .ctx = self, .serve = &serveCustody });
    }

    pub fn unregister(_: *HolderService, svc: *service.Service) void {
        svc.unregister(.token_grant);
        svc.unregister(.token_recall_ack);
        svc.unregister(.token_release);
        svc.unregister(.custody_grant);
    }

    /// The lessee check (spec G3 first): null = this plane serves it.
    fn redirect(self: *HolderService, object: Object) ?verbs.GrantPage {
        const attrs = std.mem.zeroes(verbs.InodeValueWire);
        return switch (self.resolver.homeOf(self.resolver.ctx, route_table.slotOfEffective(object))) {
            .local => null,
            .foreign => |f| blk: {
                table.stats.dlm_token_not_holder_redirects.inc();
                break :blk .{ .head = .{ .kind = @backingInt(verbs.GrantPageKind.not_holder), .flags = 0, .reserved = 0, .n_xattrs = 0, .n_dir = 0, .holder = f.holder, .attrs = attrs, .reserved2 = @splat(0) }, .xattrs_bytes = "", .dir_bytes = "" };
            },
            .unreachable_ => |u| .{ .head = .{ .kind = @backingInt(verbs.GrantPageKind.not_holder), .flags = 0, .reserved = 0, .n_xattrs = 0, .n_dir = 0, .holder = u.holder, .attrs = attrs, .reserved2 = @splat(0) }, .xattrs_bytes = "", .dir_bytes = "" },
            .dead => |d| blk: {
                table.stats.dlm_token_holder_dead_redirects.inc();
                break :blk .{ .head = .{ .kind = @backingInt(verbs.GrantPageKind.holder_dead), .flags = 0, .reserved = 0, .n_xattrs = 0, .n_dir = 0, .holder = d.holder, .attrs = attrs, .reserved2 = @splat(0) }, .xattrs_bytes = "", .dir_bytes = "" };
            },
        };
    }

    fn grantAnswer(reply: table.Error!table.GrantReply) Answer {
        const r = reply catch |e| return switch (e) {
            error.TableFull => Answer.deferred("the holder's grant table is full"),
            error.ParkExpired => Answer.deferred("the grant parked past the recall deadline on an object a pass holds in flight"),
            error.OutOfMemory => Answer.refused(error.OutOfMemory, "out of memory building the grant page"),
            error.Io, error.Corrupt => Answer.refused(error.InputOutput, "the holder's records could not be read"),
        };
        return switch (r) {
            .granted => |p| .{ .ok = .{ .token_grant = p.wire() } },
            .gone => .{ .ok = .{ .token_grant = .{ .head = .{ .kind = @backingInt(verbs.GrantPageKind.gone), .flags = 0, .reserved = 0, .n_xattrs = 0, .n_dir = 0, .holder = 0, .attrs = std.mem.zeroes(verbs.InodeValueWire), .reserved2 = @splat(0) }, .xattrs_bytes = "", .dir_bytes = "" } } },
            .not_member => .{ .status = .{ .status = .not_member } },
        };
    }

    fn serveGrant(ctx: *anyopaque, req: *const service.Request, scratch: []u8) Answer {
        const self: *HolderService = @ptrCast(@alignCast(ctx));
        const r = req.payload.token_grant;
        switch (screen.tokenGrant(r)) {
            .ok => {},
            .rejected => |why| return Answer.rejected(@tagName(why)),
        }
        if (self.redirect(r.head.object)) |page| return .{ .ok = .{ .token_grant = page } };
        return grantAnswer(self.holder.serveGrant(self.io, .{ .client = r.head.client_id, .object = r.head.object, .wants_dentries = r.head.wants & verbs.WANT_DENTRIES != 0, .wants_records = r.head.wants & verbs.WANT_RECORDS != 0, .after = r.head.after, .xattr_after = r.bytes }, self.source, scratch));
    }

    fn serveAck(ctx: *anyopaque, req: *const service.Request, _: []u8) Answer {
        const self: *HolderService = @ptrCast(@alignCast(ctx));
        const r = req.payload.token_recall_ack;
        const n = self.holder.serveAck(self.io, r.client_id, r.frame_id);
        return .{ .ok = .{ .token_recall_ack = if (n != 0) verbs.Ack.fresh else verbs.Ack.replay } };
    }

    fn serveRelease(ctx: *anyopaque, req: *const service.Request, _: []u8) Answer {
        const self: *HolderService = @ptrCast(@alignCast(ctx));
        const r = req.payload.token_release;
        switch (screen.tokenRelease(r)) {
            .ok => {},
            .rejected => |why| return Answer.rejected(@tagName(why)),
        }
        const n = self.holder.serveRelease(self.io, r.head.client_id, r.items);
        return .{ .ok = .{ .token_release = .{ .count = n, .reserved = 0 } } };
    }

    /// G5: the words screened (a REGULAR FILE — never a directory), the
    /// lessee, the arbitration, THEN the grant under the same gate; a
    /// token that cannot be served releases the custody.
    fn serveCustody(ctx: *anyopaque, req: *const service.Request, scratch: []u8) Answer {
        const self: *HolderService = @ptrCast(@alignCast(ctx));
        const r = req.payload.custody_grant;
        switch (screen.custodyGrant(r)) {
            .ok => {},
            .rejected => |why| {
                table.stats.dlm_token_custody_rejected.fire(@tagName(why));
                return Answer.rejected(@tagName(why));
            },
        }
        if (r.span_len == 0) {
            table.stats.dlm_token_custody_rejected.fire("custody span empty");
            return Answer.rejected("custody span empty");
        }
        if (self.redirect(r.object)) |page| {
            if (page.head.kind == @backingInt(verbs.GrantPageKind.holder_dead)) return .{ .status = .{ .status = .refused, .err = error.HolderUnreachable, .reason = "the slot's lessee is dead", .holder = page.head.holder } };
            return .{ .status = .{ .status = .refused, .err = error.SlotMoved, .reason = "another appender leases the object's slot", .holder = page.head.holder } };
        }
        const v = (self.source.inode(self.source.ctx, r.object) catch return Answer.refused(error.InputOutput, "the object's record could not be read")) orelse {
            table.stats.dlm_token_custody_rejected.fire("custody object absent");
            return Answer.rejected("custody object absent");
        };
        if (economy.isDirMode(v.mode)) {
            table.stats.dlm_token_custody_rejected.fire("a directory is never a custody object");
            return Answer.rejected("a directory is never a custody object");
        }
        const custody = self.custody orelse return .{ .status = .{ .status = .not_holder } };
        const verdict = custody.acquire(custody.ctx, r.client_id, r.object, r.span_start, r.span_len, r.concurrent_write != 0, r.lease_epoch);
        const epoch = switch (verdict) {
            .granted => |g| g.epoch,
            .refused => |x| return .{ .ok = .{ .custody_grant = .{ .head = .{ .kind = @backingInt(verbs.CustodyKind.refused), .reserved = @splat(0), .status = x.status, .grant_epoch = 0, .page_len = 0, .reserved2 = 0 }, .page_bytes = "" } } },
            .deferred => return Answer.deferred("a handover or quarantine stands on the object's slot"),
        };
        const reply = self.holder.serveGrant(self.io, .{ .client = r.client_id, .object = r.object, .wants_dentries = false, .wants_records = true, .after = 0, .xattr_after = "" }, self.source, scratch[0 .. scratch.len / 2]);
        const page = switch (reply catch |e| {
            custody.release(custody.ctx, r.client_id, r.object, epoch);
            return grantAnswer(e);
        }) {
            .granted => |p| p,
            .gone, .not_member => |_, tag| {
                custody.release(custody.ctx, r.client_id, r.object, epoch);
                return grantAnswer(if (tag == .gone) table.GrantReply.gone else table.GrantReply.not_member);
            },
        };
        // The page rides the custody reply's tail: encode it into the
        // scratch's second half.
        var b: codec.Builder = .init(scratch[scratch.len / 2 ..]);
        codec.encodePayload(verbs.GrantPage, page.wire(), &b) catch {
            custody.release(custody.ctx, r.client_id, r.object, epoch);
            return Answer.refused(error.ValueTooLarge, "the custody reply could not carry the token page");
        };
        return .{ .ok = .{ .custody_grant = .{ .head = .{ .kind = @backingInt(verbs.CustodyKind.granted), .reserved = @splat(0), .status = 0, .grant_epoch = epoch, .page_len = @intCast(b.pos), .reserved2 = 0 }, .page_bytes = b.written() } } };
    }
};

// ---------------------------------------------------------------------------
// The reader's wire face: the pushed recall.
// ---------------------------------------------------------------------------

pub const ReaderService = struct {
    io: Io,
    divert: *Divert,

    pub fn register(self: *ReaderService, svc: *service.Service) void {
        svc.register(.token_recall, .{ .ctx = self, .serve = &serveRecall });
    }

    pub fn unregister(_: *ReaderService, svc: *service.Service) void {
        svc.unregister(.token_recall);
    }

    fn serveRecall(ctx: *anyopaque, req: *const service.Request, _: []u8) Answer {
        const self: *ReaderService = @ptrCast(@alignCast(ctx));
        const r = req.payload.token_recall;
        switch (screen.tokenRecall(r)) {
            .ok => {},
            .rejected => |why| return Answer.rejected(@tagName(why)),
        }
        // A frame for a holder this reader never bound names tokens it
        // never held: acked at once (nothing to drain, nothing to purge).
        const plane = self.divert.planeOf(self.io, r.head.holder) orelse return .{ .ok = .{ .token_recall = .acked } };
        // The changed names leave this kernel before the ack (decision
        // 0003 §AB.2): the holder's change lands only after it. A lookup in
        // flight on a parent holds the parent's `i_rwsem`, so its dentry is
        // installed before the invalidation takes it — never after.
        self.divert.noteRecallNames(r);
        const verdict = plane.handleRecall(self.io, r.head.frame_id, r.items) catch return Answer.refused(error.OutOfMemory, "out of memory queueing the recall frame");
        return .{ .ok = .{ .token_recall = switch (verdict) {
            .acked => verbs.RecallVerdict.acked,
            .deferred => verbs.RecallVerdict.deferred,
        } } };
    }
};

// ---------------------------------------------------------------------------
// The holder's push over the readers' sessions.
// ---------------------------------------------------------------------------

/// The holder's view of its readers' listeners: a link per client (the
/// session the reader's endpoint was dialed on; null = unbound).
pub const Peers = struct {
    ctx: *anyopaque,
    linkFor: *const fn (ctx: *anyopaque, client: ClientId) ?Link,
};

pub const LinkPusher = struct {
    volume: u16,
    identity: ClientId,
    peers: Peers,

    pub fn pusher(self: *LinkPusher) table.Pusher {
        return .{ .ctx = self, .push = &push };
    }

    fn push(ctx: *anyopaque, client: ClientId, frame_id: u64, objects: []const Object, names: table.RecallNames) table.PushVerdict {
        const self: *LinkPusher = @ptrCast(@alignCast(ctx));
        const link = self.peers.linkFor(self.peers.ctx, client) orelse return .unreachable_;
        // The names ride the first push; more than a frame carries makes
        // the frame UNNAMED (the reader drops every dentry instead).
        const fits = names.notes.len <= verbs.RECALL_NAMES_MAX;
        var name_list: []const verbs.RecallName = if (fits) names.notes else &.{};
        var unnamed = names.unnamed or !fits;
        var off: usize = 0;
        var verdict: table.PushVerdict = .acked;
        // A union wider than one frame's cap goes in several pushes under
        // ONE frame id; the ack covers them all.
        while (off < objects.len) {
            const n = @min(objects.len - off, verbs.RECALL_BATCH_MAX);
            const head: verbs.RecallFrameHead = .{ .volume = self.volume, .n_names = @intCast(name_list.len), .n = @intCast(n), .frame_id = frame_id, .holder = self.identity, .unnamed = @intFromBool(unnamed), .reserved = @splat(0) };
            const rep = link.call(link.ctx, .{ .token_recall = .{ .head = head, .items = objects[off .. off + n], .name_list = name_list } }) catch return .unreachable_;
            if (rep.token_recall.kind == @backingInt(verbs.RecallVerdictKind.deferred)) verdict = .deferred;
            off += n;
            name_list = &.{};
            unnamed = false;
        }
        return verdict;
    }
};

// ---------------------------------------------------------------------------
// The FUSE-layer sink: a recalled object's kernel view.
// ---------------------------------------------------------------------------

/// The session's notify writes (the sideband carries them).
pub const NotifySeam = struct {
    ctx: *anyopaque,
    invalInode: *const fn (ctx: *anyopaque, ino: u64, off: i64, len: i64) error{QueueFull}!void,
    prune: *const fn (ctx: *anyopaque, nodeids: []const u64) error{QueueFull}!void,
    /// `FUSE_NOTIFY_INVAL_ENTRY` of `(parent, name)`; never dropped — a
    /// full sideband owes the kernel an epoch instead (decision 0003 §AB.2).
    invalEntry: *const fn (ctx: *anyopaque, parent: u64, name: []const u8) void,
    /// `FUSE_NOTIFY_INC_EPOCH`: every dentry the kernel holds goes stale.
    incEpoch: *const fn (ctx: *anyopaque) void,
};

/// `note_recalled_object`: the whole inode invalidated (`(0, 0)` = every
/// page + the attributes), then pruned (a pre-7.45 kernel answers EINVAL,
/// counted by the transport). The object is a local key ino; the kernel
/// knows the GLOBAL one.
pub const KernelInvalidator = struct {
    notify: NotifySeam,

    pub fn noteRecalled(self: *KernelInvalidator, object: Object) void {
        const global = route_table.globalOfEffective(object);
        self.notify.invalInode(self.notify.ctx, global, 0, 0) catch {
            stats.dlm_token_kernel_notify_dropped.inc();
            return;
        };
        stats.dlm_token_kernel_invals.inc();
        self.notify.prune(self.notify.ctx, &.{global}) catch {
            stats.dlm_token_kernel_notify_dropped.inc();
            return;
        };
        stats.dlm_token_kernel_prunes.inc();
    }

    /// A recalled name (decision 0003 §AB.2): the parent is a local key
    /// ino; the kernel knows the GLOBAL one.
    pub fn noteName(self: *KernelInvalidator, parent: Object, name: []const u8) void {
        const global = route_table.kernelGlobalOf(parent) orelse return;
        self.notify.invalEntry(self.notify.ctx, global, name);
        stats.dlm_token_kernel_name_invals.inc();
    }

    /// A recall that could not name a changed dentry.
    pub fn noteUnnamed(self: *KernelInvalidator) void {
        self.notify.incEpoch(self.notify.ctx);
        stats.dlm_token_kernel_epochs.inc();
    }
};

// ---------------------------------------------------------------------------
// Tests: the pure arms (the protocol rides `tokens.zig`).
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the unarmed resolver reads every slot locally: two probes, no plane bound, no allocation" {
    const io = testing.io;
    var d = Divert.init(testing.allocator, io, Resolver.unarmed, 1, 0, cache.RecallSink.none, cache.SelfLease.always, 1 << 20);
    defer d.deinit();
    const before = stats.dlm_token_local_reads.load();
    try testing.expectEqual(@as(?cache.Serve, null), try d.serve(io, 2, .{}));
    try testing.expectEqual(before + 1, stats.dlm_token_local_reads.load());
    try testing.expectEqual(@as(?*cache.Reader, null), d.planeOf(io, 7));
}

test "a foreign slot whose holder has no endpoint is refused retryable (never the projection); a dead lessee is never dialled" {
    const io = testing.io;
    const R = struct {
        fn homeOf(_: *anyopaque, slot: u16) Home {
            return if (slot == 3) .{ .foreign = .{ .holder = 9, .identity = 0x9 } } else .{ .dead = .{ .holder = 4 } };
        }
        fn refresh(_: *anyopaque, _: u16, _: u32) Home {
            return .local;
        }
        fn dial(_: *anyopaque, _: ClientId, _: u32) ?Link {
            return null;
        }
    };
    var x: u8 = 0;
    const resolver: Resolver = .{ .ctx = &x, .homeOf = &R.homeOf, .refresh = &R.refresh, .dial = &R.dial };
    var d = Divert.init(testing.allocator, io, resolver, 1, 0, cache.RecallSink.none, cache.SelfLease.always, 1 << 20);
    defer d.deinit();
    const unbound_before = stats.dlm_token_reader_unbound_holders.load();
    // Slot 3's guest key ino.
    const foreign_object = route_table.effectiveIno(inos.makeGlobalIno(5, 3, inos.ROUTING_WIDTH));
    try testing.expectError(error.ReaderUnbound, d.serve(io, foreign_object, .{}));
    try testing.expectEqual(unbound_before + 1, stats.dlm_token_reader_unbound_holders.load());
    try testing.expectEqual(errors.Error.HolderUnreachable, coreError(error.ReaderUnbound));
    const dead_object = route_table.effectiveIno(inos.makeGlobalIno(5, 4, inos.ROUTING_WIDTH));
    try testing.expectError(error.HolderUnreachable, d.serve(io, dead_object, .{}));
}

test "a holder that cannot be reached re-resolves the slot ONCE inside the op (found by the shared hot directory row: C's `ls` of the striped directory answered EAGAIN for every stripe the killed B had held, long after the manager recovered them): a home that changed serves from it; an unchanged one is the typed EAGAIN; a holder with no endpoint re-resolves the same way" {
    const io = testing.io;
    const R = struct {
        refreshes: u32 = 0,
        /// What the manager answers after the recovery (null = B still
        /// holds the slot).
        recovered: bool = true,
        fn homeOf(_: *anyopaque, slot: u16) Home {
            return if (slot == 3) .{ .foreign = .{ .holder = 9, .identity = 0x9 } } else .{ .unreachable_ = .{ .holder = 9 } };
        }
        fn refresh(ctx: *anyopaque, _: u16, _: u32) Home {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            self.refreshes += 1;
            return if (self.recovered) .local else .{ .foreign = .{ .holder = 9, .identity = 0x9 } };
        }
        fn dial(_: *anyopaque, _: ClientId, _: u32) ?Link {
            return .{ .ctx = @ptrFromInt(8), .call = &deadCall };
        }
        /// B's listener is gone: every call is refused.
        fn deadCall(_: *anyopaque, _: verbs.Request) errors.Error!verbs.Reply {
            return error.HolderUnreachable;
        }
    };
    var r: R = .{};
    const resolver: Resolver = .{ .ctx = &r, .homeOf = &R.homeOf, .refresh = &R.refresh, .dial = &R.dial };
    var d = Divert.init(testing.allocator, io, resolver, 1, 0, cache.RecallSink.none, cache.SelfLease.always, 1 << 20);
    defer d.deinit();
    const foreign_object = route_table.effectiveIno(inos.makeGlobalIno(5, 3, inos.ROUTING_WIDTH));
    // The manager recovered the dead holder's slot: the serve reads at
    // the slot's new home (here this mount's own, local).
    const local0 = stats.dlm_token_local_reads.load();
    try testing.expectEqual(@as(?cache.Serve, null), try d.serve(io, foreign_object, .{}));
    try testing.expectEqual(@as(u32, 1), r.refreshes);
    try testing.expectEqual(local0 + 1, stats.dlm_token_local_reads.load());
    // Not yet recovered: the re-resolve names the same dead holder — the
    // typed EAGAIN, one re-resolve, never a loop (the channel is closed
    // now: the re-probe redials, fails, and re-resolves).
    r.recovered = false;
    r.refreshes = 0;
    try testing.expectError(error.TokenChannelStale, d.serve(io, foreign_object, .{}));
    try testing.expectEqual(@as(u32, 1), r.refreshes);
    // No endpoint for the holder: the same single re-resolve.
    r.recovered = true;
    r.refreshes = 0;
    const no_ep_object = route_table.effectiveIno(inos.makeGlobalIno(5, 4, inos.ROUTING_WIDTH));
    try testing.expectEqual(@as(?cache.Serve, null), try d.serve(io, no_ep_object, .{}));
    try testing.expectEqual(@as(u32, 1), r.refreshes);
}

test "the kernel invalidator: INVAL_INODE then PRUNE on the GLOBAL ino; a full sideband is counted, never a failure" {
    const Rec = struct {
        invals: u32 = 0,
        prunes: u32 = 0,
        last: u64 = 0,
        whole: bool = true,
        pruned_one: bool = true,
        full: bool = false,
        entries: u32 = 0,
        entry_parent: u64 = 0,
        entry_ok: bool = false,
        epochs: u32 = 0,
        fn inval(ctx: *anyopaque, ino: u64, off: i64, len: i64) error{QueueFull}!void {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            if (self.full) return error.QueueFull;
            self.whole = self.whole and off == 0 and len == 0;
            self.invals += 1;
            self.last = ino;
        }
        fn prune(ctx: *anyopaque, nodeids: []const u64) error{QueueFull}!void {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            self.pruned_one = self.pruned_one and nodeids.len == 1;
            self.prunes += 1;
        }
        fn entry(ctx: *anyopaque, parent: u64, name: []const u8) void {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            self.entries += 1;
            self.entry_parent = parent;
            self.entry_ok = std.mem.eql(u8, name, "five");
        }
        fn epoch(ctx: *anyopaque) void {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            self.epochs += 1;
        }
    };
    var rec: Rec = .{};
    var k: KernelInvalidator = .{ .notify = .{ .ctx = &rec, .invalInode = &Rec.inval, .prune = &Rec.prune, .invalEntry = &Rec.entry, .incEpoch = &Rec.epoch } };
    const global = inos.makeGlobalIno(7, 2, inos.ROUTING_WIDTH);
    k.noteRecalled(route_table.effectiveIno(global));
    try testing.expectEqual(@as(u32, 1), rec.invals);
    try testing.expectEqual(@as(u32, 1), rec.prunes);
    try testing.expect(rec.whole and rec.pruned_one);
    try testing.expectEqual(global, rec.last);
    const dropped = stats.dlm_token_kernel_notify_dropped.load();
    rec.full = true;
    k.noteRecalled(route_table.effectiveIno(global));
    try testing.expectEqual(dropped + 1, stats.dlm_token_kernel_notify_dropped.load());
    try testing.expectEqual(@as(u32, 1), rec.invals);
    // A recalled name (decision 0003 §AB.2) on the parent's GLOBAL ino; an
    // unnamed recall drops every dentry.
    k.noteName(route_table.effectiveIno(global), "five");
    try testing.expectEqual(@as(u32, 1), rec.entries);
    try testing.expectEqual(global, rec.entry_parent);
    try testing.expect(rec.entry_ok);
    k.noteUnnamed();
    try testing.expectEqual(@as(u32, 1), rec.epochs);
}
