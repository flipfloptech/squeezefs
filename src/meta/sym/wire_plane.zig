//! The daemon's cluster-wire plane (Linux; charter §5.5 "The manager
//! lease … Verbs (one wire family over the cluster wire)", "Membership is
//! sharded by home volume", "Slot → holder resolution … endpoints
//! published into each writer's claim-set entry"; §5.7 "the D0 winner IS
//! the manager and a later RW mount JOINS it"; `docs/porting/
//! sym_wire_membership_recovery.md` §5.2 "one listener per daemon serving
//! `ManagerSetService` (dispatch by the frame's `volume` ordinal to that
//! volume's service; a node manages the volumes it won)"): what `mount`
//! arms once the set is open and this process holds the manager lease of
//! every volume it opened — the N-daemon posture's MANAGER half.
//!
//! ONE listener on `SQUEEZEFS_MW_BIND` (rung 3 of the join ladder: an
//! explicit `off` REFUSES a writable mount — "a writer that cannot be
//! SEEN cannot be EVICTED"), authenticated by the set's `job:enroll`
//! secret (tree 0 of volume 0; `wire/auth.zig`), serving on ONE
//! `service.Service`:
//!   - the manager family (`ManagerSet`: the region verbs + the slot verbs
//!     of every volume this daemon manages, dispatched by the request's
//!     volume);
//!   - the membership shard (`OwnerService`: join / renew / leave /
//!     census) — the volumes this daemon manages fold into ONE owner at
//!     its home volume, since every member homed on any of them renews at
//!     this very listener (the frame's `volume` is the manager family's
//!     key, membership has none);
//!   - the death ledger's writer verbs (`RecordDeath` / `RecordRecovered`,
//!     volume 0's manager);
//!   - the token holder's face (`token_grant` / `recall_ack` / `release` /
//!     `custody_grant`) over this daemon's trees, its A1½ recall armed on
//!     every volume's conveyor pass;
//!   - the cross-owner step service (`XvStep` / `LookupExact`) and the
//!     set-wide `dir_rename` lease (volume 0's manager).
//!
//! The endpoint is published TWICE: into each managed volume's RAM
//! endpoint table (`PublishEndpoint`'s home, what `ResolveEndpoint`
//! answers) and as the DURABLE `endpoint:{appender 0}` control record of
//! each volume — the word a joiner reads off the device, through the
//! offline tree-0 reader, before it holds any wire (the manager's own
//! entry in the claim set; charter §5.5).
//!
//! The owner shard arms at the manager's term with the predecessor's
//! writer roster (the `member:` rows of tree 0) as the grace window's
//! `expected`; its death sink is the ledger's writer. The cadence tick
//! (the reclaim fiber's) runs the shard's eviction and grace reap and the
//! ledger's projection. The recovery DRIVER over the real backend is not
//! bound here (the poll runs with no volume hooks: it drains parked death
//! records and retires expired ones — `dead_members_acted` stays 0 until
//! the driver's device binding lands; the evidence note owns the gap).
//!
//! Teardown in order: the token hook disarmed on every conveyor (a pass in
//! flight finishes its recall first), the listener stopped (every
//! connection severed, the accept thread joined), the services dropped.

const std = @import("std");
const builtin = @import("builtin");
const Allocator = std.mem.Allocator;
const Io = std.Io;

