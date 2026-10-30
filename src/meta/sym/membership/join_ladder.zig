//! The join ladder over the WIRE and the N-daemon posture (charter §5.7
//! "a later RW mount JOINS it (`JoinAppender` over the wire presenting its
//! own `(node, mount_slot)` identity; a clean `umount` + remount at
//! another mount point is a same-node successor of the allocation
//! lease)"; §5.5 "Flush ceiling"; `docs/porting/sym_wire_membership_
//! recovery.md` §5–§6): the seven rungs — declaration, bits, membership
//! (an explicit `MW_BIND=off` REFUSES: "a writer that cannot be SEEN
//! cannot be EVICTED"), registrant (the WERO duty split), `JoinAppender`
//! over the wire, `AcquireSlots` over the wire (`M` rotor slots), the
//! planes on ONE listener. The MANAGER's wire face serves the region verbs
//! and the slot verbs (through the plane's `HandlerTable`) with every
//! answer the ONE status table's word; the JOINER's wire (`ManagerWire`)
//! is one session with a `redial` flag any failed verb sets (the next verb
//! re-dials the manager at its CURRENT endpoint — a failover moved it),
//! gauges `joined_wire_*`. The `Joined` reply's grant word — never the
//! page read — builds the RAM grant; the page at `page_addr` supplies the
//! words the wire does not carry (term, generation, the seq
//! continuation). The handover's push rides the renewal carriage: the
//! holder's OFFER lands on the requester's next `Grant`, the requester
//! accepts with `AcquireSlot` over the wire (`RecallForOffer` over a
//! dial-only wire).
//!
//! The same-node successor rule (§5.3): the D0 flock this open holds
//! proves dead exactly ONE same-node holder — the predecessor MANAGER, the
//! identity page 0 carried when this open READ it; a same-node holder
//! whose page is any other is a JOINED appender of this host (alive as
//! readily as dead; its death is the ledger's).
//!
//! The flush-ceiling audit (KD-SYM-10): every dirty leaf flushed within
//! the landing ceiling (1,100 ms); a structural hold of the SMO mutex
//! EXCUSES the measured overlap with the leaf's dirty window — the
//! recovery class capped at the recovery bound, the service class at ONE
//! ceiling (a longer hold is the stall class the consumers must see);
//! `appender_flush_ceiling_overruns` must-stay-0; every cycle's tape
//! rides `.stats`.

const std = @import("std");
const testkit = @import("../../../core/testkit.zig");
const Allocator = std.mem.Allocator;
const counters = @import("../../../core/counters.zig");
const clock = @import("../../../core/clock.zig");
const log = @import("../../../core/log.zig");
const errors = @import("../../../core/errors.zig");
const checkpoint = @import("../../checkpoint.zig");
const node_seq = @import("../../node_seq.zig");
const manager = @import("../manager.zig");
const join = @import("../join.zig");
const page = @import("../appender_page.zig");
const cr = @import("../control_records.zig");
const slots = @import("../slots.zig");
const verbs = @import("../../../wire/verbs.zig");
const service = @import("../../../wire/service.zig");
const screen = @import("../../../wire/screen.zig");
const codec = @import("../../../wire/codec.zig");
const endpoint = @import("../../../wire/endpoint.zig");
const records = @import("../../../data/sym_planes/records.zig");
const alloc_lease = @import("../../../data/sym_planes/alloc_lease.zig");
const shard = @import("shard.zig");
const fencing = @import("fencing.zig");
const park = @import("park_reclaim.zig");

pub const WireIdentity = verbs.WireIdentity;
pub const Identity = page.Identity;

/// The grant runs a `Joined` / `LeaveAppender` frame carries: the
/// manager's reply shape (the page's 4 runs × 4).
pub const GRANT_WIRE_MAX: usize = page.GRANT_RUNS_MAX * 4;

pub const stats = struct {
    pub var joined_wire_verbs: counters.ShardedCounter = .{};
    pub var joined_wire_acquires: counters.ShardedCounter = .{};
    pub var joined_wire_releases: counters.ShardedCounter = .{};
    pub var joined_wire_extent_grants: counters.ShardedCounter = .{};
    pub var joined_wire_extent_returns: counters.ShardedCounter = .{};
    /// A first touch of an unleased slot over the wire.
    pub var joined_wire_reactive_grants: counters.ShardedCounter = .{};
    pub var joined_wire_ring_grows: counters.ShardedCounter = .{};
    pub var joined_wire_failures: counters.ShardedCounter = .{};
    pub var joined_wire_deferrals: counters.ShardedCounter = .{};
    pub var joined_wire_redials: counters.ShardedCounter = .{};
    /// A reply word the joiner's screen refused: must stay 0.
    pub var joined_wire_words_rejected: counters.Tripwire = .init("joined_wire_words_rejected");
    /// The ladder's refusals by rung (1..7 → index 0..6).
    pub var join_ladder_refusals: [Rung.count]counters.ShardedCounter = @splat(.{});
    /// Offers that rode the carriage and were accepted over the wire.
    pub var joined_wire_offers_accepted: counters.ShardedCounter = .{};
    /// A clean leave + remount at another mount point armed as the
    /// predecessor manager's successor (the allocation lease's).
    pub var same_node_successions: counters.ShardedCounter = .{};
    // The flush-ceiling audit's family (the overrun tripwire is
    // `checkpoint.appender_flush_ceiling_overruns`).
    pub var appender_flush_ceiling_recovery_extensions: counters.ShardedCounter = .{};
    pub var appender_flush_ceiling_service_extensions: counters.ShardedCounter = .{};
    /// Exact-sum Σ excluded ns.
    pub var appender_flush_ceiling_excused_ns: counters.ShardedLatencyHistogram = .{};
    pub var appender_flush_ceiling_excused_max_ms: counters.Gauge = .{};
    pub var appender_flush_ceiling_ms: counters.Gauge = .{};
    pub var appender_flush_ceiling_service_cap_ms: counters.Gauge = .{};
};

// ---------------------------------------------------------------------------
// The seven rungs.
// ---------------------------------------------------------------------------

pub const Rung = enum(u8) {
    declaration = 1,
    bits = 2,
    membership = 3,
    registrant = 4,
    join_appender = 5,
    acquire_slots = 6,
    planes = 7,

    pub const count = @typeInfo(Rung).@"enum".field_names.len;

    pub fn index(self: Rung) usize {
        return @backingInt(self) - 1;
    }
};

pub const RungRefusal = union(enum) {
    /// `SQUEEZEFS_SYMMETRIC_META=0` on a stamped volume.
    declaration,
    /// A volume lacks a required incompat bit.
    bits: struct { volume: u16, bit: u8 },
    /// An explicit `SQUEEZEFS_MW_BIND=off` on a writable mount.
    membership_off,
    registrant: fencing.JoinError,
    join: join.JoinError,
    slots: errors.Error,
    /// The planes could not bind (the listener).
    planes: errors.Error,

    pub fn rung(self: RungRefusal) Rung {
        return switch (self) {
            .declaration => .declaration,
            .bits => .bits,
            .membership_off => .membership,
            .registrant => .registrant,
            .join => .join_appender,
            .slots => .acquire_slots,
            .planes => .planes,
        };
    }
};

/// The required incompat set of a symmetric volume (charter §5.5: bit 17
/// = the forest + appender directory).
pub const SYMMETRIC_PLANE_BIT: u8 = 17;

/// Rungs 1–3 are pure decisions over the mount's declared facts.
pub const Declared = struct {
    symmetric_knob_on: bool,
    volume_stamped: bool,
    /// Per volume ordinal: carries the symmetric bit.
    bits: []const bool,
    mw_bind: endpoint.Bind,
};

pub fn declarationRungs(d: Declared) ?RungRefusal {
    if (d.volume_stamped and !d.symmetric_knob_on) return .declaration;
    for (d.bits, 0..) |ok, v| if (!ok) return .{ .bits = .{ .volume = @intCast(v), .bit = SYMMETRIC_PLANE_BIT } };
    if (d.mw_bind == .off) return .membership_off;
    return null;
}

pub fn noteRefusal(r: RungRefusal) void {
    stats.join_ladder_refusals[r.rung().index()].inc();
}

pub fn writeRefusal(w: *std.Io.Writer, r: RungRefusal) std.Io.Writer.Error!void {
    switch (r) {
        .declaration => try w.writeAll("rung 1: SQUEEZEFS_SYMMETRIC_META=0 on a volume stamped with the symmetric plane — once armed, stays armed (`-o ro` is the read posture)"),
        .bits => |b| try w.print("rung 2: volume {d} lacks incompat bit {d} (the forest + appender directory) — `squeezefs volume enable-symmetric` stamps it", .{ b.volume, b.bit }),
        .membership_off => try w.writeAll("rung 3: SQUEEZEFS_MW_BIND=off on a writable mount — a writer that cannot be SEEN cannot be EVICTED, and the S6 eviction is what records a death"),
        .registrant => |e| try w.print("rung 4: the registrant join refused: {s}", .{@errorName(e)}),
        .join => |e| try w.print("rung 5: JoinAppender refused: {s}", .{@errorName(e)}),
        .slots => |e| try w.print("rung 6: AcquireSlots refused: {s}", .{@errorName(e)}),
        .planes => |e| try w.print("rung 7: the planes could not bind: {s}", .{@errorName(e)}),
    }
}

// ---------------------------------------------------------------------------
// The manager's wire face: the region verbs + the slot verbs on the ONE
// listener.
// ---------------------------------------------------------------------------

fn pageIdentity(w: WireIdentity) Identity {
    return .{ .node_token = w.node_token, .mount_slot = w.mount_slot, .writer_id = w.writer_id };
}

