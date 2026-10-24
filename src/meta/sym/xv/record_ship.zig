//! RECORD-LEVEL MUTATIONS OF A FOREIGN-SLOT FILE — the metanode ship
//! (charter §5.5 "Record-level mutations of a foreign-slot file";
//! `docs/porting/sym_tokens_intents_striping.md` §3): `setattr`,
//! `setxattr`, `removexattr` and the layout publish of an inode whose slot
//! ANOTHER appender leases are each ONE shipped verb to the holder — NOT
//! intents ("only the multi-object namespace ops need the intent"). The
//! ONE resolver every record-level site asks: a slot this mount leases ⇒
//! `Local` (apply here); `Foreign{holder}` ⇒ the ship, with ONE redirect
//! on the slot-moved class (re-resolved at the manager; a second stale
//! answer is the caller's retryable class); an unbound holder ⇒ the typed
//! `HolderUnreachable` (EAGAIN) — never an `EREMOTE` naming an arm that
//! does not exist. `record_ships` counts the LOGICAL verb once.
//!
//! The served side applies under the holder's own lease and commit door
//! in its own ring; its conveyor pass RECALLS the object's tokens (the
//! writer's own included, so the writer re-fetches at its next resolve —
//! exact). Then the holder invalidates its OWN view: (1) its attr-cache
//! entry goes WHOLE ("a served truncate or a backwards `touch` can never
//! win a merge"); (2) its layout entry under the dirty law — `DirtyKept`
//! beside a RECORD verb is legal and counted, beside a served PUBLISH it is
//! the tripwire (two writers under one custody); (3) the KERNEL:
//! `FUSE_NOTIFY_INVAL_INODE` FIRST — attrs-only `(−1, 0)` for a record
//! verb, attrs + every page `(0, 0)` for a publish — then, iff the
//! writeback cache was negotiated AND the kernel speaks uapi ≥ 7.45,
//! `FUSE_NOTIFY_PRUNE` ("the prune is what makes a writeback kernel adopt
//! a PEER mount's size / mtime / ctime"). The publish rides the per-holder
//! CUSTODY lease epoch the `CustodyGrant` established. PORTABLE.

const std = @import("std");
const Allocator = std.mem.Allocator;
const Io = std.Io;
const counters = @import("../../../core/counters.zig");
const errors = @import("../../../core/errors.zig");
const clock = @import("../../../core/clock.zig");
const verbs = @import("../../../wire/verbs.zig");
const service = @import("../../../wire/service.zig");
const key = @import("../../key.zig");
const record = @import("../../record.zig");
const tree = @import("../../tree.zig");
const tx = @import("../../tx.zig");
const economy = @import("../../economy.zig");
const CAPS_XATTR = economy.CAPS_XATTR;
const route_table = @import("../../routing/route_table.zig");
const layout = @import("../../../data/layout.zig");
const token_cache = @import("../tokens/token_cache.zig");
const ops = @import("ops.zig");
const stripe = @import("stripe.zig");

const S = economy.S;
pub const Link = token_cache.Link;
pub const InodeValue = record.InodeValue;

pub const stats = struct {
    /// Logical record verbs that LANDED at a holder (once per verb at its
    /// terminal reply; a redirect re-dispatches the same verb).
    pub var record_ships: counters.ShardedCounter = .{};
    /// Verbs served at this holder.
    pub var record_served: counters.ShardedCounter = .{};
    /// Served access stamps that advanced a record's atime (a non-holder's
    /// READ under the mount's atime posture; charter §4 "Access time").
    pub var record_atime_touches: counters.ShardedCounter = .{};
    /// Served write size claims (`SETATTR_SIZE_CLAIM`: the size raised to
    /// `max(size, claim)`, no size path run).
    pub var record_size_claims: counters.ShardedCounter = .{};
    /// Served claims refused: made under an older size epoch than a size
    /// set that landed here since (`TruncateFace.stale_claim`).
    pub var record_size_claims_refused: counters.ShardedCounter = .{};
    /// Shipped setattrs that carried KILLPRIV_V2's bit: the holder stripped
    /// the setid bits (and the caps xattr) in the verb's tx.
    pub var record_killpriv_strips: counters.ShardedCounter = .{};
    /// A served verb refused on its merits (must stay 0; a slot-moved
    /// answer is the requester's re-resolve, not a refusal).
    pub var record_refusals: counters.Tripwire = .init("record_refusals");
    /// A `SlotMoved` answer re-resolved ONCE and re-dispatched.
    pub var record_ship_redirects: counters.ShardedCounter = .{};
    /// A foreign slot whose holder has no bound endpoint: EAGAIN.
    pub var record_unreachable: counters.ShardedCounter = .{};
    /// Layout publishes that landed at a foreign holder / served here /
    /// the REQUESTER's terminal failures and rejections (`ships ≡ served`
    /// fleet-wide at rest).
    pub var foreign_publish_ships: counters.ShardedCounter = .{};
    pub var foreign_publish_served: counters.ShardedCounter = .{};
    pub var foreign_publish_refusals: counters.ShardedCounter = .{};
    /// The holder's own-cache and kernel invalidations after a served verb.
    pub var served_mutation_invals: counters.ShardedCounter = .{};
    pub var served_mutation_prunes: counters.ShardedCounter = .{};
    pub var served_mutation_dirty_kept: counters.ShardedCounter = .{};
    pub var served_mutation_discard_skipped: counters.ShardedCounter = .{};
    /// A served PUBLISH met a dirty local entry: two writers under one
    /// custody (must stay 0; the dirty entry is kept).
    pub var served_publish_over_dirty_entry: counters.Tripwire = .init("served_publish_over_dirty_entry");
    /// The phases: `resolve + rtt ≡ total`.
    pub var record_ship_phase_resolve: counters.ShardedLatencyHistogram = .{};
    pub var record_ship_phase_rtt: counters.ShardedLatencyHistogram = .{};
    pub var record_ship_phase_total: counters.ShardedLatencyHistogram = .{};
};