const counters = @import("../../core/counters.zig");
const clock = @import("../../core/clock.zig");
const log = @import("../../core/log.zig");
const errors = @import("../../core/errors.zig");
const checksum = @import("../../core/checksum.zig");
const key = @import("../key.zig");
const checkpoint = @import("../checkpoint.zig");
const tree = @import("../tree.zig");
const record = @import("../record.zig");
const backend = @import("../backend.zig");
const meta_set = @import("../meta_set.zig");
const claim = @import("../guard/claim.zig");
const manager = @import("manager.zig");
const cr = @import("control_records.zig");
const membership = @import("membership.zig");
const shard = membership.shard;
const death_ledger = membership.death_ledger;
const join_ladder = membership.join_ladder;
const tokens = @import("tokens.zig");
const xv = @import("xv.zig");
const dir_rename = xv.dir_rename;
const alloc_lease = @import("../../data/sym_planes/alloc_lease.zig");
const wire_auth = @import("../../wire/auth.zig");
const listener = @import("../../wire/listener.zig");
const endpoint = @import("../../wire/endpoint.zig");
const service = @import("../../wire/service.zig");
const verbs = @import("../../wire/verbs.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("sym/wire_plane is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

const MetaSet = meta_set.MetaSet;
const KvBackend = backend.KvBackend;

pub const stats = struct {
    /// 1 while the listener is up.
    pub var wire_listener_up: counters.Gauge = .{};
    /// Live connections on the listener (sampled at each render).
    pub var wire_listener_connections: counters.Gauge = .{};
    /// Endpoint records written durably (one per managed volume per arm).
    pub var wire_endpoint_publications: counters.ShardedCounter = .{};
    /// Cadence ticks of the plane (eviction + grace reap + ledger poll).
    pub var wire_plane_ticks: counters.ShardedCounter = .{};
};

/// Why the plane refused to arm (each reported by name at the mount).
pub const StartError = error{
    /// Rung 3: `SQUEEZEFS_MW_BIND=off` on a writable mount.
    MembershipOff,
    /// `SQUEEZEFS_MW_BIND` neither `auto`, `off` nor `addr:port`.
    MalformedBind,
    /// Tree 0 of volume 0 carries no (or an undecodable) `job:enroll`
    /// record: formatted before the record existed — reformat.
    EnrollMissing,
    /// The listener could not bind or start.
    ListenerRefused,
    /// The membership clocks derive unsafe (`T_self ≤ 0`).
    UnsafeClocks,
    /// The endpoint record could not be committed.
    Io,
    OutOfMemory,
};

pub fn startReason(e: StartError) []const u8 {
    return switch (e) {
        error.MembershipOff => "rung 3: SQUEEZEFS_MW_BIND=off on a writable mount — a writer that cannot be SEEN cannot be EVICTED (`-o ro` is the read posture)",
        error.MalformedBind => "SQUEEZEFS_MW_BIND is neither `auto`, `off` nor an `addr:port` / `[v6]:port` literal",
        error.EnrollMissing => "volume 0's control tree carries no decodable job:enroll record — formatted before the cluster wire existed; reformat",
        error.ListenerRefused => "the cluster wire listener could not bind (see the log)",
        error.UnsafeClocks => "the membership lease clocks derive unsafe (T_self ≤ 0) at the T_owner in force",
        error.Io => "the endpoint record could not be committed to tree 0",
        error.OutOfMemory => "out of memory",
    };
}

/// The longest `host:port` text (`[v6]:port` at full width).
pub const ENDPOINT_TEXT_MAX: usize = 64;

/// The token holder's table caps: objects = one per cached foreign inode
/// a reader may hold (the budget law caps the reader at `mem_budget/256`
/// bytes; 64 k objects × ~200 B ≈ 12 MiB on the holder side at the
/// fleet's 3 GiB budget — the reference's `token_table_objects`), clients
/// = the listener's connection cap (every reader is a connection).
pub const TOKEN_OBJECTS_CAP: u32 = 1 << 16;

pub const WirePlane = struct {
    gpa: Allocator,
    io: Io,
    set: *MetaSet,
    svc: service.Service = .init,
    listener: *listener.Listener,
    advertised: endpoint.EndpointWire,
    endpoint_buf: [ENDPOINT_TEXT_MAX]u8 = undefined,
    endpoint_len: usize = 0,
    /// This mount's wire identity (volume 0's appender 0).
    identity: verbs.WireIdentity,
    faces: []join_ladder.ManagerHandlers,
    managers: join_ladder.ManagerSet,
    owner: shard.Owner,
    owner_svc: shard.OwnerService,
    rows: RowStore,
    ledger_store: LedgerStore,
    ledger: death_ledger.Ledger,
    ledger_handlers: death_ledger.LedgerHandlers,
    projection: death_ledger.Projection,
    source: SetRecordSource,
    holder: tokens.table.Holder,
    oracle: OwnerOracle,
    pusher: NoPeers,
    binding: tokens.ConveyorBinding,
    hook: @import("../conveyor.zig").TokenHook,
    holder_svc: tokens.divert.HolderService,
    resolver_ctx: LocalResolver,
    steps: xv.ops.StepService(MetaSet),
    rename_store: RenameStore,
    rename_lease: dir_rename.Lease,
    rename_svc: dir_rename.LeaseService,
    t_owner_ms: u64,
    /// This mount's read divert (charter §5.5 "Every writer is also a
    /// token client of the holders it writes to"): a slot another
    /// appender leases is read under a token or answers the typed EAGAIN
    /// — never off this mount's cache. Bound into the filesystem by
    /// `mount`; its kernel-invalidation half joins once the session is up.
    divert: tokens.Divert,
    sink: RecallSink,

    /// Arm the plane (module doc); null on a set whose volume 0 carries no
    /// appender set (the `--single-writer` class: no wire).
    pub fn start(gpa: Allocator, io: Io, set: *MetaSet, env: *const std.process.Environ.Map, raw_cpus: u32, mem_budget_bytes: u64, xv_plane: *xv.set_bind.SetPlane, read_only: bool) StartError!?*WirePlane {
        const be0 = set.volumes[0];
        const sym0 = be0.sym orelse return null;
        const bind = listener.bindFromEnv(env) catch return error.MalformedBind;
        if (join_ladder.declarationRungs(.{ .symmetric_knob_on = true, .volume_stamped = true, .bits = &.{}, .mw_bind = bind })) |r| {
            join_ladder.noteRefusal(r);
            if (!read_only) return error.MembershipOff;
            return null;
        }
        const secret = readEnrollSecret(gpa, be0) catch |e| return e;

        const self = try gpa.create(WirePlane);
        errdefer gpa.destroy(self);
        const faces = try gpa.alloc(join_ladder.ManagerHandlers, set.volumes.len);
        errdefer gpa.free(faces);
        var n_faces: usize = 0;
        for (set.volumes, 0..) |be, v| {
            const s = be.sym orelse continue;
            faces[n_faces] = .{ .mgr = &s.manager, .table = &s.handlers, .set_width = @intCast(set.volumes.len), .volume = @intCast(v) };
            n_faces += 1;
        }
        const t_owner_ms = shard.tOwnerMsFromEnv(env);
        // The owner derives with RTT 0 (no member to measure yet) and the
        // checkpoint landing ceiling as the revalidation cadence.
        const clocks = shard.LeaseClocks.derive(t_owner_ms, 0, shard.CHECKPOINT_LANDING_CEILING_MS) catch return error.UnsafeClocks;
        const identity: verbs.WireIdentity = .{ .node_token = sym0.identity.node_token, .writer_id = sym0.identity.writer_id, .mount_slot = sym0.identity.mount_slot, .reserved = 0 };
        self.* = .{
            .gpa = gpa,
            .io = io,
            .set = set,
            .listener = undefined,
            .advertised = .none,
            .identity = identity,
            .faces = faces[0..n_faces],
            .managers = .{ .faces = faces[0..n_faces] },
            .owner = undefined,
            .owner_svc = undefined,
            .rows = .{ .plane = self },
            .ledger_store = .{ .plane = self },
            .ledger = undefined,
            .ledger_handlers = undefined,
            .projection = undefined,
            .source = .{ .set = set },
            .holder = undefined,
            .oracle = .{ .plane = self },
            .pusher = .{},
            .binding = undefined,
            .hook = undefined,
            .holder_svc = undefined,
            .resolver_ctx = .{ .set = set },
            .steps = .{ .plane = xv_plane },
            .rename_store = .{ .plane = self },
            .rename_lease = undefined,
            .rename_svc = undefined,
            .t_owner_ms = t_owner_ms,
            .divert = undefined,
            .sink = .{},
        };
        // The manager IS its own membership owner: its self-lease never
        // lapses (`SelfLease.always`); its client id is its member id's.
        self.divert = tokens.Divert.init(gpa, io, self.resolver_ctx.resolver(), clientIdOf(shard.writerMemberId(identity)), 0, self.sink.sink(), .always, tokens.cache.recordsBudgetBytes(mem_budget_bytes));
        // The owner shard at the manager's term, the predecessor's roster
        // as the grace window's expectation.
        var expected: std.ArrayList(shard.MemberId) = .empty;
        defer expected.deinit(gpa);
        self.priorRoster(&expected) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            else => error.Io,
        };
        const term = sym0.manager.term;
        self.owner = shard.Owner.arm(gpa, clocks, 0, term, term -| 1, expected.items, nowMs()) catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            // `term ≤ prior` cannot happen: prior is `term − 1` by construction.
            error.StaleTerm => unreachable,
        };
        errdefer self.owner.deinit();
        self.owner.bindRows(self.rows.store());
        self.ledger = death_ledger.Ledger.init(gpa, self.ledger_store.store(), @intCast(set.volumes.len), identity, t_owner_ms);
        errdefer self.ledger.deinit();
        self.owner.bindDeathSink(self.ledger.deathSink());
        self.owner_svc = .{ .owner = &self.owner, .now = &nowMsFn };
        self.ledger_handlers = .{ .ledger = &self.ledger, .census = &self.owner, .now = &nowMsFn, .is_vol0_manager = &isVol0ManagerFn, .now_ctx = self };
        self.projection = .{ .ledger = &self.ledger, .volumes = &.{}, .vol_tags = &.{} };
        // The token holder over this daemon's trees.
        self.holder = tokens.table.Holder.init(gpa, io, identity.writer_id, self.oracle.oracle(), TOKEN_OBJECTS_CAP, listener.deriveMaxConnections(raw_cpus, listener.raiseNofile())) catch return error.OutOfMemory;
        errdefer self.holder.deinit();
        self.binding = .{ .io = io, .holder = &self.holder, .pusher = self.pusher.pusher() };
        self.hook = self.binding.hook();
        self.holder_svc = .{ .io = io, .holder = &self.holder, .source = self.source.source(), .resolver = self.resolver_ctx.resolver(), .custody = null };
        self.rename_lease = .{ .io = io, .store = self.rename_store.store(), .term = term, .own_id = sym0.region0().appenderId() };
        self.rename_svc = .{ .lease = &self.rename_lease };
        // The ONE service, every family registered, then the listener.
        self.managers.register(&self.svc);
        self.owner_svc.register(&self.svc);
        self.ledger_handlers.register(&self.svc);
        self.holder_svc.register(&self.svc);
        self.steps.register(&self.svc);
        self.rename_svc.register(&self.svc);
        self.listener = listener.Listener.start(gpa, io, bind, &self.svc, secret, .{ .env = env, .raw_cpus = raw_cpus, .nofile = listener.raiseNofile() }) catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            error.BindOff => unreachable,
            else => {
                log.err("cluster wire: the listener could not start ({s})", .{@errorName(e)});
                return error.ListenerRefused;
            },
        };
        errdefer self.listener.stop();
        self.advertised = self.listener.advertised;
        var w: Io.Writer = .fixed(&self.endpoint_buf);
        self.advertised.format(&w) catch {};
        self.endpoint_len = w.buffered().len;
        stats.wire_listener_up.set(1);
        // The endpoint published: RAM table + the durable record, per
        // managed volume.
        try self.publishEndpoints();
        // The recall hook on every volume's pass.
        for (set.volumes) |be| be.cv.armTokens(&self.hook);
        log.info("cluster wire: manager of {d} volume(s) listening at {s}; membership owner term {d} (T_owner {d} ms, {d} expected member(s) in grace)", .{ self.faces.len, self.endpointText(), term, t_owner_ms, expected.items.len });
        return self;
    }

    pub fn stop(self: *WirePlane) void {
        for (self.set.volumes) |be| be.cv.disarmTokens();
        self.listener.stop();
        stats.wire_listener_up.set(0);
        stats.wire_listener_connections.set(0);
        self.divert.deinit();
        self.holder.deinit();
        self.ledger.deinit();
        self.owner.deinit();
        self.gpa.free(self.faces.ptr[0..self.set.volumes.len]);
        self.gpa.destroy(self);
    }

    pub fn endpointText(self: *const WirePlane) []const u8 {
        return self.endpoint_buf[0..self.endpoint_len];
    }

    /// The cadence (the reclaim fiber's tick): the shard's eviction at
    /// `T_owner` (each death to the ledger's writer), the grace reap, the
    /// ledger's projection.
    pub fn tick(self: *WirePlane) void {
        stats.wire_plane_ticks.inc();
        stats.wire_listener_connections.set(self.listener.liveConnections());
        const now = nowMs();
        const evicted = self.owner.expireDue(now);
        const reaped = self.owner.reapGraceDeadline(now, &rosterKey);
        const rep = self.projection.run(now, @intCast(clock.realtimeCoarseNs()));
        if (evicted != 0 or reaped != 0 or rep.deaths != 0 or rep.released != 0 or rep.retired != 0) {
            log.info("cluster wire: tick — {d} evicted at T_owner, {d} reaped at the grace deadline; ledger: {d} death record(s) standing, {d} acted, {d} region(s) released, {d} retired, {d} parked record(s) drained", .{ evicted, reaped, rep.deaths, rep.acted, rep.released, rep.retired, rep.drained });
        }
        if (rep.role_released) log.err("cluster wire: volume 0 unreachable for longer than T_owner — the manager role is released (the mount serves until its fence)", .{});
    }

    /// A `.stats` family: the listener and the shard.
    pub fn writeStatsJson(self: *WirePlane, w: *Io.Writer) Io.Writer.Error!void {
        try w.print("{{\"wire_endpoint\":\"{s}\",\"wire_listener_connections\":{d},\"wire_managed_volumes\":{d},\"membership_members\":{d},\"membership_term\":{d},\"membership_grace_remaining_ms\":{d}}}", .{ self.endpointText(), self.listener.liveConnections(), self.faces.len, self.owner.members.items.len, self.owner.term, self.owner.graceRemainingMs(nowMs()) });
    }

    // ---- the endpoint's two homes ----

    fn publishEndpoints(self: *WirePlane) StartError!void {
        const rec: cr.Endpoint = .{ .family = self.advertised.family, .port = self.advertised.port, .addr = self.advertised.addr, .pr_key = 0 };
        const value = cr.EndpointRecordWire.init(rec).encode();
        var ep: manager.Endpoint = .{};
        const bytes = std.mem.asBytes(&self.advertised);
        @memcpy(ep.bytes[0..bytes.len], bytes);
        ep.len = @intCast(bytes.len);
        for (self.set.volumes) |be| {
            const s = be.sym orelse continue;
            const r0 = s.region0();
            _ = s.manager.publishEndpoint(.{ .identity = r0.identity, .appender_id = r0.appenderId(), .endpoint = ep, .pr_key = 0 }) catch |e| switch (e) {
                error.OutOfMemory => return error.OutOfMemory,
                else => return error.Io,
            };
            const k = cr.endpointKey(r0.appenderId());
            // Idempotent against the durable word: a remount at the same
            // endpoint writes nothing.
            if (be.lookup(self.gpa, key.TREE_CONTROL, k.slice()) catch null) |old| {
                defer self.gpa.free(old);
                if (std.mem.eql(u8, old, &value)) continue;
            }
            controlEntry(s, &.{.{ .put = .{ .key = k.slice(), .value = &value } }}, .parking) catch return error.Io;
            stats.wire_endpoint_publications.inc();
        }
    }

    /// The predecessor's writer roster: every `member:` row of volume 0's
    /// tree 0 (a `-o ro` reader is in no claim set, so never expected).
    fn priorRoster(self: *WirePlane, out: *std.ArrayList(shard.MemberId)) backend.Error!void {
        const r = cr.rangeOf(.member);
        var kvs: std.ArrayList(tree.KeyValue) = .empty;
        defer tree.deinitKeyValues(self.gpa, &kvs);
        try self.set.volumes[0].range(self.gpa, key.TREE_CONTROL, r.start.slice(), r.end.slice(), std.math.maxInt(usize), &kvs);
        for (kvs.items) |kv| {
            const id = cr.identityOfKey(kv.key) orelse continue;
            try out.append(self.gpa, shard.writerMemberId(.{ .node_token = id.node_token, .writer_id = 0, .mount_slot = id.mount_slot, .reserved = 0 }));
        }
    }

    // ---- tree-0 writes: ONE control entry through the manager's volume seam ----

    /// `parking` cycles the checkpoint on a full user window and retries,
    /// bounded by the structural door's cycle cap; `try_` answers the
    /// full window as `WindowFull`.
    fn controlEntry(s: *@import("bind.zig").AppenderSet, recs: []const manager.ControlRecord, adm: alloc_lease.Admission) alloc_lease.StoreError!void {
        var cycles: u32 = 0;
        while (true) {
            s.volume().vt.controlEntry(s.volume().ctx, recs) catch |e| switch (e) {
                error.JournalReserveExhausted => {
                    if (adm == .try_ or cycles == checkpoint.COVER_CYCLES_MAX) return error.WindowFull;
                    cycles += 1;
                    _ = s.volume().vt.cycleCheckpoint(s.volume().ctx) catch return error.Io;
                    continue;
                },
                error.OutOfMemory => return error.OutOfMemory,
                error.ValueTooLarge, error.Io, error.Corrupt => return error.Io,
            };
            return;
        }
    }

    fn set0(self: *WirePlane) *@import("bind.zig").AppenderSet {
        return self.set.volumes[0].sym.?;
    }

    // ---- clocks ----

    fn nowMs() u64 {
        return clock.monotonicNs() / std.time.ns_per_ms;
    }

    fn nowMsFn(_: ?*anyopaque) u64 {
        return nowMs();
    }

    fn isVol0ManagerFn(ctx: ?*anyopaque) bool {
        const self: *WirePlane = @ptrCast(@alignCast(ctx.?));
        return self.set.volumes[0].sym != null;
    }

    /// The registrant key of a roster member (a file substrate holds no
    /// PR key: 0).
    fn rosterKey(_: shard.MemberId) u64 {
        return 0;
    }
};