fn wireIdentity(i: Identity) WireIdentity {
    return .{ .node_token = i.node_token, .writer_id = i.writer_id, .mount_slot = i.mount_slot, .reserved = 0 };
}

fn classifyVerb(e: manager.VerbError) service.Answer {
    const c = manager.classify(e, null, null);
    return answerOf(c.status, c.reason);
}

/// The manager's status table → the wire's ONE table. A capacity or
/// no-space refusal is an operational outcome (`busy`), never the
/// service's must-stay-0 class.
fn answerOf(status: manager.Status, reason: manager.Reason) service.Answer {
    return switch (status) {
        .ok => unreachable,
        .schema => .{ .status = .{ .status = .schema } },
        .malformed => .{ .status = .{ .status = .malformed } },
        .not_manager => .{ .status = .{ .status = .not_manager } },
        .rejected => service.Answer.rejected(@tagName(reason)),
        .deferred => service.Answer.deferred(@tagName(reason)),
        .refused => switch (reason) {
            .capacity, .no_space => .{ .status = .{ .status = .busy, .err = error.NoSpaceLeft, .reason = @tagName(reason) } },
            .io => service.Answer.refused(error.InputOutput, @tagName(reason)),
            else => service.Answer.refused(error.Unexpected, @tagName(reason)),
        },
    };
}

/// The verbs the manager's wire face serves (the region verbs + the slot
/// verbs); every one carries the volume ordinal it is addressed to.
pub const manager_verbs = [_]verbs.Verb{ .join_appender, .leave_appender, .extent_grant, .return_extents, .grow_ring, .publish_roots, .publish_endpoint, .resolve_endpoint, .acquire_slots, .acquire_slot, .offer_slot, .release_slot, .resolve_slot };

/// The volume ordinal a manager-family request is addressed to (null for
/// any other family).
pub fn requestVolume(payload: verbs.Request) ?u16 {
    return switch (payload) {
        .join_appender => |r| r.volume,
        .leave_appender => |r| r.head.volume,
        .extent_grant => |r| r.volume,
        .return_extents => |r| r.head.volume,
        .grow_ring => |r| r.volume,
        .publish_roots => |r| r.head.volume,
        .publish_endpoint => |r| r.volume,
        .resolve_endpoint => |r| r.volume,
        .acquire_slots => |r| r.volume,
        .acquire_slot => |r| r.volume,
        .offer_slot => |r| r.volume,
        .release_slot => |r| r.head.volume,
        .resolve_slot => |r| r.volume,
        else => null,
    };
}

/// The ledger volume's half of a JoinAppender (charter §5.5: the join is
/// the identity's newer incarnation): a standing death record naming the
/// joining `(node, mount slot)` is retired BEFORE the join writes a page
/// Live — else the next ledger poll, finding the fresh Live page under the
/// record's pair while the member's census join (the wire plane's start,
/// after the open) has not landed, recovers the LIVE joiner's region.
pub const JoinRetire = struct {
    ctx: *anyopaque,
    retire: *const fn (ctx: *anyopaque, identity: Identity) manager.VerbError!void,
};