/// The setattr fields a record verb carries (the economy's request shape).
pub const Setattr = struct {
    mode: ?u32 = null,
    uid: ?u32 = null,
    gid: ?u32 = null,
    size: ?u64 = null,
    atime: ?u64 = null,
    mtime: ?u64 = null,
    ctime: ?u64 = null,
    /// The attribute flags (`record.INODE_FLAGS_ATTR`) to carry: a shipped
    /// FS_IOC_SETFLAGS/FSSETXATTR of a foreign-slot inode.
    flags: ?u32 = null,
    /// KILLPRIV_V2 (`verbs.SETATTR_KILL_SUIDGID`): the holder strips the
    /// setid bits and drops `security.capability` in the same tx.
    kill_suidgid: bool = false,
    /// A READ's access stamp (`verbs.SETATTR_ATIME_TOUCH`; charter §4
    /// "Access time"): `atime` is journaled iff it advances the record,
    /// as a Δatime alone — no ctime, no other field.
    atime_touch: bool = false,
    /// `size` is a non-holder WRITE's claim (`verbs.SETATTR_SIZE_CLAIM`):
    /// the holder raises the record's size to it, never runs its size path.
    size_claim: bool = false,
    /// With `size_claim`: the holder's size epoch the claimer's write read
    /// its size under (0 = none).
    claim_epoch: u64 = 0,

    pub fn toWire(self: Setattr) verbs.SetattrWire {
        var w: verbs.SetattrWire = .none;
        if (self.kill_suidgid) w.valid |= verbs.SETATTR_KILL_SUIDGID;
        if (self.atime_touch) w.valid |= verbs.SETATTR_ATIME_TOUCH;
        if (self.size_claim and self.size != null) {
            w.valid |= verbs.SETATTR_SIZE_CLAIM;
            w.claim_epoch = self.claim_epoch;
        }
        if (self.mode) |v| {
            w.valid |= verbs.SETATTR_MODE;
            w.mode = v;
        }
        if (self.uid) |v| {
            w.valid |= verbs.SETATTR_UID;
            w.uid = v;
        }
        if (self.gid) |v| {
            w.valid |= verbs.SETATTR_GID;
            w.gid = v;
        }
        if (self.size) |v| {
            w.valid |= verbs.SETATTR_SIZE;
            w.size = v;
        }
        if (self.atime) |v| {
            w.valid |= verbs.SETATTR_ATIME;
            w.atime = @bitCast(v);
        }
        if (self.mtime) |v| {
            w.valid |= verbs.SETATTR_MTIME;
            w.mtime = @bitCast(v);
        }
        if (self.ctime) |v| {
            w.valid |= verbs.SETATTR_CTIME;
            w.ctime = @bitCast(v);
        }
        if (self.flags) |v| {
            w.valid |= verbs.SETATTR_FLAGS;
            w.flags = v;
        }
        return w;
    }

    pub fn fromWire(w: verbs.SetattrWire) Setattr {
        return .{
            .mode = if (w.valid & verbs.SETATTR_MODE != 0) w.mode else null,
            .uid = if (w.valid & verbs.SETATTR_UID != 0) w.uid else null,
            .gid = if (w.valid & verbs.SETATTR_GID != 0) w.gid else null,
            .size = if (w.valid & verbs.SETATTR_SIZE != 0) w.size else null,
            .atime = if (w.valid & verbs.SETATTR_ATIME != 0) @bitCast(w.atime) else null,
            .mtime = if (w.valid & verbs.SETATTR_MTIME != 0) @bitCast(w.mtime) else null,
            .ctime = if (w.valid & verbs.SETATTR_CTIME != 0) @bitCast(w.ctime) else null,
            .flags = if (w.valid & verbs.SETATTR_FLAGS != 0) w.flags else null,
            .kill_suidgid = w.valid & verbs.SETATTR_KILL_SUIDGID != 0,
            .atime_touch = w.valid & verbs.SETATTR_ATIME_TOUCH != 0,
            .size_claim = w.valid & verbs.SETATTR_SIZE_CLAIM != 0,
            .claim_epoch = w.claim_epoch,
        };
    }
};