// ---------------------------------------------------------------------------
// The seams over the set.
// ---------------------------------------------------------------------------

/// The member rows' durable home: tree 0 of volume 0, one put/delete per
/// change.
const RowStore = struct {
    plane: *WirePlane,

    fn store(self: *RowStore) shard.RowStore {
        return .{ .ctx = self, .put = &put, .delete = &delete };
    }

    fn put(ctx: ?*anyopaque, k: []const u8, v: []const u8) error{Io}!void {
        const self: *RowStore = @ptrCast(@alignCast(ctx.?));
        WirePlane.controlEntry(self.plane.set0(), &.{.{ .put = .{ .key = k, .value = v } }}, .parking) catch return error.Io;
    }

    fn delete(ctx: ?*anyopaque, k: []const u8) error{Io}!void {
        const self: *RowStore = @ptrCast(@alignCast(ctx.?));
        WirePlane.controlEntry(self.plane.set0(), &.{.{ .delete = k }}, .parking) catch return error.Io;
    }
};

/// Volume 0's tree 0 as the death ledger's store.
const LedgerStore = struct {
    plane: *WirePlane,

    const control_vtable: alloc_lease.ControlStore.VTable = .{ .get = &get, .put = &put, .delete = &delete };

    fn store(self: *LedgerStore) death_ledger.Store {
        return .{ .control = .{ .ctx = self, .vtable = &control_vtable }, .range_ctx = self, .range = &range, .probe = &probe };
    }

    fn get(ctx: *anyopaque, gpa: Allocator, k: []const u8) alloc_lease.StoreError!?[]u8 {
        const self: *LedgerStore = @ptrCast(@alignCast(ctx));
        return self.plane.set.volumes[0].lookup(gpa, key.TREE_CONTROL, k) catch |e| switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            else => error.Io,
        };
    }

    fn put(ctx: *anyopaque, k: []const u8, v: []const u8, adm: alloc_lease.Admission) alloc_lease.StoreError!void {
        const self: *LedgerStore = @ptrCast(@alignCast(ctx));
        return WirePlane.controlEntry(self.plane.set0(), &.{.{ .put = .{ .key = k, .value = v } }}, adm);
    }

    fn delete(ctx: *anyopaque, k: []const u8, adm: alloc_lease.Admission) alloc_lease.StoreError!void {
        const self: *LedgerStore = @ptrCast(@alignCast(ctx));
        return WirePlane.controlEntry(self.plane.set0(), &.{.{ .delete = k }}, adm);
    }

    fn range(ctx: *anyopaque, gpa: Allocator, start: []const u8, end: []const u8, out: *std.ArrayList(death_ledger.KeyValue)) error{ Io, OutOfMemory }!void {
        const self: *LedgerStore = @ptrCast(@alignCast(ctx));
        var kvs: std.ArrayList(tree.KeyValue) = .empty;
        defer kvs.deinit(gpa);
        self.plane.set.volumes[0].range(gpa, key.TREE_CONTROL, start, end, std.math.maxInt(usize), &kvs) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            else => error.Io,
        };
        errdefer for (kvs.items) |*kv| kv.deinit(gpa);
        for (kvs.items) |kv| try out.append(gpa, .{ .key = kv.key, .value = kv.value });
        kvs.items.len = 0;
    }

    fn probe(ctx: *anyopaque) bool {
        const self: *LedgerStore = @ptrCast(@alignCast(ctx));
        return !self.plane.set.volumes[0].isFailed();
    }
};