pub const ManagerHandlers = struct {
    mgr: *manager.Manager,
    /// The plane's table (the slot verbs' executors).
    table: *const manager.HandlerTable,
    set_width: u16,
    /// The volume ordinal this face serves (`ManagerSet`'s dispatch key).
    volume: u16 = 0,
    /// Null until the ledger is up, and on a process that does not manage
    /// the ledger volume (it retires nothing).
    join_retire: ?JoinRetire = null,

    pub fn register(self: *ManagerHandlers, svc: *service.Service) void {
        svc.register(.join_appender, .{ .ctx = self, .serve = &serveJoin });
        svc.register(.leave_appender, .{ .ctx = self, .serve = &serveLeave });
        svc.register(.extent_grant, .{ .ctx = self, .serve = &serveExtentGrant });
        svc.register(.return_extents, .{ .ctx = self, .serve = &serveReturnExtents });
        svc.register(.grow_ring, .{ .ctx = self, .serve = &serveGrowRing });
        svc.register(.publish_roots, .{ .ctx = self, .serve = &servePublishRoots });
        svc.register(.publish_endpoint, .{ .ctx = self, .serve = &servePublishEndpoint });
        svc.register(.resolve_endpoint, .{ .ctx = self, .serve = &serveResolveEndpoint });
        inline for (.{ .acquire_slots, .acquire_slot, .offer_slot, .release_slot, .resolve_slot }) |v| {
            svc.register(v, .{ .ctx = self, .serve = &serveTable });
        }
    }

    /// One request of the manager family served by THIS face (the set
    /// dispatcher's arm).
    pub fn dispatch(self: *ManagerHandlers, req: *const service.Request, scratch: []u8) service.Answer {
        return switch (req.verb) {
            .join_appender => serveJoin(self, req, scratch),
            .leave_appender => serveLeave(self, req, scratch),
            .extent_grant => serveExtentGrant(self, req, scratch),
            .return_extents => serveReturnExtents(self, req, scratch),
            .grow_ring => serveGrowRing(self, req, scratch),
            .publish_roots => servePublishRoots(self, req, scratch),
            .publish_endpoint => servePublishEndpoint(self, req, scratch),
            .resolve_endpoint => serveResolveEndpoint(self, req, scratch),
            .acquire_slots, .acquire_slot, .offer_slot, .release_slot, .resolve_slot => serveTable(self, req, scratch),
            else => .{ .status = .{ .status = .schema } },
        };
    }

    fn self_(ctx: *anyopaque) *ManagerHandlers {
        return @ptrCast(@alignCast(ctx));
    }

    fn volumeOk(self: *const ManagerHandlers, volume: u16) bool {
        return screen.volume(volume, self.set_width).isOk();
    }

    fn serveJoin(ctx: *anyopaque, req: *const service.Request, scratch: []u8) service.Answer {
        const self = self_(ctx);
        const r = req.payload.join_appender;
        if (!self.volumeOk(r.volume)) return .{ .status = .{ .status = .not_manager } };
        switch (screen.identityPeer(r.identity, req.peer, false)) {
            .ok => {},
            .rejected => |why| return service.Answer.rejected(@tagName(why)),
        }
        if (self.join_retire) |jr| jr.retire(jr.ctx, pageIdentity(r.identity)) catch |e| return classifyVerb(e);
        // The kernel's posture before the joiner can write (decision 0003
        // §AB.2).
        if (self.mgr.peer_gate) |g| g.enter(g.ctx);
        const out = self.mgr.joinAppender(.{ .identity = pageIdentity(r.identity), .ring_want_bytes = r.ring_want_bytes, .home_volume = r.volume }) catch |e| return classifyVerb(e);
        // The tails ride the scratch: the segments, then the grant runs.
        const segs: []align(1) verbs.RunWire = @ptrCast(scratch[0 .. out.ring_segments.len * @sizeOf(verbs.RunWire)]);
        for (out.ring_segments.slice(), 0..) |s, i| segs[i] = .{ .start = s.start, .len = s.len };
        const gstart = segs.len * @sizeOf(verbs.RunWire);
        const grant: []align(1) verbs.GrantRunWire = @ptrCast(scratch[gstart .. gstart + out.n_grant * @sizeOf(verbs.GrantRunWire)]);
        for (out.grantRuns(), 0..) |g, i| grant[i] = .{ .start = g.start, .len = g.len, .reserved = 0 };
        return .{ .ok = .{ .join_appender = .{ .head = .{ .appender_id = out.appender_id, .already = @intFromBool(out.already), .n_segments = out.ring_segments.len, .n_grant = out.n_grant, .reserved = 0, .page_addr = out.page_addr, .node_seq_base = out.node_seq_base, .ring_bytes = out.ring_segments.totalBytes() }, .segments = segs, .grant = grant } } };
    }

    fn serveLeave(ctx: *anyopaque, req: *const service.Request, _: []u8) service.Answer {
        const self = self_(ctx);
        const r = req.payload.leave_appender;
        if (!self.volumeOk(r.head.volume)) return .{ .status = .{ .status = .not_manager } };
        switch (screen.identityPeer(r.head.identity, req.peer, true)) {
            .ok => {},
            .rejected => |why| return service.Answer.rejected(@tagName(why)),
        }
        var runs: [GRANT_WIRE_MAX]cr.GrantRun = undefined;
        if (r.items.len > runs.len) return service.Answer.rejected("unclaimed runs past the cap");
        for (r.items, 0..) |it, i| runs[i] = .{ .start = it.start, .len = it.len };
        const out = self.mgr.leaveAppender(.{ .identity = pageIdentity(r.head.identity), .appender_id = r.head.appender_id, .unclaimed = runs[0..r.items.len] }) catch |e| return classifyVerb(e);
        return .{ .ok = .{ .leave_appender = if (out.already) verbs.Ack.replay else verbs.Ack.fresh } };
    }

    fn serveExtentGrant(ctx: *anyopaque, req: *const service.Request, scratch: []u8) service.Answer {
        const self = self_(ctx);
        const r = req.payload.extent_grant;
        if (!self.volumeOk(r.volume)) return .{ .status = .{ .status = .not_manager } };
        const out = self.mgr.extentGrant(.{ .appender_id = r.appender_id, .want = r.want }) catch |e| return classifyVerb(e);
        defer self.mgr.gpa.free(out.runs);
        const need = out.runs.len * @sizeOf(verbs.GrantRunWire);
        if (scratch.len < need or out.runs.len > std.math.maxInt(u16)) return service.Answer.refused(error.ValueTooLarge, "grant runs past the reply cap");
        const tail: []align(1) verbs.GrantRunWire = @ptrCast(scratch[0..need]);
        for (out.runs, 0..) |g, i| tail[i] = .{ .start = g.start, .len = g.len, .reserved = 0 };
        return .{ .ok = .{ .extent_grant = .{ .head = .{ .n = @intCast(out.runs.len), .reserved = @splat(0) }, .items = tail } } };
    }

    fn serveReturnExtents(ctx: *anyopaque, req: *const service.Request, _: []u8) service.Answer {
        const self = self_(ctx);
        const r = req.payload.return_extents;
        if (!self.volumeOk(r.head.volume)) return .{ .status = .{ .status = .not_manager } };
        const total = self.mgr.vol.vt.geometry(self.mgr.vol.ctx).total_extents;
        switch (screen.returnRuns(r.items, total)) {
            .ok => {},
            .rejected => |why| return service.Answer.rejected(@tagName(why)),
        }
        var runs = self.mgr.gpa.alloc(cr.GrantRun, r.items.len) catch return service.Answer.refused(error.OutOfMemory, "runs");
        defer self.mgr.gpa.free(runs);
        for (r.items, 0..) |it, i| runs[i] = .{ .start = it.start, .len = it.len };
        const out = self.mgr.returnExtents(.{ .appender_id = r.head.appender_id, .runs = runs }) catch |e| return classifyVerb(e);
        return .{ .ok = .{ .return_extents = .{ .cleared = out.cleared, .already = out.already } } };
    }

    fn serveGrowRing(ctx: *anyopaque, req: *const service.Request, _: []u8) service.Answer {
        const self = self_(ctx);
        const r = req.payload.grow_ring;
        if (!self.volumeOk(r.volume)) return .{ .status = .{ .status = .not_manager } };
        const out = self.mgr.growRing(.{ .appender_id = r.appender_id, .want_bytes = r.want_bytes }) catch |e| return classifyVerb(e);
        return .{ .ok = .{ .grow_ring = .{ .present = @intFromBool(out.segment != null), .reserved = @splat(0), .segment = if (out.segment) |s| .{ .start = s.start, .len = s.len } else .{ .start = 0, .len = 0 } } } };
    }

    fn servePublishRoots(ctx: *anyopaque, req: *const service.Request, _: []u8) service.Answer {
        const self = self_(ctx);
        const r = req.payload.publish_roots;
        if (!self.volumeOk(r.head.volume)) return .{ .status = .{ .status = .not_manager } };
        var roots = self.mgr.gpa.alloc(manager.SlotRootWord, r.items.len) catch return service.Answer.refused(error.OutOfMemory, "roots");
        defer self.mgr.gpa.free(roots);
        for (r.items, 0..) |it, i| {
            if (!screen.slot(it.slot).isOk()) return service.Answer.rejected("slot");
            roots[i] = .{ .slot = it.slot, .g = it.g, .root = .{ .addr = it.root.addr, .seq = it.root.seq }, .cursor = it.cursor, .slot_tree_extents = it.slot_tree_extents };
        }
        const out = self.mgr.publishRoots(.{ .appender_id = r.head.appender_id, .roots = roots }) catch |e| return classifyVerb(e);
        return .{ .ok = .{ .publish_roots = .{ .published = @intCast(out.published), .already = @intCast(out.already) } } };
    }

    fn servePublishEndpoint(ctx: *anyopaque, req: *const service.Request, _: []u8) service.Answer {
        const self = self_(ctx);
        const r = req.payload.publish_endpoint;
        if (!self.volumeOk(r.volume)) return .{ .status = .{ .status = .not_manager } };
        switch (screen.identityPeer(r.identity, req.peer, true)) {
            .ok => {},
            .rejected => |why| return service.Answer.rejected(@tagName(why)),
        }
        if (!r.endpoint.check() or r.endpoint.isNone()) return service.Answer.rejected("endpoint");
        var ep: manager.Endpoint = .{};
        const bytes = std.mem.asBytes(&r.endpoint);
        @memcpy(ep.bytes[0..bytes.len], bytes);
        ep.len = @intCast(bytes.len);
        const out = self.mgr.publishEndpoint(.{ .identity = pageIdentity(r.identity), .appender_id = r.appender_id, .endpoint = ep, .pr_key = r.pr_key }) catch |e| return classifyVerb(e);
        return .{ .ok = .{ .publish_endpoint = if (out.already) verbs.Ack.replay else verbs.Ack.fresh } };
    }

    fn serveResolveEndpoint(ctx: *anyopaque, req: *const service.Request, _: []u8) service.Answer {
        const self = self_(ctx);
        const r = req.payload.resolve_endpoint;
        if (!self.volumeOk(r.volume)) return .{ .status = .{ .status = .not_manager } };
        const out = self.mgr.resolveEndpoint(.{ .appender_id = r.appender_id }) catch |e| return classifyVerb(e);
        var reply: verbs.EndpointReply = .{ .present = 0, .reserved = @splat(0), .endpoint = .none };
        if (out.endpoint) |ep| {
            if (ep.len != @sizeOf(endpoint.EndpointWire)) return service.Answer.refused(error.Unexpected, "endpoint row");
            reply.present = 1;
            reply.endpoint = std.mem.bytesToValue(endpoint.EndpointWire, ep.bytes[0..@sizeOf(endpoint.EndpointWire)]);
        }
        return .{ .ok = .{ .resolve_endpoint = reply } };
    }

    /// The slot verbs: the plane's `HandlerTable` executes over the raw
    /// payload; its reply bytes are decoded into the typed reply from the
    /// scratch (the service encodes them again under the ONE table).
    fn serveTable(ctx: *anyopaque, req: *const service.Request, scratch: []u8) service.Answer {
        const self = self_(ctx);
        const mverb: manager.Verb = switch (req.verb) {
            .acquire_slots => .acquire_slots,
            .acquire_slot => .acquire_slot,
            .offer_slot => .offer_slot,
            .release_slot => .release_slot,
            .resolve_slot => .resolve_slot,
            else => unreachable,
        };
        const volume: u16 = switch (req.payload) {
            .acquire_slots => |r| r.volume,
            .acquire_slot => |r| r.volume,
            .offer_slot => |r| r.volume,
            .release_slot => |r| r.head.volume,
            .resolve_slot => |r| r.volume,
            else => unreachable,
        };
        if (!self.volumeOk(volume)) return .{ .status = .{ .status = .not_manager } };
        // The raw payload is re-encoded from the decoded request (the
        // service decoded it once; the table decodes it again — bounded).
        var b: codec.Builder = .init(scratch);
        verbs.encodeRequest(req.payload, &b) catch return service.Answer.refused(error.ValueTooLarge, "request");
        const raw_len = b.pos;
        const reply = self.table.dispatch(self.mgr, self.mgr.gpa, mverb, scratch[0..raw_len]);
        defer if (reply.payload.len != 0) self.mgr.gpa.free(reply.payload);
        if (reply.status != .ok) return answerOf(reply.status, reply.reason);
        if (scratch.len < raw_len + reply.payload.len) return service.Answer.refused(error.ValueTooLarge, "reply");
        const copy = scratch[raw_len .. raw_len + reply.payload.len];
        @memcpy(copy, reply.payload);
        const typed = verbs.decodeReply(req.verb, copy) catch return service.Answer.refused(error.Unexpected, "the plane's reply did not decode");
        return .{ .ok = typed };
    }
};

/// Every volume this daemon manages behind ONE listener (spec §5.2 "a
/// node manages the volumes it won; dispatch by the frame's volume"):
/// the manager family's verbs route to the face whose volume the request
/// names; a volume this daemon does not manage answers `not_manager`.
pub const ManagerSet = struct {
    faces: []ManagerHandlers,

    pub fn register(self: *ManagerSet, svc: *service.Service) void {
        for (manager_verbs) |v| svc.register(v, .{ .ctx = self, .serve = &serveAny });
    }

    pub fn unregister(_: *ManagerSet, svc: *service.Service) void {
        for (manager_verbs) |v| svc.unregister(v);
    }

    pub fn faceOf(self: *ManagerSet, volume: u16) ?*ManagerHandlers {
        for (self.faces) |*f| if (f.volume == volume) return f;
        return null;
    }

    fn serveAny(ctx: *anyopaque, req: *const service.Request, scratch: []u8) service.Answer {
        const self: *ManagerSet = @ptrCast(@alignCast(ctx));
        const volume = requestVolume(req.payload) orelse return .{ .status = .{ .status = .schema } };
        const face = self.faceOf(volume) orelse return .{ .status = .{ .status = .not_manager } };
        return face.dispatch(req, scratch);
    }
};

// ---------------------------------------------------------------------------
// The joiner's wire.
// ---------------------------------------------------------------------------

/// The joiner's read of the page the reply names (the words the wire does
/// not carry).
pub const PageReader = struct {
    ctx: *anyopaque,
    read: *const fn (ctx: *anyopaque, offset: u64, out: *[page.PAGE_LEN]u8) error{Io}!void,
};

pub const Redial = struct {
    ctx: ?*anyopaque,
    /// Re-point the link at the manager's CURRENT endpoint.
    redial: *const fn (ctx: ?*anyopaque) errors.Error!shard.Link,
};