/// One record-level verb.
pub const Call = union(verbs.RecordShipKind) {
    setattr: Setattr,
    setxattr: struct { name: []const u8, value: []const u8 },
    removexattr: struct { name: []const u8 },
    /// The encoded layout record (the `layout` xattr's value) with the
    /// writer's completed DMAs behind every block it newly names.
    publish_layout: struct { value: []const u8, proofs: []align(1) const verbs.DmaProofWire = &.{} },

    pub fn isPublish(self: Call) bool {
        return self == .publish_layout;
    }
};

/// The holder's layout-publish face (the data plane's): stage the
/// shipped layout record AND its `+ref`/`−ref` diff against the record
/// in place into `t` — ONE tx, the refs living in the owner's slot tree
/// (charter §5.6). `prev` is the stored xattr record value (null = a
/// fresh file); the proofs are the writer's, carried over the wire.
pub const PublishFace = struct {
    ctx: *anyopaque,
    stage: *const fn (ctx: *anyopaque, t: *tx.KvTx, key_ino: u64, coll_seq: u8, prev: ?[]const u8, value: []const u8, proofs: []align(1) const verbs.DmaProofWire) PublishError!void,

    pub const PublishError = error{ OutOfMemory, Rejected, ProofMissing };
};

/// What a served verb answered.
pub const Reply = struct {
    /// The folded record after the mutation (null for a removexattr of
    /// an absent name — the caller's ENODATA).
    inode: ?InodeValue,
};

/// The holder's truncate face (decision 0003 §AL.6): a shipped size change
/// runs the filesystem's own size path at the holder — the custody settle,
/// the map clip, a grow's extension zeros landed before the size — never
/// the size alone (a grow applied to the inode alone exposed the EOF
/// block's tail).
pub const TruncateFace = struct {
    ctx: *anyopaque,
    truncate: *const fn (ctx: *anyopaque, global: u64, size: u64) errors.Error!void,
    /// A write's claim made under the holder's size epoch `epoch`: true
    /// iff a size-setting commit landed here since — the claim is REFUSED,
    /// and the face has run its size path at the current size, clipping
    /// any block the claimer's late publish bound past it (the truncate
    /// wins: "write, then truncate"). Null = no claim is judged.
    stale_claim: ?*const fn (ctx: *anyopaque, global: u64, epoch: u64) errors.Error!bool = null,
};

pub const ShipError = error{ OutOfMemory, SlotMoved, HolderUnreachable, Deferred, Io, Rejected, ValueTooLarge, NoAttribute };

pub fn coreError(e: ShipError) errors.Error {
    return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        error.SlotMoved => error.SlotMoved,
        error.HolderUnreachable => error.HolderUnreachable,
        error.Deferred => error.Deferred,
        error.Io => error.InputOutput,
        error.Rejected => error.Rejected,
        error.ValueTooLarge => error.ValueTooLarge,
        error.NoAttribute => error.NoAttribute,
    };
}

/// Where the verb lands.
pub const Home = union(enum) {
    local,
    foreign: struct { holder: u32, link: Link },
};

/// `record_home`: unarmed, inside a served verb (this mount IS the
/// holder), or a slot an own region leases ⇒ `Local`; `Foreign` through
/// the slot's lessee; an unbound holder ⇒ `HolderUnreachable`.
pub fn resolve(resolver: ops.Resolver, global: u64) ShipError!Home {
    return switch (resolver.homeOf(resolver.ctx, route_table.slotOf(global))) {
        .local => .local,
        .foreign => |f| .{ .foreign = .{ .holder = f.holder, .link = f.link } },
        .unreachable_ => blk: {
            stats.record_unreachable.inc();
            break :blk error.HolderUnreachable;
        },
    };
}