/// The `dir_rename` lease's store over volume 0's tree 0 (a full window
/// is the verb's `Deferred`).
const RenameStore = struct {
    plane: *WirePlane,

    fn store(self: *RenameStore) dir_rename.ControlStore {
        return .{ .ctx = self, .lookup = &lookup, .put = &put, .delete = &delete };
    }

    fn lookup(ctx: *anyopaque, k: []const u8) error{Io}!?cr.DirRename {
        const self: *RenameStore = @ptrCast(@alignCast(ctx));
        const bytes = (self.plane.set.volumes[0].lookup(self.plane.gpa, key.TREE_CONTROL, k) catch return error.Io) orelse return null;
        defer self.plane.gpa.free(bytes);
        return cr.DirRenameWire.decode(bytes) catch error.Io;
    }

    fn put(ctx: *anyopaque, k: []const u8, v: []const u8) error{ Io, Deferred }!void {
        const self: *RenameStore = @ptrCast(@alignCast(ctx));
        WirePlane.controlEntry(self.plane.set0(), &.{.{ .put = .{ .key = k, .value = v } }}, .try_) catch |e| return switch (e) {
            error.WindowFull => error.Deferred,
            else => error.Io,
        };
    }

    fn delete(ctx: *anyopaque, k: []const u8) error{ Io, Deferred }!void {
        const self: *RenameStore = @ptrCast(@alignCast(ctx));
        WirePlane.controlEntry(self.plane.set0(), &.{.{ .delete = k }}, .try_) catch |e| return switch (e) {
            error.WindowFull => error.Deferred,
            else => error.Io,
        };
    }
};