pub const ManagerWire = struct {
    gpa: Allocator,
    link: shard.Link,
    volume: u16,
    identity: WireIdentity,
    pages: PageReader,
    redial: ?Redial = null,
    /// Set by any failed verb: the next verb re-dials first.
    redial_pending: bool = false,

    fn call(self: *ManagerWire, req: verbs.Request) errors.Error!verbs.Reply {
        if (self.redial_pending) if (self.redial) |r| {
            self.link = try r.redial(r.ctx);
            self.redial_pending = false;
            stats.joined_wire_redials.inc();
        };
        stats.joined_wire_verbs.inc();
        return self.link.call(self.link.ctx, req) catch |e| {
            switch (e) {
                error.Deferred, error.MembershipPending => stats.joined_wire_deferrals.inc(),
                error.HolderUnreachable, error.ConnectionLost, error.ListenerRefused => {
                    self.redial_pending = true;
                    stats.joined_wire_failures.inc();
                },
                else => stats.joined_wire_failures.inc(),
            }
            return e;
        };
    }

    fn verbError(e: errors.Error) manager.VerbError {
        return switch (e) {
            error.Rejected => error.Rejected,
            error.Deferred, error.MembershipPending => error.Deferred,
            error.Busy, error.NoSpaceLeft => error.Capacity,
            error.NotManager, error.StaleOwnerEra => error.Refused,
            error.OutOfMemory => error.OutOfMemory,
            else => error.Io,
        };
    }

    /// `JoinAppender` over the wire → the manager's reply shape, the
    /// page's words read at `page_addr`.
    pub fn joinAppender(self: *ManagerWire, req: manager.JoinAppender) manager.VerbError!manager.Joined {
        const rep = self.call(.{ .join_appender = .{ .volume = self.volume, .reserved = 0, .reserved2 = 0, .identity = wireIdentity(req.identity), .ring_want_bytes = req.ring_want_bytes } }) catch |e| return verbError(e);
        const j = rep.join_appender;
        if (!j.head.check()) {
            stats.joined_wire_words_rejected.fire("Joined head out of range");
            return error.Rejected;
        }
        var out: manager.Joined = .{ .appender_id = j.head.appender_id, .page_addr = j.head.page_addr, .term = 0, .generation = 0, .head_hint = 0, .ring_segments = .{}, .grant = undefined, .n_grant = 0, .already = j.head.already == 1, .node_seq_base = j.head.node_seq_base };
        for (j.segments) |s| out.ring_segments.push(.{ .start = s.start, .len = s.len }) catch return error.Rejected;
        for (j.grant) |g| {
            if (out.n_grant == out.grant.len) return error.Rejected;
            out.grant[out.n_grant] = .{ .start = g.start, .len = g.len };
            out.n_grant += 1;
        }
        // The page at `page_addr`: term, generation, the seq continuation.
        var img: [page.PAGE_LEN]u8 = undefined;
        self.pages.read(self.pages.ctx, j.head.page_addr, &img) catch return error.Io;
        const p = page.decode(&img) catch return error.Corrupt;
        if (p.appender_id != j.head.appender_id or !p.identity.sameMount(req.identity)) {
            stats.joined_wire_words_rejected.fire("the page at page_addr is not this join's");
            return error.Rejected;
        }
        out.term = p.term;
        out.generation = p.generation;
        out.head_hint = p.head_hint;
        return out;
    }

    pub fn leaveAppender(self: *ManagerWire, req: manager.LeaveAppender) manager.VerbError!manager.Left {
        var items: [GRANT_WIRE_MAX]verbs.GrantRunWire = undefined;
        if (req.unclaimed.len > items.len) return error.Rejected;
        for (req.unclaimed, 0..) |r, i| items[i] = .{ .start = r.start, .len = r.len, .reserved = 0 };
        const rep = self.call(.{ .leave_appender = .{ .head = .{ .volume = self.volume, .n_unclaimed = @intCast(req.unclaimed.len), .appender_id = req.appender_id, .identity = wireIdentity(req.identity), .reserved = 0 }, .items = items[0..req.unclaimed.len] } }) catch |e| return verbError(e);
        return .{ .already = rep.leave_appender.already == 1 };
    }

    /// The join ladder's door over the wire.
    pub fn door(self: *ManagerWire) join.Door {
        return .{ .ctx = self, .join = &doorJoin, .leave = &doorLeave };
    }

    fn doorJoin(ctx: *anyopaque, req: manager.JoinAppender) manager.VerbError!manager.Joined {
        const self: *ManagerWire = @ptrCast(@alignCast(ctx));
        return self.joinAppender(req);
    }

    fn doorLeave(ctx: *anyopaque, req: manager.LeaveAppender) manager.VerbError!manager.Left {
        const self: *ManagerWire = @ptrCast(@alignCast(ctx));
        return self.leaveAppender(req);
    }

    pub const SlotGrant = struct { slot: u16, g: u32, words: slots.lease_table.SlotWords };

    pub const AcquireReply = union(enum) {
        granted: struct { grants: []SlotGrant, already: u16 },
        refused: struct { slot: u16, holder: u32, g: u32 },
    };

    fn decodeAcquire(self: *ManagerWire, rep: verbs.AcquireReply) errors.Error!AcquireReply {
        if (rep.head.kind == 1) return .{ .refused = .{ .slot = rep.head.slot, .holder = rep.head.holder, .g = rep.head.g } };
        const grants = try self.gpa.alloc(SlotGrant, rep.items.len);
        for (rep.items, 0..) |it, i| {
            if (!screen.slot(it.slot).isOk() or it.words.cursor > screen.CURSOR_MAX) {
                self.gpa.free(grants);
                stats.joined_wire_words_rejected.fire("a slot grant's words out of range");
                return error.Rejected;
            }
            grants[i] = .{ .slot = it.slot, .g = it.g, .words = .{ .root = .{ .addr = it.words.root.addr, .seq = it.words.root.seq }, .cursor = it.words.cursor, .extents = it.words.slot_tree_extents, .seq_floor = it.words.seq_floor } };
        }
        return .{ .granted = .{ .grants = grants, .already = rep.head.already } };
    }

    /// Rung 6: `M` rotor slots over the wire (`grants` owned by `gpa`).
    pub fn acquireSlots(self: *ManagerWire, appender_id: u32, want: u16) errors.Error!AcquireReply {
        const rep = try self.call(.{ .acquire_slots = .{ .volume = self.volume, .want = want, .appender_id = appender_id } });
        stats.joined_wire_acquires.inc();
        return self.decodeAcquire(rep.acquire_slots);
    }

    /// A first touch of an unleased slot (or the accept of an offer).
    pub fn acquireSlot(self: *ManagerWire, appender_id: u32, slot: u16) errors.Error!AcquireReply {
        const rep = try self.call(.{ .acquire_slot = .{ .volume = self.volume, .slot = slot, .appender_id = appender_id } });
        stats.joined_wire_reactive_grants.inc();
        return self.decodeAcquire(rep.acquire_slot);
    }

    pub fn releaseSlot(self: *ManagerWire, appender_id: u32, slot: u16, g: u32, words: slots.lease_table.SlotWords, tails: []const verbs.LeafTailWire) errors.Error!bool {
        const rep = try self.call(.{ .release_slot = .{ .head = .{ .volume = self.volume, .slot = slot, .appender_id = appender_id, .g = g, .n_tails = @intCast(tails.len), .words = .{ .root = .{ .addr = words.root.addr, .seq = words.root.seq }, .cursor = words.cursor, .seq_floor = words.seq_floor, .slot_tree_extents = words.extents, .reserved = 0 } }, .items = tails } });
        stats.joined_wire_releases.inc();
        return rep.release_slot.already == 1;
    }

    pub fn publishRoots(self: *ManagerWire, appender_id: u32, roots: []const verbs.SlotRootWire) errors.Error!manager.RootsPublished {
        const rep = try self.call(.{ .publish_roots = .{ .head = .{ .volume = self.volume, .reserved = 0, .appender_id = appender_id, .n = @intCast(roots.len), .reserved2 = 0 }, .items = roots } });
        return .{ .published = rep.publish_roots.published, .already = rep.publish_roots.already };
    }

    pub fn publishEndpoint(self: *ManagerWire, appender_id: u32, ep: endpoint.EndpointWire, pr_key: u64) errors.Error!bool {
        const rep = try self.call(.{ .publish_endpoint = .{ .volume = self.volume, .reserved = 0, .appender_id = appender_id, .identity = self.identity, .pr_key = pr_key, .endpoint = ep, .reserved2 = @splat(0) } });
        return rep.publish_endpoint.already == 1;
    }

    pub fn resolveEndpoint(self: *ManagerWire, appender_id: u32) errors.Error!?endpoint.EndpointWire {
        const rep = try self.call(.{ .resolve_endpoint = .{ .volume = self.volume, .reserved = 0, .appender_id = appender_id } });
        return if (rep.resolve_endpoint.present == 1) rep.resolve_endpoint.endpoint else null;
    }

    pub fn extentGrant(self: *ManagerWire, appender_id: u32, want: u32, out: *std.ArrayList(cr.GrantRun)) errors.Error!void {
        const rep = try self.call(.{ .extent_grant = .{ .volume = self.volume, .reserved = 0, .appender_id = appender_id, .want = want, .reserved2 = 0 } });
        stats.joined_wire_extent_grants.inc();
        for (rep.extent_grant.items) |r| try out.append(self.gpa, .{ .start = r.start, .len = r.len });
    }

    pub fn returnExtents(self: *ManagerWire, appender_id: u32, runs: []const verbs.GrantRunWire) errors.Error!manager.Returned {
        const rep = try self.call(.{ .return_extents = .{ .head = .{ .volume = self.volume, .n_runs = @intCast(runs.len), .appender_id = appender_id, .reserved = 0 }, .items = runs } });
        stats.joined_wire_extent_returns.inc();
        return .{ .cleared = rep.return_extents.cleared, .already = rep.return_extents.already };
    }

    pub fn growRing(self: *ManagerWire, appender_id: u32, want_bytes: u64) errors.Error!?page.Extent {
        const rep = try self.call(.{ .grow_ring = .{ .volume = self.volume, .reserved = 0, .appender_id = appender_id, .want_bytes = want_bytes } });
        stats.joined_wire_ring_grows.inc();
        return if (rep.grow_ring.present == 1) .{ .start = rep.grow_ring.segment.start, .len = rep.grow_ring.segment.len } else null;
    }
};