/// Ship `call` for `global` once: `null` = apply locally; else the
/// holder's reply. A slot-moved answer re-resolves ONCE and dispatches
/// again (locally when ours now).
pub fn ship(resolver: ops.Resolver, volume: u16, global: u64, call: Call, lease_epoch: u64) ShipError!?Reply {
    const t0 = clock.monotonicNs();
    var home = try resolve(resolver, global);
    const t1 = clock.monotonicNs();
    stats.record_ship_phase_resolve.record(t1 - t0);
    var redirected = false;
    defer stats.record_ship_phase_total.record(clock.monotonicNs() - t0);
    while (true) {
        switch (home) {
            .local => return null,
            .foreign => |f| {
                const ts = clock.monotonicNs();
                defer stats.record_ship_phase_rtt.record(clock.monotonicNs() - ts);
                const rep = f.link.call(f.link.ctx, .{ .record_ship = request(volume, global, call, lease_epoch) }) catch |e| switch (e) {
                    error.SlotMoved, error.StaleHolderView => {
                        if (redirected) return error.SlotMoved;
                        redirected = true;
                        stats.record_ship_redirects.inc();
                        home = switch (resolver.refresh(resolver.ctx, route_table.slotOf(global))) {
                            .local => .local,
                            .foreign => |g| .{ .foreign = .{ .holder = g.holder, .link = g.link } },
                            .unreachable_ => return error.HolderUnreachable,
                        };
                        continue;
                    },
                    error.OutOfMemory => return error.OutOfMemory,
                    error.Deferred => return error.Deferred,
                    error.Rejected => {
                        if (call.isPublish()) stats.foreign_publish_refusals.inc();
                        return error.Rejected;
                    },
                    error.ValueTooLarge => return error.ValueTooLarge,
                    error.NoAttribute => return error.NoAttribute,
                    error.ConnectionLost, error.HolderUnreachable, error.NotManager => {
                        if (call.isPublish()) stats.foreign_publish_refusals.inc();
                        return error.HolderUnreachable;
                    },
                    else => {
                        if (call.isPublish()) stats.foreign_publish_refusals.inc();
                        return error.Io;
                    },
                };
                const r = rep.record_ship;
                if (r.kind == @backingInt(verbs.RecordShipReplyKind.slot_moved)) {
                    if (redirected) return error.SlotMoved;
                    redirected = true;
                    stats.record_ship_redirects.inc();
                    home = switch (resolver.refresh(resolver.ctx, route_table.slotOf(global))) {
                        .local => .local,
                        .foreign => |g| .{ .foreign = .{ .holder = g.holder, .link = g.link } },
                        .unreachable_ => return error.HolderUnreachable,
                    };
                    continue;
                }
                // The LOGICAL verb landed: counted once at the terminal
                // reply (a redirect re-dispatches the same verb; a
                // rejection lands nothing) — `ships ≡ served` at rest.
                stats.record_ships.inc();
                if (call.isPublish()) stats.foreign_publish_ships.inc();
                return .{ .inode = if (r.has_inode != 0) (InodeValue.decode(&std.mem.toBytes(r.inode)) catch return error.Io) else null };
            },
        }
    }
}

fn request(volume: u16, global: u64, call: Call, lease_epoch: u64) verbs.RecordShipReq {
    var head: verbs.RecordShipHead = .{ .volume = volume, .kind = @backingInt(@as(verbs.RecordShipKind, call)), .reserved = 0, .name_len = 0, .n_proofs = 0, .value_len = 0, .reserved3 = 0, .ino = global, .lease_epoch = lease_epoch, .setattr = .none };
    var name: []const u8 = "";
    var value: []const u8 = "";
    var proofs: []align(1) const verbs.DmaProofWire = &.{};
    switch (call) {
        .setattr => |a| head.setattr = a.toWire(),
        .setxattr => |x| {
            name = x.name;
            value = x.value;
        },
        .removexattr => |x| name = x.name,
        .publish_layout => |p| {
            name = layout.LAYOUT_XATTR_NAME;
            value = p.value;
            proofs = p.proofs;
        },
    }
    head.name_len = @intCast(name.len);
    head.value_len = @intCast(value.len);
    head.n_proofs = @intCast(proofs.len);
    return .{ .head = head, .name = name, .value = value, .proofs = proofs };
}

// ---------------------------------------------------------------------------
// The served side.
// ---------------------------------------------------------------------------

/// The session facts the kernel hook needs (0002 Q16: passed at install,
/// never read through globals).
pub const InitCaps = struct {
    writeback_cache: bool,
    /// uapi ≥ 7.45: `FUSE_NOTIFY_PRUNE` exists.
    prune_capable: bool,
};

/// The session's notify writes (the sideband carries them).
pub const KernelHook = struct {
    ctx: *anyopaque,
    invalInode: *const fn (ctx: *anyopaque, ino: u64, off: i64, len: i64) error{QueueFull}!void,
    prune: *const fn (ctx: *anyopaque, nodeids: []const u64) error{QueueFull}!void,
};

pub const DiscardVerdict = enum { discarded, dirty_kept, held_skipped, none };

/// The holder's own caches: the FUSE layer's attr cache and the data
/// plane's layout entry (under the dirty law).
pub const OwnCaches = struct {
    ctx: *anyopaque,
    dropAttr: *const fn (ctx: *anyopaque, global: u64) void,
    discardLayout: *const fn (ctx: *anyopaque, global: u64) DiscardVerdict,
};

pub const ServedSink = struct {
    caches: ?OwnCaches = null,
    kernel: ?KernelHook = null,
    caps: InitCaps = .{ .writeback_cache = false, .prune_capable = false },

    /// After a served mutation of `global`: the attr entry whole, the
    /// layout entry under the dirty law, then the kernel.
    pub fn noteServed(self: *const ServedSink, global: u64, publish: bool) void {
        if (self.caches) |c| {
            c.dropAttr(c.ctx, global);
            switch (c.discardLayout(c.ctx, global)) {
                .discarded, .none => {},
                .dirty_kept => {
                    stats.served_mutation_dirty_kept.inc();
                    if (publish) stats.served_publish_over_dirty_entry.fire("a served layout publish met a dirty local entry");
                },
                .held_skipped => stats.served_mutation_discard_skipped.inc(),
            }
        }
        const k = self.kernel orelse return;
        // INVAL_INODE first: attrs-only for a record verb, everything for
        // a publish.
        if (publish) k.invalInode(k.ctx, global, 0, 0) catch return else k.invalInode(k.ctx, global, -1, 0) catch return;
        stats.served_mutation_invals.inc();
        if (self.caps.writeback_cache and self.caps.prune_capable) {
            k.prune(k.ctx, &.{global}) catch return;
            stats.served_mutation_prunes.inc();
        }
    }
};