/// The reader side's ack half (`token_cache.RecallSink`): the kernel view
/// of a recalled object invalidated through the session's notify seam
/// once the session is up (a recall before it owes no kernel view). The
/// data plane's layout-entry discard and block-key census purge are not
/// bound (the planes' device-path arm is owed): a recalled object answers
/// `no_entry`, which purges nothing and keeps nothing — this mount holds
/// no foreign layout entries until its divert can dial.
pub const RecallSink = struct {
    notify: ?tokens.divert.NotifySeam = null,

    pub fn bindSession(self: *RecallSink, seam: tokens.divert.NotifySeam) void {
        self.notify = seam;
    }

    fn sink(self: *RecallSink) tokens.cache.RecallSink {
        return .{ .ctx = self, .discardLayout = &discardLayout, .purgeCensus = &purgeCensus, .noteRecalled = &noteRecalled };
    }

    fn discardLayout(_: *anyopaque, _: u64) tokens.cache.DiscardVerdict {
        return .no_entry;
    }

    fn purgeCensus(_: *anyopaque) void {}

    fn noteRecalled(ctx: *anyopaque, object: u64) void {
        const self: *RecallSink = @ptrCast(@alignCast(ctx));
        const seam = self.notify orelse return;
        var inval: tokens.divert.KernelInvalidator = .{ .notify = seam };
        inval.noteRecalled(object);
    }
};