// ---------------------------------------------------------------------------
// The joiner: rungs 5–7 over the wire (the N-daemon harness's second
// daemon; the daemon's joined door composes the same calls over its
// backend).
// ---------------------------------------------------------------------------

pub const Joiner = struct {
    gpa: Allocator,
    wire: *ManagerWire,
    region: ?join.JoinedRegion = null,
    /// The slots this joiner leases, with their generations.
    leased: std.ArrayList(ManagerWire.SlotGrant) = .empty,
    stamps: fencing.FrameStamps,
    /// The accepted offers (the push's receiving end).
    offers_accepted: u32 = 0,

    pub fn init(gpa: Allocator, wire: *ManagerWire) Joiner {
        return .{ .gpa = gpa, .wire = wire, .stamps = fencing.FrameStamps.init(gpa, 0) };
    }

    pub fn deinit(self: *Joiner) void {
        if (self.region) |*r| r.deinit();
        self.leased.deinit(self.gpa);
        self.stamps.deinit();
    }

    pub fn appenderId(self: *const Joiner) u32 {
        return self.region.?.appenderId();
    }

    /// Rung 5: the region over the wire (the ladder's screens included).
    pub fn joinRegion(self: *Joiner, in: join.JoinInputs) join.JoinError!void {
        self.region = try join.join(self.gpa, self.wire.door(), in);
        self.stamps.me = self.appenderId();
        // The joined door's prime: no leases yet (its first frames follow
        // the leases' install).
        self.stamps.primed = true;
    }

    fn learnGrants(self: *Joiner, grants: []const ManagerWire.SlotGrant) Allocator.Error!void {
        for (grants) |g| {
            try self.leased.append(self.gpa, g);
            try self.stamps.learn(g.slot, g.g);
        }
    }

    /// Rung 6: `M` rotor slots.
    pub fn acquireRotor(self: *Joiner, m: u16) errors.Error!usize {
        const out = try self.wire.acquireSlots(self.appenderId(), m);
        switch (out) {
            .granted => |g| {
                defer self.gpa.free(g.grants);
                try self.learnGrants(g.grants);
                return g.grants.len;
            },
            .refused => return error.SlotMoved,
        }
    }

    pub const Touch = union(enum) { granted: u32, busy: struct { holder: u32, g: u32 } };

    /// A first touch of `slot` over the wire (or the accept of an offer):
    /// `SlotBusy` naming the holder when another appender leases it.
    pub fn firstTouch(self: *Joiner, slot: u16) errors.Error!Touch {
        const out = try self.wire.acquireSlot(self.appenderId(), slot);
        switch (out) {
            .granted => |g| {
                defer self.gpa.free(g.grants);
                try self.learnGrants(g.grants);
                return .{ .granted = if (g.grants.len != 0) g.grants[0].g else 0 };
            },
            .refused => |r| return .{ .busy = .{ .holder = r.holder, .g = r.g } },
        }
    }

    pub fn leases(self: *const Joiner, slot: u16) ?u32 {
        for (self.leased.items) |l| if (l.slot == slot) return l.g;
        return null;
    }

    /// `PublishRoots` of a wire lessee: every leased slot's live words.
    pub fn publishRoots(self: *Joiner, roots: []const verbs.SlotRootWire) errors.Error!manager.RootsPublished {
        for (roots) |r| if (self.leases(r.slot) == null) return error.Rejected;
        return self.wire.publishRoots(self.appenderId(), roots);
    }

    /// The carriage's receiving end: an offer for a slot this joiner
    /// asked for is accepted over the wire.
    pub fn carriageSink(self: *Joiner) shard.CarriageSink {
        return .{ .ctx = self, .offered = &onOffered, .released = &onReleased };
    }

    fn onOffered(ctx: ?*anyopaque, slot: u16, _: u32) void {
        const self: *Joiner = @ptrCast(@alignCast(ctx.?));
        const t = self.firstTouch(slot) catch return;
        if (t == .granted) {
            self.offers_accepted += 1;
            stats.joined_wire_offers_accepted.inc();
        }
    }

    fn onReleased(ctx: ?*anyopaque, slot: u16) void {
        const self: *Joiner = @ptrCast(@alignCast(ctx.?));
        for (self.leased.items, 0..) |l, i| if (l.slot == slot) {
            _ = self.leased.swapRemove(i);
            self.stamps.forget(slot);
            return;
        };
    }

    /// The clean leave: every lease released first, then the region.
    pub fn leave(self: *Joiner) join.JoinError!manager.Left {
        var r = self.region.?;
        const left = try join.leave(self.gpa, self.wire.door(), &r);
        self.region = r;
        return left;
    }
};

// ---------------------------------------------------------------------------
// The same-node successor rule (§5.3).
// ---------------------------------------------------------------------------

/// The D0 proof of `alloc_lease.acquireAndHold`: page 0's identity when
/// THIS open read it.
pub const SameNodeProof = struct {
    page0_at_open: Identity,
    me: Identity,

    pub fn d0Proof(self: *SameNodeProof) alloc_lease.D0Proof {
        return .{ .ctx = self, .provesDead = &provesDead };
    }

    /// Dead iff the holder is this node's predecessor MANAGER — the exact
    /// `(node, mount_slot)` page 0 carried at the open (a prior
    /// incarnation of THIS mount point included: the D0 ladder this open
    /// won proved it dead; the incarnations differ by `writer_id`). Our
    /// own incarnation is never a predecessor; another same-node holder
    /// is a joined appender of this host; a foreign node's is live.
    pub fn provesDead(ctx: ?*anyopaque, holder: records.WriterIdentity) bool {
        const self: *SameNodeProof = @ptrCast(@alignCast(ctx.?));
        if (holder.node_token != self.me.node_token) return false;
        if (holder.eql(.{ .node_token = self.me.node_token, .mount_slot = self.me.mount_slot, .writer_id = self.me.writer_id })) return false;
        const dead = self.page0_at_open.node_token == holder.node_token and self.page0_at_open.mount_slot == holder.mount_slot;
        if (dead) stats.same_node_successions.inc();
        return dead;
    }
};

// ---------------------------------------------------------------------------
// The flush-ceiling audit (KD-SYM-10), the appender side.
// ---------------------------------------------------------------------------

pub const HoldClass = enum(u8) { recovery, service };

pub const Hold = struct { class: HoldClass, start_ns: u64, end_ns: u64 };

pub const CycleTape = struct {
    term_ns: u64,
    excused_ns: u64,
    overruns: u32,
    extensions: u32,
};

pub const FlushCeilingAudit = struct {
    /// The landing ceiling in force (1,100 ms shipped).
    ceiling_ns: u64,
    /// The recovery class's cap: the derived recovery bound.
    recovery_cap_ns: u64,
    holds: std.ArrayList(Hold) = .empty,
    tape: [checkpoint.TERM_HORIZON_CYCLES]CycleTape = @splat(.{ .term_ns = 0, .excused_ns = 0, .overruns = 0, .extensions = 0 }),
    tape_len: usize = 0,
    tape_next: usize = 0,

    pub fn init(ceiling_ms: u64, recovery_bound_ms: u64) FlushCeilingAudit {
        stats.appender_flush_ceiling_ms.set(ceiling_ms);
        stats.appender_flush_ceiling_service_cap_ms.set(ceiling_ms);
        return .{ .ceiling_ns = ceiling_ms * std.time.ns_per_ms, .recovery_cap_ns = recovery_bound_ms * std.time.ns_per_ms };
    }

    pub fn deinit(self: *FlushCeilingAudit, gpa: Allocator) void {
        self.holds.deinit(gpa);
    }

    /// A structural hold of the SMO mutex recorded (the recovery's steps
    /// 4–7; a service hold: a wire appender's grant/release, a transfer's
    /// adoption, a projection refresh, a region's release).
    pub fn noteHold(self: *FlushCeilingAudit, gpa: Allocator, hold: Hold) Allocator.Error!void {
        try self.holds.append(gpa, hold);
    }

    fn capOf(self: *const FlushCeilingAudit, class: HoldClass) u64 {
        return switch (class) {
            .recovery => self.recovery_cap_ns,
            .service => self.ceiling_ns,
        };
    }

    /// The excused ns for a leaf dirty since `since_ns` judged at `now_ns`:
    /// the MEASURED overlap of each hold with the dirty window, per hold
    /// capped at its class's bound; the classes counted for the leaf.
    pub fn excusedFor(self: *const FlushCeilingAudit, since_ns: u64, now_ns: u64) struct { ns: u64, recovery: bool, service: bool } {
        var total: u64 = 0;
        var rec = false;
        var svc = false;
        for (self.holds.items) |h| {
            const lo = @max(h.start_ns, since_ns);
            const hi = @min(h.end_ns, now_ns);
            if (hi <= lo) continue;
            const overlap = @min(hi - lo, self.capOf(h.class));
            total += overlap;
            switch (h.class) {
                .recovery => rec = true,
                .service => svc = true,
            }
        }
        return .{ .ns = total, .recovery = rec, .service = svc };
    }

    /// One cycle's audit over the regions' oldest dirty leaves (`leaves`
    /// carry `since_ns`; the excused term is folded here), the tape row.
    pub fn auditCycle(self: *FlushCeilingAudit, leaves: []checkpoint.DirtyLeafAge, now_ns: u64, term_ns: u64) checkpoint.CeilingAudit {
        var excused_total: u64 = 0;
        var extensions: u32 = 0;
        for (leaves) |*leaf| {
            const ex = self.excusedFor(leaf.since_ns, now_ns);
            leaf.excused_ns = ex.ns;
            if (ex.ns == 0) continue;
            excused_total += ex.ns;
            if (checkpoint.judgeFlushCeiling(leaf.*, now_ns, self.ceiling_ns) == .extended) {
                extensions += 1;
                if (ex.recovery) stats.appender_flush_ceiling_recovery_extensions.inc();
                if (ex.service) stats.appender_flush_ceiling_service_extensions.inc();
            }
        }
        const out = checkpoint.auditFlushCeiling(leaves, now_ns, self.ceiling_ns);
        if (excused_total != 0) {
            stats.appender_flush_ceiling_excused_ns.record(excused_total);
            const ms = excused_total / std.time.ns_per_ms;
            if (ms > stats.appender_flush_ceiling_excused_max_ms.load()) stats.appender_flush_ceiling_excused_max_ms.set(ms);
        }
        self.tape[self.tape_next] = .{ .term_ns = term_ns, .excused_ns = excused_total, .overruns = @intCast(out.overruns), .extensions = extensions };
        self.tape_next = (self.tape_next + 1) % self.tape.len;
        if (self.tape_len < self.tape.len) self.tape_len += 1;
        // Holds that ended before every window the next cycle can judge
        // are retired (a leaf dirtied now cannot overlap them).
        var i: usize = 0;
        while (i < self.holds.items.len) {
            if (self.holds.items[i].end_ns <= now_ns -| self.ceiling_ns -| self.recovery_cap_ns) {
                _ = self.holds.swapRemove(i);
            } else i += 1;
        }
        return out;
    }

    /// The tape, oldest first, for `.stats`.
    pub fn writeTapeJson(self: *const FlushCeilingAudit, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.writeByte('[');
        var i: usize = 0;
        while (i < self.tape_len) : (i += 1) {
            const idx = (self.tape_next + self.tape.len - self.tape_len + i) % self.tape.len;
            const row = self.tape[idx];
            if (i != 0) try w.writeByte(',');
            try w.print("{{\"term_ns\":{d},\"excused_ns\":{d},\"overruns\":{d},\"extensions\":{d}}}", .{ row.term_ns, row.excused_ns, row.overruns, row.extensions });
        }
        try w.writeByte(']');
    }
};