/// The holder's wire face for `record_ship`, over the plane's store.
pub fn RecordShipService(comptime Store: type) type {
    return struct {
        const Self = @This();
        const Plane = ops.Plane(Store);

        plane: *Plane,
        sink: ServedSink = .{},
        /// The handover judge's feed (every served record verb).
        served_ship: ?ops.ServedShipSink = null,
        /// The data plane's publish face, armed after the plane is open
        /// (the release store here pairs with the acquire load in `apply`:
        /// a publish arriving before the arm meets a typed rejection,
        /// never a torn face); a holder without one (a volume-less set)
        /// rejects every shipped publish.
        publish: std.atomic.Value(?*const PublishFace) = .init(null),
        /// The filesystem's truncate face (armed with the publish face; the
        /// same release/acquire pair). A holder without one rejects every
        /// shipped size change.
        truncate: std.atomic.Value(?*const TruncateFace) = .init(null),

        pub fn armPublish(self: *Self, face: ?*const PublishFace) void {
            self.publish.store(face, .release);
        }

        pub fn armTruncate(self: *Self, face: ?*const TruncateFace) void {
            self.truncate.store(face, .release);
        }

        pub fn register(self: *Self, svc: *service.Service) void {
            svc.register(.record_ship, .{ .ctx = self, .serve = &serve });
        }

        fn serve(ctx: *anyopaque, req: *const service.Request, _: []u8) service.Answer {
            const self: *Self = @ptrCast(@alignCast(ctx));
            const r = req.payload.record_ship;
            if (r.head.volume != self.plane.volume) return service.Answer.rejected("record_ship names a volume this node does not hold");
            const call: Call = switch (@as(verbs.RecordShipKind, @fromBackingInt(r.head.kind))) {
                .setattr => .{ .setattr = Setattr.fromWire(r.head.setattr) },
                .setxattr => .{ .setxattr = .{ .name = r.name, .value = r.value } },
                .removexattr => .{ .removexattr = .{ .name = r.name } },
                .publish_layout => blk: {
                    if (!std.mem.eql(u8, r.name, layout.LAYOUT_XATTR_NAME)) return service.Answer.rejected("a layout publish must name the layout record");
                    break :blk .{ .publish_layout = .{ .value = r.value, .proofs = r.proofs } };
                },
            };
            const rep = self.apply(r.head.ino, call) catch |e| return switch (e) {
                error.SlotMoved => .{ .ok = .{ .record_ship = .{ .kind = @backingInt(verbs.RecordShipReplyKind.slot_moved), .has_inode = 0, .reserved = 0, .holder = 0, .inode = std.mem.zeroes(verbs.InodeValueWire), .reserved2 = @splat(0) } } },
                error.Deferred => service.Answer.deferred("the holder's door deferred the verb"),
                error.OutOfMemory => service.Answer.refused(error.OutOfMemory, "out of memory serving the record verb"),
                error.ValueTooLarge => service.Answer.refused(error.ValueTooLarge, "the record value is past the cap"),
                error.NoAttribute => blk: {
                    // An absent name is the caller's ENODATA, not a refusal.
                    break :blk .{ .ok = .{ .record_ship = .{ .kind = @backingInt(verbs.RecordShipReplyKind.served), .has_inode = 0, .reserved = 0, .holder = 0, .inode = std.mem.zeroes(verbs.InodeValueWire), .reserved2 = @splat(0) } } };
                },
                // The screen's class: a publish without a face or a proof,
                // a command on a plane without the striping face — typed
                // `STATUS_REJECTED`, never the merits tripwire (the
                // requester counts its terminal publish failures).
                error.Rejected => service.Answer.rejected("the holder rejected the record verb (no publish face, a block without its DMA proof, or an unarmed command)"),
                error.Io, error.HolderUnreachable => blk: {
                    stats.record_refusals.fire("a served record verb failed on its merits");
                    break :blk service.Answer.refused(error.InputOutput, "the holder could not apply the record verb");
                },
            };
            if (self.served_ship) |ss| if (req.peer) |p| ss.note(ss.ctx, r.head.volume, route_table.slotOf(r.head.ino), p);
            var w: verbs.RecordShipReply = .{ .kind = @backingInt(verbs.RecordShipReplyKind.served), .has_inode = 0, .reserved = 0, .holder = 0, .inode = std.mem.zeroes(verbs.InodeValueWire), .reserved2 = @splat(0) };
            if (rep.inode) |v| {
                w.has_inode = 1;
                w.inode = std.mem.bytesToValue(verbs.InodeValueWire, &v.encode());
            }
            return .{ .ok = .{ .record_ship = w } };
        }

        /// Apply `call` to `global` at this holder under its door: ONE
        /// entry; the own-cache and kernel invalidation after it. A verb
        /// naming an inode with NO record here is refused (a shipped
        /// publish never plants a layout for a missing record — the
        /// ordering law a reader relies on).
        pub fn apply(self: *Self, global: u64, call: Call) ShipError!Reply {
            if (!self.plane.isLocal(global)) return error.SlotMoved;
            // A size change runs the holder's own size path first (its
            // commit carries the size); the verb's other fields ride the
            // entry below. A WRITE's size claim is no size change: it rides
            // the entry as `max(size, claim)` (below).
            if (call == .setattr and !call.setattr.size_claim) if (call.setattr.size) |sz| {
                const face = self.truncate.load(.acquire) orelse return error.Rejected;
                face.truncate(face.ctx, global, sz) catch |e| return switch (e) {
                    error.OutOfMemory => error.OutOfMemory,
                    error.SlotMoved => error.SlotMoved,
                    else => error.Io,
                };
            };
            // A claim under an older epoch than the holder's last size set
            // is refused (the face clipped at the current size).
            var claim_refused = false;
            if (call == .setattr and call.setattr.size_claim and call.setattr.claim_epoch != 0) {
                const face = self.truncate.load(.acquire) orelse return error.Rejected;
                if (face.stale_claim) |stale| claim_refused = stale(face.ctx, global, call.setattr.claim_epoch) catch |e| return switch (e) {
                    error.OutOfMemory => error.OutOfMemory,
                    error.SlotMoved => error.SlotMoved,
                    else => error.Io,
                };
            }
            const a = &self.plane.apply;
            const k = route_table.effectiveIno(global);
            var v = (a.readInode(global) catch return error.Io) orelse return error.Io;
            // The explicit flip COMMAND shipped to the holder (`setfattr -n
            // user.squeezefs.stripes -v K` from a non-holder): the striping
            // face runs it; the reply is the directory's folded record.
            if (call == .setxattr and std.mem.eql(u8, call.setxattr.name, stripe.COMMAND_XATTR)) {
                const face = self.plane.striping orelse return error.Rejected;
                face.flip(face.ctx, global, stripe.clampExplicitK(call.setxattr.value, self.plane.stripe_k)) catch |e| return switch (e) {
                    error.OutOfMemory => error.OutOfMemory,
                    error.Deferred => error.Deferred,
                    error.SlotMoved, error.StaleHolderView => error.SlotMoved,
                    error.HolderUnreachable => error.HolderUnreachable,
                    error.PathAlreadyExists, error.NotDir, error.InvalidArgument, error.NotSupported => error.Rejected,
                    else => error.Io,
                };
                stats.record_served.inc();
                return .{ .inode = (a.readInode(global) catch return error.Io) orelse v };
            }
            const t = a.store.begin(@src()) catch return error.OutOfMemory;
            var owned = true;
            errdefer if (owned) a.store.abandon(t);
            const now: u64 = @bitCast(clock.realtimeCoarseNs());
            switch (call) {
                .setattr => |s| if (s.atime_touch) {
                    // A non-holder's READ (charter §4 "Access time"): the
                    // stamp advances the record's atime or does nothing —
                    // a Δatime alone, no ctime (a read is no status
                    // change), nothing else of the verb read.
                    const x = s.atime orelse return error.Rejected;
                    if (!economy.laterThan(x, v.atime)) {
                        owned = false;
                        a.store.abandon(t);
                        stats.record_served.inc();
                        return .{ .inode = v };
                    }
                    t.stageDelta(key.TREE_INODES, &key.inodeKey(k), .{ .mask = record.DELTA_ATIME, .fields = .{ .atime = x } }) catch return error.OutOfMemory;
                    v.atime = x;
                    stats.record_atime_touches.inc();
                } else {
                    if (s.mode) |m| v.mode = (v.mode & S.IFMT) | (m & ~@as(u32, S.IFMT));
                    if (s.uid) |u| v.uid = u;
                    if (s.gid) |g| v.gid = g;
                    // THE CLAIM LAW: a non-holder write's bytes and their
                    // binding reached this layout through its publish
                    // before its claim, so the claim raises the size and
                    // touches no byte — the truncate-up path's extension
                    // zeros would land over [size, claim), the claimer's own
                    // bytes (a joiner's `>>` read back zeros on both
                    // mounts); a claim at or below the size moves nothing.
                    if (s.size) |sz| v.size = if (!s.size_claim) sz else if (claim_refused) v.size else @max(v.size, sz);
                    if (s.size_claim) stats.record_size_claims.inc();
                    if (claim_refused) stats.record_size_claims_refused.inc();
                    if (s.atime) |x| v.atime = x;
                    if (s.mtime) |x| v.mtime = x;
                    // The attribute bits alone: STRIPE is the format's.
                    if (s.flags) |f| v.flags = (v.flags & ~record.INODE_FLAGS_ATTR) | (f & record.INODE_FLAGS_ATTR);
                    // KILLPRIV_V2 at the holder: the mode strip and the
                    // caps xattr's Delete ride this tx, as the local
                    // truncate's `killpriv` (the writer's kernel told the
                    // writer; the holder never sees the flag otherwise).
                    if (s.kill_suidgid) {
                        v.mode = economy.killSuidgidMode(v.mode);
                        if (try self.findXattr(k, CAPS_XATTR)) |hit| t.stageDelete(key.TREE_XATTRS, &hit.k) catch return error.OutOfMemory;
                        stats.record_killpriv_strips.inc();
                    }
                    v.ctime = s.ctime orelse now;
                    t.stagePut(key.TREE_INODES, &key.inodeKey(k), &v.encode()) catch return error.OutOfMemory;
                },
                .setxattr => |x| {
                    try self.stageXattr(t, k, x.name, x.value);
                    v.ctime = now;
                    t.stagePut(key.TREE_INODES, &key.inodeKey(k), &v.encode()) catch return error.OutOfMemory;
                },
                .removexattr => |x| {
                    const hit = (try self.findXattr(k, x.name)) orelse {
                        // Served (the answer is the caller's ENODATA).
                        stats.record_served.inc();
                        return error.NoAttribute;
                    };
                    t.stageDelete(key.TREE_XATTRS, &hit.k) catch return error.OutOfMemory;
                    v.ctime = now;
                    t.stagePut(key.TREE_INODES, &key.inodeKey(k), &v.encode()) catch return error.OutOfMemory;
                },
                .publish_layout => |p| {
                    // The layout record AND its `±ref` diff through the data
                    // plane's face (the refs live in this slot tree); the
                    // layout record is the control plane's: no token
                    // carries it beside the object's own.
                    const face = self.publish.load(.acquire) orelse return error.Rejected;
                    var occ: [256]u8 = undefined;
                    var n: usize = 0;
                    const hit = try self.scanXattr(k, layout.LAYOUT_XATTR_NAME, &occ, &n);
                    const prev: ?[]u8 = if (hit) |h| ((a.store.lookup(a.gpa, key.TREE_XATTRS, &h.k) catch return error.Io) orelse return error.Io) else null;
                    defer if (prev) |pv| a.gpa.free(pv);
                    const coll: u8 = if (hit) |h| h.k[h.k.len - 1] else (key.firstFreeCollSeq(occ[0..n]) orelse return error.ValueTooLarge);
                    face.stage(face.ctx, t, k, coll, prev, p.value, p.proofs) catch |e| return switch (e) {
                        error.OutOfMemory => error.OutOfMemory,
                        error.Rejected, error.ProofMissing => error.Rejected,
                    };
                },
            }
            owned = false;
            a.store.commit(t) catch |e| return switch (Store.coreError(e)) {
                error.SlotMoved => error.SlotMoved,
                error.Deferred => error.Deferred,
                error.OutOfMemory => error.OutOfMemory,
                error.ValueTooLarge => error.ValueTooLarge,
                else => error.Io,
            };
            // The reply follows the holder's durability lane: the writer
            // acks its application on this answer (a publish's bytes, a
            // size claim) and cannot barrier this holder's ring itself.
            a.barrier() catch return error.Io;
            stats.record_served.inc();
            if (call.isPublish()) stats.foreign_publish_served.inc();
            self.sink.noteServed(global, call.isPublish());
            return .{ .inode = v };
        }

        pub const XattrHit = struct { k: [key.XATTR_KEY_LEN]u8 };

        /// The xattr record of `name` under KEY ino `k`, if any.
        pub fn findXattr(self: *Self, k: u64, name: []const u8) ShipError!?XattrHit {
            var occ: [256]u8 = undefined;
            var n: usize = 0;
            return self.scanXattr(k, name, &occ, &n);
        }

        fn scanXattr(self: *Self, k: u64, name: []const u8, occ: *[256]u8, n: *usize) ShipError!?XattrHit {
            const a = &self.plane.apply;
            var kvs: std.ArrayList(tree.KeyValue) = .empty;
            defer tree.deinitKeyValues(a.gpa, &kvs);
            const h = key.xattrNameHash56(name, a.store.nameKey());
            const start = key.xattrKey(k, h, 0);
            const end = key.xattrKey(k, h, std.math.maxInt(u8));
            a.store.range(a.gpa, key.TREE_XATTRS, &start, &end, 256, &kvs) catch return error.Io;
            n.* = 0;
            var hit: ?XattrHit = null;
            for (kvs.items) |kv| {
                occ[n.*] = kv.key[kv.key.len - 1];
                n.* += 1;
                const xv = record.XattrValue.decode(kv.value) catch return error.Io;
                if (hit == null and std.mem.eql(u8, xv.name, name)) {
                    var hk: [key.XATTR_KEY_LEN]u8 = undefined;
                    @memcpy(&hk, kv.key[0..key.XATTR_KEY_LEN]);
                    hit = .{ .k = hk };
                }
            }
            return hit;
        }

        fn stageXattr(self: *Self, t: *tx.KvTx, k: u64, name: []const u8, value: []const u8) ShipError!void {
            var occ: [256]u8 = undefined;
            var n: usize = 0;
            const xk = if (try self.scanXattr(k, name, &occ, &n)) |hit| hit.k else blk: {
                const coll = key.firstFreeCollSeq(occ[0..n]) orelse return error.ValueTooLarge;
                break :blk key.xattrKey(k, key.xattrNameHash56(name, self.plane.apply.store.nameKey()), coll);
            };
            const buf = self.plane.apply.gpa.alloc(u8, 1 + name.len + value.len) catch return error.OutOfMemory;
            defer self.plane.apply.gpa.free(buf);
            const enc = (record.XattrValue{ .name = name, .value = value }).encodeInto(buf) catch return error.Rejected;
            t.stagePut(key.TREE_XATTRS, &xk, enc) catch return error.OutOfMemory;
        }
    };
}