/// The token holder's records: the set's trees (the FOLDED inode, the
/// carried xattrs in name order, the dentries in cookie order).
pub const SetRecordSource = struct {
    set: *MetaSet,

    pub fn source(self: *SetRecordSource) tokens.table.RecordSource {
        return .{ .ctx = self, .inode = &inode, .xattrs = &xattrs, .dentries = &dentries };
    }

    fn mapErr(e: meta_set.Error) tokens.table.SourceError {
        return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
            else => error.Corrupt,
        };
    }

    fn inode(ctx: *anyopaque, object: u64) tokens.table.SourceError!?record.InodeValue {
        const self: *SetRecordSource = @ptrCast(@alignCast(ctx));
        const bytes = (self.set.lookup(self.set.gpa, key.TREE_INODES, &key.inodeKey(object)) catch |e| return mapErr(e)) orelse return null;
        defer self.set.gpa.free(bytes);
        return record.InodeValue.decode(bytes) catch return error.Corrupt;
    }

    fn xattrs(ctx: *anyopaque, arena: Allocator, object: u64, out: *std.ArrayList(tokens.table.XattrRec)) tokens.table.SourceError!void {
        const self: *SetRecordSource = @ptrCast(@alignCast(ctx));
        var kvs: std.ArrayList(tree.KeyValue) = .empty;
        defer tree.deinitKeyValues(self.set.gpa, &kvs);
        const start = key.xattrKey(object, 0, 0);
        const end = key.xattrKey(object, key.HASH56_MAX, std.math.maxInt(u8));
        self.set.range(self.set.gpa, key.TREE_XATTRS, &start, &end, std.math.maxInt(usize), &kvs) catch |e| return mapErr(e);
        for (kvs.items) |kv| {
            const x = record.XattrValue.decode(kv.value) catch return error.Corrupt;
            if (!tokens.table.isCarriedXattr(x.name)) continue;
            try out.append(arena, .{ .name = try arena.dupe(u8, x.name), .value = try arena.dupe(u8, x.value) });
        }
        std.mem.sort(tokens.table.XattrRec, out.items, {}, struct {
            fn lt(_: void, a: tokens.table.XattrRec, b: tokens.table.XattrRec) bool {
                return std.mem.lessThan(u8, a.name, b.name);
            }
        }.lt);
    }

    fn dentries(ctx: *anyopaque, arena: Allocator, object: u64, after: u64, max: usize, out: *std.ArrayList(tokens.table.DirRec)) tokens.table.SourceError!bool {
        const self: *SetRecordSource = @ptrCast(@alignCast(ctx));
        var kvs: std.ArrayList(tree.KeyValue) = .empty;
        defer tree.deinitKeyValues(self.set.gpa, &kvs);
        const start = key.dentryKey(object, 0, 0);
        const end = key.dentryKey(object, key.HASH54_MAX, std.math.maxInt(u8));
        self.set.range(self.set.gpa, key.TREE_DENTRIES, &start, &end, std.math.maxInt(usize), &kvs) catch |e| return mapErr(e);
        var n: usize = 0;
        for (kvs.items) |kv| {
            const dk = key.DentryKey.decode(kv.key) catch return error.Corrupt;
            const cookie = key.encodeReaddirCookie(dk.hash54(), dk.collSeq());
            if (cookie <= after) continue;
            if (n == max) return true;
            const dv = record.DentryValue.decode(kv.value) catch return error.Corrupt;
            try out.append(arena, .{ .cookie = cookie, .child_ino = dv.child_ino, .file_type = dv.file_type, .name = try arena.dupe(u8, dv.name) });
            n += 1;
        }
        return false;
    }
};

/// The holder's lease oracle over the membership shard: a client id is a
/// member's token id (`clientIdOf`), its verdict the shard's three-valued
/// liveness.
const OwnerOracle = struct {
    plane: *WirePlane,

    fn oracle(self: *OwnerOracle) tokens.table.LeaseOracle {
        return .{ .ctx = self, .verdict = &verdict, .remaining = &remaining };
    }

    fn find(self: *OwnerOracle, client: tokens.table.ClientId) ?shard.MemberId {
        for (self.plane.owner.members.items) |m| if (clientIdOf(m.id) == client) return m.id;
        return null;
    }

    fn verdict(ctx: ?*anyopaque, client: tokens.table.ClientId, _: u64) tokens.table.LeaseVerdict {
        const self: *OwnerOracle = @ptrCast(@alignCast(ctx.?));
        const id = self.find(client) orelse return .unknown;
        return switch (self.plane.owner.liveness(id, WirePlane.nowMs())) {
            .live => .live,
            .dead => .expired,
            .unknown => .unknown,
        };
    }

    fn remaining(ctx: ?*anyopaque, client: tokens.table.ClientId, _: u64) ?u64 {
        const self: *OwnerOracle = @ptrCast(@alignCast(ctx.?));
        const id = self.find(client) orelse return null;
        const m = self.plane.owner.find(id) orelse return null;
        const now = WirePlane.nowMs();
        return if (m.deadline_ms > now) (m.deadline_ms - now) * std.time.ns_per_ms else 0;
    }
};

/// A member's token client id: xxh3 of its 16 B member id (a reader's id
/// is random, a writer's is its stable identity; both hash injectively
/// enough for a table keyed per holder).
pub fn clientIdOf(id: shard.MemberId) tokens.table.ClientId {
    return checksum.xxh3(&id);
}

/// The holder's push wire: the readers' listeners are dialled by their
/// published endpoints — a reader plane this daemon has no link to is
/// `unreachable_` (its recall completes at the reader's lease expiry;
/// the charter's A1½ rule). The N-daemon joiner wave binds the dialler.
const NoPeers = struct {
    fn pusher(self: *NoPeers) tokens.table.Pusher {
        return .{ .ctx = self, .push = &push };
    }

    fn push(_: *anyopaque, _: tokens.table.ClientId, _: u64, _: []const u64) tokens.table.PushVerdict {
        return .unreachable_;
    }
};

/// The token service's resolver over the set: a slot this process leases
/// (every slot of a volume it manages whose lease is its own) is local;
/// another appender's lease names the holder.
const LocalResolver = struct {
    set: *MetaSet,

    fn resolver(self: *LocalResolver) tokens.divert.Resolver {
        return .{ .ctx = self, .homeOf = &homeOf, .refresh = &refresh, .dial = &dial };
    }

    fn homeOf(ctx: *anyopaque, slot: u16) tokens.divert.Home {
        const self: *LocalResolver = @ptrCast(@alignCast(ctx));
        const be = self.set.volumeOfSlot(slot);
        if (!be.slots.armed) return .local;
        return switch (be.slots.cache.stepHome(slot)) {
            .local => .local,
            // The holder's token identity is its appender id on this
            // volume (what a joiner's divert dials by; the reader side
            // keys its planes on the same word).
            .foreign => |f| .{ .foreign = .{ .holder = f.holder, .identity = f.holder } },
            .unreachable_ => |u| .{ .unreachable_ = .{ .holder = u.holder } },
        };
    }

    fn refresh(ctx: *anyopaque, slot: u16, _: u32) tokens.divert.Home {
        return homeOf(ctx, slot);
    }

    fn dial(_: *anyopaque, _: tokens.table.ClientId, _: u32) ?tokens.cache.Link {
        return null;
    }
};

fn readEnrollSecret(gpa: Allocator, be0: *KvBackend) StartError!wire_auth.Secret {
    const bytes = (be0.lookup(gpa, key.TREE_CONTROL, wire_auth.ENROLL_KEY) catch |e| return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        else => error.Io,
    }) orelse return error.EnrollMissing;
    defer gpa.free(bytes);
    return wire_auth.decodeEnroll(bytes) catch error.EnrollMissing;
}