// ---------------------------------------------------------------------------
// Tests: the rungs, the wire face over the loopback hub (two in-process
// daemons), the same-node rule, the audit.
// ---------------------------------------------------------------------------

const testing = std.testing;
const builtin = @import("builtin");
const loopback = @import("../../../wire/loopback.zig");
const auth = @import("../../../wire/auth.zig");
const acquire = @import("../slots/acquire.zig");
const listener = if (builtin.os.tag == .linux) @import("../../../wire/listener.zig") else struct {};
const dialer = if (builtin.os.tag == .linux) @import("../../../wire/dialer.zig") else struct {};

test "rungs 1–3 are pure: a stamped volume with the knob off refuses at 1; a missing bit names the volume and bit 17 at 2; MW_BIND=off refuses at 3; the refusal counters are per rung" {
    try testing.expect(declarationRungs(.{ .symmetric_knob_on = true, .volume_stamped = true, .bits = &.{ true, true }, .mw_bind = .auto }) == null);
    const r1 = declarationRungs(.{ .symmetric_knob_on = false, .volume_stamped = true, .bits = &.{true}, .mw_bind = .auto }).?;
    try testing.expectEqual(Rung.declaration, r1.rung());
    try testing.expect(declarationRungs(.{ .symmetric_knob_on = false, .volume_stamped = false, .bits = &.{true}, .mw_bind = .auto }) == null);
    const r2 = declarationRungs(.{ .symmetric_knob_on = true, .volume_stamped = true, .bits = &.{ true, false }, .mw_bind = .auto }).?;
    try testing.expectEqual(@as(u16, 1), r2.bits.volume);
    try testing.expectEqual(@as(u8, 17), r2.bits.bit);
    const r3 = declarationRungs(.{ .symmetric_knob_on = true, .volume_stamped = true, .bits = &.{true}, .mw_bind = .off }).?;
    try testing.expectEqual(Rung.membership, r3.rung());
    const before = stats.join_ladder_refusals[2].load();
    noteRefusal(r3);
    try testing.expectEqual(before + 1, stats.join_ladder_refusals[2].load());
    var buf: [256]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeRefusal(&w, r3);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "cannot be EVICTED") != null);
    try testing.expectEqual(@as(usize, 7), Rung.count);
}

fn identityOf(n: u32) WireIdentity {
    return .{ .node_token = 0x7000 + n, .writer_id = 0x8000 + n, .mount_slot = n, .reserved = 0 };
}

/// Daemon A: the manager over the acquire plane's RAM volume, its owner
/// shard, its wire face; daemon B: the joiner over the hub.
const TwoDaemons = struct {
    gpa: Allocator,
    hub: loopback.LoopbackHub,
    plane: *acquire.TestPlane,
    svc_a: service.Service = .init,
    svc_b: service.Service = .init,
    handlers_table: manager.HandlerTable = .{},
    mh: ManagerHandlers,
    clocks: shard.LeaseClocks,
    owner: shard.Owner,
    osvc: shard.OwnerService,
    conn_b_to_a: loopback.Conn,

    const secret: auth.Secret = @splat(0x4D);

    fn init(gpa: Allocator) !*TwoDaemons {
        const self = try gpa.create(TwoDaemons);
        errdefer gpa.destroy(self);
        self.gpa = gpa;
        self.hub = loopback.LoopbackHub.init(gpa, testing.io);
        self.plane = try acquire.TestPlane.init(gpa);
        self.plane.own_n = 1;
        self.svc_a = .init;
        self.svc_b = .init;
        // The plane's own slot-verb handlers (the manager's table).
        self.handlers_table = .{};
        acquire.Verbs.bindHandlers(&self.handlers_table);
        self.mh = .{ .mgr = &self.plane.mgr, .table = &self.handlers_table, .set_width = 1 };
        self.mh.register(&self.svc_a);
        self.clocks = try shard.LeaseClocks.derive(45_000, 0, 100);
        self.owner = try shard.Owner.arm(gpa, self.clocks, 0, 1, 0, &.{}, 0);
        self.osvc = .{ .owner = &self.owner, .now = &manualNow };
        self.osvc.register(&self.svc_a);
        _ = try self.hub.addDaemon(&self.svc_a, secret, identityOf(0));
        _ = try self.hub.addDaemon(&self.svc_b, secret, identityOf(1));
        self.conn_b_to_a = try self.hub.connect(1, 0);
        return self;
    }

    fn deinit(self: *TwoDaemons) void {
        self.conn_b_to_a.deinit();
        self.owner.deinit();
        self.plane.deinit();
        self.hub.deinit();
        self.gpa.destroy(self);
    }

    fn linkCall(ctx: *anyopaque, req: verbs.Request) errors.Error!verbs.Reply {
        const c: *loopback.Conn = @ptrCast(@alignCast(ctx));
        return c.call(req);
    }

    fn link(self: *TwoDaemons) shard.Link {
        return .{ .ctx = &self.conn_b_to_a, .call = &linkCall };
    }

    fn readPage(ctx: *anyopaque, offset: u64, out: *[page.PAGE_LEN]u8) error{Io}!void {
        const self: *TwoDaemons = @ptrCast(@alignCast(ctx));
        const v = self.plane.tv.volume();
        return v.vt.readPage(v.ctx, offset, out);
    }

    fn pages(self: *TwoDaemons) PageReader {
        return .{ .ctx = self, .read = &readPage };
    }
};

var manual_now_ms: u64 = 0;
fn manualNow(_: ?*anyopaque) u64 {
    return manual_now_ms;
}

const Poison = struct {
    fn poison(_: ?*anyopaque, _: []const u8) void {}
    fn sink() park.PoisonSink {
        return .{ .ctx = null, .poison = &poison };
    }
};