// ---------------------------------------------------------------------------
// Tests: the pure arms; the ship rides `two_holders.zig`.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the setattr words round-trip through the wire; the served sink: attrs-only INVAL for a record verb, whole-inode for a publish, the PRUNE iff writeback ∧ uapi ≥ 7.45; a dirty entry beside a publish is the tripwire" {
    const sa: Setattr = .{ .mode = 0o640, .mtime = 123, .size = 9 };
    const back = Setattr.fromWire(sa.toWire());
    try testing.expectEqual(@as(?u32, 0o640), back.mode);
    try testing.expectEqual(@as(?u64, 123), back.mtime);
    try testing.expectEqual(@as(?u64, 9), back.size);
    try testing.expectEqual(@as(?u32, null), back.uid);
    try testing.expectEqual(@as(?u32, null), back.flags);
    // A shipped flag set carries the attribute bits alone (IMMUTABLE here).
    const fl: Setattr = .{ .flags = record.INODE_FLAG_IMMUTABLE };
    try testing.expectEqual(@as(?u32, record.INODE_FLAG_IMMUTABLE), Setattr.fromWire(fl.toWire()).flags);
    try testing.expect(fl.toWire().valid == verbs.SETATTR_FLAGS);
    const Rec = struct {
        invals: u32 = 0,
        prunes: u32 = 0,
        last_off: i64 = 0,
        drops: u32 = 0,
        verdict: DiscardVerdict = .discarded,
        fn inval(ctx: *anyopaque, _: u64, off: i64, _: i64) error{QueueFull}!void {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            self.invals += 1;
            self.last_off = off;
        }
        fn prune(ctx: *anyopaque, _: []const u64) error{QueueFull}!void {
            @as(*@This(), @ptrCast(@alignCast(ctx))).prunes += 1;
        }
        fn drop(ctx: *anyopaque, _: u64) void {
            @as(*@This(), @ptrCast(@alignCast(ctx))).drops += 1;
        }
        fn discard(ctx: *anyopaque, _: u64) DiscardVerdict {
            return @as(*@This(), @ptrCast(@alignCast(ctx))).verdict;
        }
    };
    var rec: Rec = .{};
    var sink: ServedSink = .{ .caches = .{ .ctx = &rec, .dropAttr = &Rec.drop, .discardLayout = &Rec.discard }, .kernel = .{ .ctx = &rec, .invalInode = &Rec.inval, .prune = &Rec.prune }, .caps = .{ .writeback_cache = false, .prune_capable = true } };
    sink.noteServed(9, false);
    try testing.expectEqual(@as(u32, 1), rec.drops);
    try testing.expectEqual(@as(i64, -1), rec.last_off);
    try testing.expectEqual(@as(u32, 0), rec.prunes);
    sink.caps.writeback_cache = true;
    sink.noteServed(9, true);
    try testing.expectEqual(@as(i64, 0), rec.last_off);
    try testing.expectEqual(@as(u32, 1), rec.prunes);
    sink.caps.prune_capable = false;
    sink.noteServed(9, true);
    try testing.expectEqual(@as(u32, 1), rec.prunes);
    try testing.expectEqual(@as(u32, 3), rec.invals);
    // The dirty law.
    rec.verdict = .dirty_kept;
    const kept0 = stats.served_mutation_dirty_kept.load();
    sink.noteServed(9, false);
    try testing.expectEqual(kept0 + 1, stats.served_mutation_dirty_kept.load());
    stats.served_publish_over_dirty_entry.resetLogBudgetForTest();
    @import("../../../core/log.zig").testing.expectErrorLogs(1);
    sink.noteServed(9, true);
    try @import("../../../core/log.zig").testing.expectAllFired();
    rec.verdict = .held_skipped;
    const skipped0 = stats.served_mutation_discard_skipped.load();
    sink.noteServed(9, false);
    try testing.expectEqual(skipped0 + 1, stats.served_mutation_discard_skipped.load());
}