// ---------------------------------------------------------------------------
// Tests (Linux; a file-backed symmetric volume under /build/tmp): the plane
// over a real backend, dialled over real TCP.
// ---------------------------------------------------------------------------

const testing = std.testing;
const dlm = @import("../dlm.zig");
const dialer = @import("../../wire/dialer.zig");
const page = @import("appender_page.zig");
const join = @import("join.zig");
const node_seq = @import("../node_seq.zig");

const DevicePages = struct {
    be: *KvBackend,

    fn reader(self: *DevicePages) join_ladder.PageReader {
        return .{ .ctx = self, .read = &read };
    }

    fn read(ctx: *anyopaque, offset: u64, out: *[page.PAGE_LEN]u8) error{Io}!void {
        const self: *DevicePages = @ptrCast(@alignCast(ctx));
        const img = self.be.dev.readRange(self.be.gpa, offset, page.PAGE_LEN) catch return error.Io;
        defer self.be.gpa.free(img);
        @memcpy(out, img[0..page.PAGE_LEN]);
    }
};

const Poison = struct {
    fn poison(_: ?*anyopaque, _: []const u8) void {}
    fn sink() membership.park_reclaim.PoisonSink {
        return .{ .ctx = null, .poison = &poison };
    }
};

fn joinerIdentity() verbs.WireIdentity {
    return .{ .node_token = 0xB0B0, .writer_id = 77, .mount_slot = 0x5002, .reserved = 0 };
}

test "linux: the daemon's wire plane over a real symmetric set — the listener up, the endpoint in the RAM table AND as the durable endpoint:{0} record; a second process's joiner dials it over TCP, JoinAppender + AcquireSlots + a first touch + PublishRoots land, its membership join makes it a member with a durable member row, the clean leave frees its region; a remount re-publishes the endpoint and finds no member" {
    const gpa = testing.allocator;
    const s = try meta_set.TestSet.create(gpa, "wire-plane", 1);
    defer s.destroy();
    try s.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    const pubs0 = stats.wire_endpoint_publications.load();
    var ep_text_buf: [ENDPOINT_TEXT_MAX]u8 = undefined;
    var ep_text_len: usize = 0;
    {
        const set = try s.open();
        defer set.close();
        var locks = try dlm.Manager.init(gpa, 64);
        defer locks.deinit();
        const bound = try xv.set_bind.Bound.create(gpa, s.io(), set, &locks, &s.env);
        defer bound.destroy(gpa);
        const plane = (try WirePlane.start(gpa, s.io(), set, &s.env, 4, 1 << 30, &bound.plane, false)).?;
        var plane_live = true;
        defer if (plane_live) plane.stop();
        try testing.expect(plane.advertised.port != 0);
        try testing.expectEqual(@as(u64, 1), stats.wire_listener_up.load());
        try testing.expectEqual(pubs0 + 1, stats.wire_endpoint_publications.load());
        @memcpy(ep_text_buf[0..plane.endpointText().len], plane.endpointText());
        ep_text_len = plane.endpointText().len;
        // The two homes of the endpoint agree.
        const be0 = set.volumes[0];
        const rep = try be0.sym.?.manager.resolveEndpoint(.{ .appender_id = 0 });
        try testing.expectEqual(@sizeOf(endpoint.EndpointWire), rep.endpoint.?.len);
        try testing.expect(std.mem.bytesToValue(endpoint.EndpointWire, rep.endpoint.?.bytes[0..@sizeOf(endpoint.EndpointWire)]).eql(plane.advertised));
        const durable = (try be0.lookup(gpa, key.TREE_CONTROL, cr.endpointKey(0).slice())).?;
        defer gpa.free(durable);
        const dec = try cr.EndpointRecordWire.decode(durable);
        try testing.expectEqual(plane.advertised.port, dec.port);
        try testing.expectEqual(plane.advertised.family, dec.family);

        // The joiner: another process's shape — a dialled TCP client.
        var client = dialer.Client.init(gpa, s.io(), plane.advertised, meta_set.TestSet.secret, joinerIdentity());
        defer client.deinit();
        var tl: join_ladder.TcpLink = .{ .client = &client };
        var pages: DevicePages = .{ .be = be0 };
        var wire: join_ladder.ManagerWire = .{ .gpa = gpa, .link = tl.link(), .volume = 0, .identity = joinerIdentity(), .pages = pages.reader() };
        var joiner = join_ladder.Joiner.init(gpa, &wire);
        defer joiner.deinit();
        const ident: page.Identity = .{ .node_token = joinerIdentity().node_token, .mount_slot = joinerIdentity().mount_slot, .writer_id = joinerIdentity().writer_id };
        try joiner.joinRegion(.{ .identity = ident, .volume_base = node_seq.nodeSeqBase(be0.sb.uuid), .grain = be0.dev.mode.grain(), .ring0_head = be0.core.headPos() });
        try testing.expectEqual(@as(u32, 1), joiner.appenderId());
        try testing.expectEqual(@as(usize, 2), try joiner.acquireRotor(2));
        const t = try joiner.firstTouch(60_000);
        try testing.expect(t == .granted);
        try testing.expect(be0.slots.table.view(60_000).leasedBy(1));
        // The native slot is the manager's: a touch of it is SlotBusy.
        const busy = try joiner.firstTouch(0);
        try testing.expect(busy == .busy);
        try testing.expectEqual(@as(u32, 0), busy.busy.holder);
        // Membership: the joiner is a member of this daemon's shard with a
        // durable row; the cadence evicts nothing while it beats.
        var gate = membership.park_reclaim.ParkGate.arm(s.io(), 0, membership.park_reclaim.tParkMaxMs(46_500, plane.owner.clocks.grace_ms));
        var member: shard.Member = .{ .gpa = gpa, .id = shard.writerMemberId(joinerIdentity()), .role = .writer, .link = tl.link(), .pid = 4242, .boot = @splat(1), .endpoint = .none, .pr_key = 0, .mount = "/mnt-b", .gate = &gate, .poison = Poison.sink(), .carriage = joiner.carriageSink() };
        const now = clock.monotonicNs() / std.time.ns_per_ms;
        try testing.expectEqual(shard.Member.JoinOutcome.granted, member.join(now, null));
        try testing.expectEqual(@as(usize, 1), plane.owner.members.items.len);
        try testing.expectEqual(shard.Liveness.live, plane.owner.liveness(member.id, now));
        const row = (try be0.lookup(gpa, key.TREE_CONTROL, cr.memberKey(.{ .node_token = joinerIdentity().node_token, .mount_slot = joinerIdentity().mount_slot }).slice())).?;
        defer gpa.free(row);
        try testing.expectEqual(@as(u32, 4242), (try shard.MemberRow.decode(row)).pid);
        plane.tick();
        try testing.expectEqual(@as(usize, 1), plane.owner.members.items.len);
        // The renewal beat over the wire: idle before the cadence, renewed
        // at it (the shard's deadline moves; nothing is evicted).
        try testing.expectEqual(shard.TickVerdict.idle, member.tick(now + 1, 0));
        const renewals0 = shard.stats.membership_renewals.load();
        try testing.expectEqual(shard.TickVerdict.renewed, member.tick(now + plane.owner.clocks.renew_ms + 1, 0));
        try testing.expectEqual(renewals0 + 1, shard.stats.membership_renewals.load());
        try testing.expectEqual(shard.Liveness.live, plane.owner.liveness(member.id, now + plane.owner.clocks.renew_ms + 1));
        // The token holder's lease oracle sees the member as live.
        try testing.expectEqual(tokens.table.LeaseVerdict.live, plane.oracle.oracle().verdict(plane.oracle.oracle().ctx, clientIdOf(member.id), 0));
        // The clean leave: the region Free, the member gone, its row deleted.
        try member.leave();
        for (joiner.leased.items) |l| {
            // A lessee's release words floor strictly above tree 0's record
            // (the departing ring's frontier; a never-written slot's is the
            // grant's word + 1).
            const rec_bytes = (try be0.lookup(gpa, key.TREE_CONTROL, cr.slotStateKey(l.slot).slice())).?;
            defer gpa.free(rec_bytes);
            const rec = try cr.SlotStateWire.decode(rec_bytes);
            try testing.expect(!try wire.releaseSlot(joiner.appenderId(), l.slot, l.g, .{ .root = rec.root, .cursor = rec.cursor + 1, .extents = rec.slot_tree_extents, .seq_floor = rec.seq_floor + 1 }, &.{}));
            try testing.expectEqual(@import("slots/lease_table.zig").State.unleased, be0.slots.table.view(l.slot).state);
        }
        joiner.leased.items.len = 0;
        const left = try joiner.leave();
        try testing.expect(!left.already);
        try testing.expectEqual(@as(usize, 0), plane.owner.members.items.len);
        try testing.expect((try be0.lookup(gpa, key.TREE_CONTROL, cr.memberKey(.{ .node_token = joinerIdentity().node_token, .mount_slot = joinerIdentity().mount_slot }).slice())) == null);
        const p1 = (try be0.sym.?.manager.readPageOf(1)).?;
        try testing.expectEqual(page.State.free, p1.state);
        plane.stop();
        plane_live = false;
        try testing.expectEqual(@as(u64, 0), stats.wire_listener_up.load());
    }
    // The remount: a fresh ephemeral port re-publishes the record; the
    // shard arms with an EMPTY roster (the clean leave deleted the row).
    {
        const set = try s.open();
        defer set.close();
        var locks = try dlm.Manager.init(gpa, 64);
        defer locks.deinit();
        const bound = try xv.set_bind.Bound.create(gpa, s.io(), set, &locks, &s.env);
        defer bound.destroy(gpa);
        const plane = (try WirePlane.start(gpa, s.io(), set, &s.env, 4, 1 << 30, &bound.plane, false)).?;
        defer plane.stop();
        try testing.expectEqual(pubs0 + 2, stats.wire_endpoint_publications.load());
        try testing.expect(plane.owner.grace == null);
        try testing.expect(!std.mem.eql(u8, ep_text_buf[0..ep_text_len], plane.endpointText()));
    }
}