test "two in-process daemons over the hub: B joins A's volume over the wire (the page's words read at page_addr, the grant from the reply), acquires M rotor slots, first-touches an unleased slot, a slot A leases refuses SlotBusy naming A, publishes its roots, publishes its endpoint; A's offer rides B's renewal grant and B accepts it over the wire (the handover with the push); B leaves cleanly" {
    const gpa = testing.allocator;
    const d = try TwoDaemons.init(gpa);
    defer d.deinit();
    manual_now_ms = 1000;
    // A (appender 0) leases slots 1 and 2 (its rotor).
    const a_grant = try d.plane.verbs.acquireSlots(gpa, .{ .appender_id = 0, .explicit = &.{ 1, 2 }, .rotor = true });
    a_grant.deinit(gpa);
    var wire: ManagerWire = .{ .gpa = gpa, .link = d.link(), .volume = 0, .identity = identityOf(1), .pages = d.pages() };
    var b = Joiner.init(gpa, &wire);
    defer b.deinit();
    const verbs0 = stats.joined_wire_verbs.load();
    // Rung 5.
    const ident = pageIdentity(identityOf(1));
    try b.joinRegion(.{ .identity = ident, .volume_base = d.plane.tv.g.volume_base, .grain = 4096, .ring0_head = d.plane.tv.ring0_head });
    try testing.expectEqual(@as(u32, 1), b.appenderId());
    try testing.expect(!b.region.?.already);
    try testing.expectEqual(@as(u64, 1), b.region.?.core.page.term);
    try testing.expect(b.region.?.grant.unclaimedCount() > 0);
    try testing.expect(b.region.?.grant.closureHolds());
    // A wrong volume ordinal is not_manager at the manager's face.
    wire.volume = 3;
    try testing.expectError(error.NotManager, wire.acquireSlots(1, 1));
    wire.volume = 0;
    // Rung 6: M = 2 rotor slots (the plane's candidates 1..8 minus A's);
    // B is a WIRE lessee: never one of A's own regions.
    try testing.expectEqual(@as(usize, 2), try b.acquireRotor(2));
    try testing.expectEqual(@as(usize, 2), b.leased.items.len);
    for (b.leased.items) |l| {
        try testing.expect(d.plane.table.view(l.slot).leasedBy(1));
        try testing.expectEqual(l.g, d.plane.table.view(l.slot).g);
        try testing.expectEqual(fencing.FrameStamp{ .appender_id = 1, .g = l.g }, b.stamps.stampFor(l.slot, d.plane.table));
    }
    // First touch of an unleased slot over the wire; a slot A holds is SlotBusy naming A.
    const t7 = try b.firstTouch(7);
    try testing.expectEqual(@as(u32, 1), t7.granted);
    try testing.expect(d.plane.table.view(7).leasedBy(1));
    const busy = try b.firstTouch(1);
    try testing.expectEqual(@as(u32, 0), busy.busy.holder);
    try testing.expect(d.plane.table.view(1).leasedBy(0));
    // PublishRoots of a wire lessee: the words land in tree 0.
    const g7 = b.leases(7).?;
    const pub_out = try b.publishRoots(&.{.{ .root = .{ .addr = 0, .seq = 0 }, .cursor = 1 << 16, .slot_tree_extents = 0, .g = g7, .slot = 7, .reserved = @splat(0) }});
    try testing.expectEqual(@as(u32, 1), pub_out.published);
    try testing.expectError(error.Rejected, b.publishRoots(&.{.{ .root = .{ .addr = 0, .seq = 0 }, .cursor = 1, .slot_tree_extents = 0, .g = 1, .slot = 1, .reserved = @splat(0) }}));
    // The endpoint published and resolved back.
    const ep = endpoint.EndpointWire.fromIp(.{ .ip4 = .loopback(4242) });
    try testing.expect(!try wire.publishEndpoint(1, ep, 0x77));
    try testing.expect(try wire.publishEndpoint(1, ep, 0x77));
    try testing.expect((try wire.resolveEndpoint(1)).?.eql(ep));
    try testing.expect((try wire.resolveEndpoint(9)) == null);
    // Rung 7: B is a member of A's shard; the carriage is bound.
    var member: shard.Member = .{ .gpa = gpa, .id = shard.writerMemberId(identityOf(1)), .role = .writer, .link = d.link(), .pid = 9, .boot = @splat(1), .endpoint = ep, .pr_key = 0x77, .mount = "/mnt/b", .poison = Poison.sink(), .carriage = b.carriageSink() };
    try testing.expectEqual(shard.Member.JoinOutcome.granted, member.join(1000, null));
    try testing.expectEqual(shard.Liveness.live, d.owner.liveness(member.id, 1000));
    // The handover with the push: A offers slot 2 to B; the offer rides B's
    // next renewal grant; B accepts over the wire; the transfer lands.
    const expires = clock.monotonicNs() + 60 * std.time.ns_per_s;
    try testing.expectEqual(acquire.Verbs.OfferResult.offered, try d.plane.verbs.offerSlot(0, 2, 1, expires));
    try d.owner.pushOffer(member.id, 2, 0);
    manual_now_ms = 11_100;
    const handovers0 = d.plane.transfers;
    try testing.expectEqual(shard.TickVerdict.renewed, member.tick(11_100, 0));
    try testing.expectEqual(@as(u32, 1), b.offers_accepted);
    try testing.expectEqual(handovers0 + 1, d.plane.transfers);
    try testing.expect(d.plane.table.view(2).leasedBy(1));
    try testing.expect(b.leases(2) != null);
    try testing.expect(stats.joined_wire_verbs.load() > verbs0 + 8);
    // The clean leave: every lease back, then the region.
    for (b.leased.items) |l| {
        // The words a lessee releases with floor tree 0's record (the
        // published cursor of slot 7 included).
        const rec = cr.SlotStateWire.decode(d.plane.tv.get(cr.slotStateKey(l.slot).slice()).?) catch unreachable;
        try testing.expect(!try wire.releaseSlot(1, l.slot, l.g, .{ .root = rec.root, .cursor = rec.cursor + 1, .extents = rec.slot_tree_extents, .seq_floor = rec.seq_floor + 1 }, &.{}));
        try testing.expectEqual(slots.lease_table.State.unleased, d.plane.table.view(l.slot).state);
    }
    const left = try b.leave();
    try testing.expect(!left.already);
    try testing.expect((try b.leave()).already);
    try member.leave();
}

test "a wire word the joiner's screen refuses (an out-of-range slot grant) is the must-stay-0 tripwire and installs nothing; a transport failure sets the redial flag and the next verb re-dials" {
    const gpa = testing.allocator;
    const Fake = struct {
        reply_bad: bool = false,
        fail: bool = false,
        calls: u32 = 0,
        fn call(ctx: *anyopaque, req: verbs.Request) errors.Error!verbs.Reply {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            self.calls += 1;
            if (self.fail) return error.HolderUnreachable;
            return switch (req) {
                .acquire_slot => .{ .acquire_slot = .{ .head = .{ .kind = 0, .reserved = 0, .n = 1, .already = 0, .slot = 0, .holder = 0, .g = 0 }, .items = &bad_items } },
                else => error.NotManager,
            };
        }
        const bad_items = [_]verbs.SlotGrantWire{.{ .words = .{ .root = .{ .addr = 0, .seq = 0 }, .cursor = std.math.maxInt(u64), .seq_floor = 0, .slot_tree_extents = 0, .reserved = 0 }, .g = 1, .slot = 5, .reserved = 0 }};
        var redials: u32 = 0;
        fn redial(ctx: ?*anyopaque) errors.Error!shard.Link {
            redials += 1;
            return .{ .ctx = ctx.?, .call = &call };
        }
    };
    var fake: Fake = .{};
    var marker: u8 = 0;
    _ = &marker;
    var wire: ManagerWire = .{ .gpa = gpa, .link = .{ .ctx = &fake, .call = &Fake.call }, .volume = 0, .identity = identityOf(2), .pages = .{ .ctx = &marker, .read = &struct {
        fn r(_: *anyopaque, _: u64, _: *[page.PAGE_LEN]u8) error{Io}!void {
            return error.Io;
        }
    }.r }, .redial = .{ .ctx = &fake, .redial = &Fake.redial } };
    const tw0 = stats.joined_wire_words_rejected.load();
    stats.joined_wire_words_rejected.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.Rejected, wire.acquireSlot(1, 5));
    try log.testing.expectAllFired();
    try testing.expectEqual(tw0 + 1, stats.joined_wire_words_rejected.load());
    fake.fail = true;
    const failures0 = stats.joined_wire_failures.load();
    try testing.expectError(error.HolderUnreachable, wire.acquireSlot(1, 5));
    try testing.expectEqual(failures0 + 1, stats.joined_wire_failures.load());
    try testing.expect(wire.redial_pending);
    fake.fail = false;
    const redials0 = stats.joined_wire_redials.load();
    try testing.expectError(error.Rejected, wire.acquireSlot(1, 5));
    try testing.expectEqual(@as(u32, 1), Fake.redials);
    try testing.expectEqual(redials0 + 1, stats.joined_wire_redials.load());
    try testing.expect(!wire.redial_pending);
}

test "the same-node successor rule: the D0 flock proves dead exactly the predecessor manager page 0 named at the open; a same-node joined appender is never taken over; a foreign node's holder is live; our own identity is never a predecessor" {
    const me: Identity = .{ .node_token = 0xAA, .mount_slot = 2, .writer_id = 0x2 };
    var proof: SameNodeProof = .{ .page0_at_open = .{ .node_token = 0xAA, .mount_slot = 1, .writer_id = 0x1 }, .me = me };
    const d0 = proof.d0Proof();
    const before = stats.same_node_successions.load();
    try testing.expect(d0.provesDead(d0.ctx, .{ .node_token = 0xAA, .mount_slot = 1, .writer_id = 0x1 }));
    try testing.expect(d0.provesDead(d0.ctx, .{ .node_token = 0xAA, .mount_slot = 1, .writer_id = 0x9 }));
    try testing.expectEqual(before + 2, stats.same_node_successions.load());
    try testing.expect(!d0.provesDead(d0.ctx, .{ .node_token = 0xAA, .mount_slot = 5, .writer_id = 0x5 }));
    try testing.expect(!d0.provesDead(d0.ctx, .{ .node_token = 0xBB, .mount_slot = 1, .writer_id = 0x1 }));
    try testing.expect(!d0.provesDead(d0.ctx, .{ .node_token = 0xAA, .mount_slot = 2, .writer_id = 0x2 }));
    // The D0 successor at the predecessor's OWN mount point (the restart
    // of a manager at the same mount): the dead incarnation is proven
    // dead, this one is not.
    var same: SameNodeProof = .{ .page0_at_open = .{ .node_token = 0xAA, .mount_slot = 0, .writer_id = 0x1 }, .me = .{ .node_token = 0xAA, .mount_slot = 0, .writer_id = 0x2 } };
    const d1 = same.d0Proof();
    try testing.expect(d1.provesDead(d1.ctx, .{ .node_token = 0xAA, .mount_slot = 0, .writer_id = 0x1 }));
    try testing.expect(!d1.provesDead(d1.ctx, .{ .node_token = 0xAA, .mount_slot = 0, .writer_id = 0x2 }));
}