test "linux: MW_BIND=off refuses a writable mount at rung 3 and arms nothing on a reader; a malformed bind refuses by name" {
    const gpa = testing.allocator;
    const s = try meta_set.TestSet.create(gpa, "wire-off", 1);
    defer s.destroy();
    const set = try s.open();
    defer set.close();
    var locks = try dlm.Manager.init(gpa, 64);
    defer locks.deinit();
    const bound = try xv.set_bind.Bound.create(gpa, s.io(), set, &locks, &s.env);
    defer bound.destroy(gpa);
    try s.env.put("SQUEEZEFS_MW_BIND", "off");
    const refusals0 = join_ladder.stats.join_ladder_refusals[join_ladder.Rung.membership.index()].load();
    try testing.expectError(error.MembershipOff, WirePlane.start(gpa, s.io(), set, &s.env, 4, 1 << 30, &bound.plane, false));
    try testing.expectEqual(refusals0 + 1, join_ladder.stats.join_ladder_refusals[join_ladder.Rung.membership.index()].load());
    try testing.expect((try WirePlane.start(gpa, s.io(), set, &s.env, 4, 1 << 30, &bound.plane, true)) == null);
    try s.env.put("SQUEEZEFS_MW_BIND", "not-an-endpoint");
    try testing.expectError(error.MalformedBind, WirePlane.start(gpa, s.io(), set, &s.env, 4, 1 << 30, &bound.plane, false));
}

test "client ids: one per member id, stable, distinct across two members" {
    const a: shard.MemberId = @splat(1);
    var b: shard.MemberId = @splat(1);
    b[15] = 2;
    try testing.expectEqual(clientIdOf(a), clientIdOf(a));
    try testing.expect(clientIdOf(a) != clientIdOf(b));
}

test "the start refusals name their rung and cause" {
    try testing.expect(std.mem.indexOf(u8, startReason(error.MembershipOff), "rung 3") != null);
    try testing.expect(std.mem.indexOf(u8, startReason(error.EnrollMissing), "job:enroll") != null);
    try testing.expect(std.mem.indexOf(u8, startReason(error.MalformedBind), "SQUEEZEFS_MW_BIND") != null);
}