test "the flush-ceiling audit: a leaf inside the ceiling passes; past it with no hold overruns (the tripwire); a recovery hold's overlap is excused up to the recovery bound and counted as a recovery extension; a service hold is capped at ONE ceiling so a longer stall still overruns; the tape keeps the last 64 cycles oldest-first" {
    const gpa = testing.allocator;
    const ms = std.time.ns_per_ms;
    var a = FlushCeilingAudit.init(1100, 1312);
    defer a.deinit(gpa);
    try testing.expectEqual(@as(u64, 1100), stats.appender_flush_ceiling_ms.load());
    const since: u64 = 10_000 * ms;
    var inside = [_]checkpoint.DirtyLeafAge{.{ .region = 1, .since_ns = since }};
    var out = a.auditCycle(&inside, since + 1100 * ms, 50 * ms);
    try testing.expectEqual(@as(u64, 0), out.overruns);
    // An overrun: 200 ms past with no hold.
    const tw0 = checkpoint.appender_flush_ceiling_overruns.load();
    checkpoint.appender_flush_ceiling_overruns.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    var late = [_]checkpoint.DirtyLeafAge{.{ .region = 1, .since_ns = since }};
    out = a.auditCycle(&late, since + 1300 * ms, 50 * ms);
    try log.testing.expectAllFired();
    try testing.expectEqual(@as(u64, 1), out.overruns);
    try testing.expectEqual(tw0 + 1, checkpoint.appender_flush_ceiling_overruns.load());
    // A recovery hold covering the lateness: excused, a recovery extension.
    try a.noteHold(gpa, .{ .class = .recovery, .start_ns = since + 100 * ms, .end_ns = since + 400 * ms });
    const rec0 = stats.appender_flush_ceiling_recovery_extensions.load();
    var excused = [_]checkpoint.DirtyLeafAge{.{ .region = 1, .since_ns = since }};
    out = a.auditCycle(&excused, since + 1300 * ms, 50 * ms);
    try testing.expectEqual(@as(u64, 0), out.overruns);
    try testing.expectEqual(@as(u64, 1), out.extensions);
    try testing.expectEqual(rec0 + 1, stats.appender_flush_ceiling_recovery_extensions.load());
    try testing.expectEqual(@as(u64, 300 * ms), excused[0].excused_ns);
    try testing.expectEqual(@as(u64, 300), stats.appender_flush_ceiling_excused_max_ms.load());
    // A service hold longer than one ceiling is capped: 1,500 ms of hold
    // excuses 1,100; a leaf 1,300 ms late (2,400 old) still overruns.
    var b = FlushCeilingAudit.init(1100, 1312);
    defer b.deinit(gpa);
    try b.noteHold(gpa, .{ .class = .service, .start_ns = since, .end_ns = since + 1500 * ms });
    checkpoint.appender_flush_ceiling_overruns.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    var stalled = [_]checkpoint.DirtyLeafAge{.{ .region = 1, .since_ns = since }};
    out = b.auditCycle(&stalled, since + 2400 * ms, 50 * ms);
    try log.testing.expectAllFired();
    try testing.expectEqual(@as(u64, 1), out.overruns);
    try testing.expectEqual(@as(u64, 1100 * ms), stalled[0].excused_ns);
    // A service hold inside one ceiling excuses: 2,100 old with 1,100 excused.
    const svc0 = stats.appender_flush_ceiling_service_extensions.load();
    var svc_ok = [_]checkpoint.DirtyLeafAge{.{ .region = 1, .since_ns = since }};
    out = b.auditCycle(&svc_ok, since + 2100 * ms, 50 * ms);
    try testing.expectEqual(@as(u64, 0), out.overruns);
    try testing.expectEqual(svc0 + 1, stats.appender_flush_ceiling_service_extensions.load());
    // The tape: 70 cycles keep the last 64, oldest first.
    var c = FlushCeilingAudit.init(1100, 1312);
    defer c.deinit(gpa);
    var i: u64 = 0;
    while (i < 70) : (i += 1) {
        var none = [_]checkpoint.DirtyLeafAge{};
        _ = c.auditCycle(&none, since, i * ms);
    }
    try testing.expectEqual(@as(usize, 64), c.tape_len);
    var buf: [8192]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try c.writeTapeJson(&w);
    try testing.expect(std.mem.startsWith(u8, w.buffered(), "[{\"term_ns\":6000000,"));
    try testing.expect(std.mem.endsWith(u8, w.buffered(), "{\"term_ns\":69000000,\"excused_ns\":0,\"overruns\":0,\"extensions\":0}]"));
}

/// A `dialer.Client` as the joiner's link (real TCP on the venue): the
/// reply decoded out of the link's own frame-sized scratch.
pub const TcpLink = struct {
    client: *dialer.Client,
    out: [frame_cap]u8 = undefined,

    const frame_cap = 1 << 16;

    fn call(ctx: *anyopaque, req: verbs.Request) errors.Error!verbs.Reply {
        const self: *TcpLink = @ptrCast(@alignCast(ctx));
        return self.client.callTyped(req, &self.out);
    }

    pub fn link(self: *TcpLink) shard.Link {
        return .{ .ctx = self, .call = &call };
    }
};

test "linux: two in-process daemons over REAL TCP on loopback — B dials A's listener, joins over the wire, acquires its rotor and first-touches a slot, is a member of A's shard; A stops: B's renewals fail quietly until T_self, then B PARKS (never the poison); A's successor listens at a new port, B re-dials it through the redial seam and the reclaim lands in grace" {
    if (builtin.os.tag != .linux) return testkit.skip(@src(), .no_linux, "join_ladder: Linux only");
    const gpa = testing.allocator;
    const d = try TwoDaemons.init(gpa);
    defer d.deinit();
    manual_now_ms = 1000;
    const secret = TwoDaemons.secret;
    const l = try listener.Listener.start(gpa, testing.io, .{ .explicit = try endpoint.parseEndpoint("127.0.0.1:0") }, &d.svc_a, secret, .{ .raw_cpus = 4, .nofile = 1 << 16 });
    var listening = true;
    defer if (listening) l.stop();
    var client = dialer.Client.init(gpa, testing.io, l.advertised, secret, identityOf(1));
    defer client.deinit();
    var tl: TcpLink = .{ .client = &client };
    var wire: ManagerWire = .{ .gpa = gpa, .link = tl.link(), .volume = 0, .identity = identityOf(1), .pages = d.pages() };
    var b = Joiner.init(gpa, &wire);
    defer b.deinit();
    try b.joinRegion(.{ .identity = pageIdentity(identityOf(1)), .volume_base = d.plane.tv.g.volume_base, .grain = 4096, .ring0_head = d.plane.tv.ring0_head });
    try testing.expectEqual(@as(u32, 1), b.appenderId());
    try testing.expectEqual(@as(usize, 2), try b.acquireRotor(2));
    try testing.expectEqual(@as(u32, 1), (try b.firstTouch(7)).granted);
    try testing.expect(d.plane.table.view(7).leasedBy(1));
    var gate = park.ParkGate.arm(testing.io, 0, park.tParkMaxMs(46_500, d.clocks.grace_ms));
    var member: shard.Member = .{ .gpa = gpa, .id = shard.writerMemberId(identityOf(1)), .role = .writer, .link = tl.link(), .pid = 9, .boot = @splat(1), .endpoint = .none, .pr_key = 0, .mount = "/mnt/b", .gate = &gate, .poison = Poison.sink(), .carriage = b.carriageSink() };
    try testing.expectEqual(shard.Member.JoinOutcome.granted, member.join(1000, null));
    manual_now_ms = 11_100;
    try testing.expectEqual(shard.TickVerdict.renewed, member.tick(11_100, 0));
    // A dies: the listener stops; the beats fail quietly until T_self.
    l.stop();
    listening = false;
    try testing.expectEqual(shard.TickVerdict.idle, member.tick(21_100, 0));
    const t_self_at = 11_100 + d.clocks.t_self_ms;
    const parks0 = park.stats.appender_parks.load();
    try testing.expectEqual(shard.TickVerdict.parked, member.tick(t_self_at, 1_000));
    try testing.expectEqual(parks0 + 1, park.stats.appender_parks.load());
    try testing.expect(gate.isParked());
    // The successor arms at term 2 expecting B and listens at a NEW port;
    // B's next beat re-dials it (a failover moved the manager).
    var succ = try shard.Owner.arm(gpa, d.clocks, 0, 2, 1, &.{member.id}, t_self_at + 5_000);
    defer succ.deinit();
    var ssvc: shard.OwnerService = .{ .owner = &succ, .now = &manualNow };
    var svc_s: service.Service = .init;
    ssvc.register(&svc_s);
    const l2 = try listener.Listener.start(gpa, testing.io, .{ .explicit = try endpoint.parseEndpoint("127.0.0.1:0") }, &svc_s, secret, .{ .raw_cpus = 4, .nofile = 1 << 16 });
    defer l2.stop();
    var client2 = dialer.Client.init(gpa, testing.io, l2.advertised, secret, identityOf(1));
    defer client2.deinit();
    var tl2: TcpLink = .{ .client = &client2 };
    member.link = tl2.link();
    manual_now_ms = t_self_at + 5_000;
    const reclaims0 = park.stats.slot_lease_reclaims.load();
    try testing.expectEqual(shard.TickVerdict.rejoined, member.tick(t_self_at + 5_000, 2_000));
    try testing.expectEqual(reclaims0 + 1, park.stats.slot_lease_reclaims.load());
    try testing.expect(!gate.isParked());
    try testing.expectEqual(shard.Liveness.live, succ.liveness(member.id, t_self_at + 5_000));
}
