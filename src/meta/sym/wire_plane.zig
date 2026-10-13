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
//! ledger's projection, whose volume hooks are the §5.9 recovery driver
//! bound over every managed volume (`membership/volume_bind.zig`): a dead
//! member's region on this daemon's volumes is recovered by the poll that
//! sees its record, and the recovered slots' open intents roll forward.
//!
//! Teardown in order: the token hook disarmed on every conveyor (a pass in
//! flight finishes its recall first), the listener stopped (every
//! connection severed, the accept thread joined), the services dropped.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
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
const volume_bind = membership.volume_bind;
const tokens = @import("tokens.zig");
const xv = @import("xv.zig");
const dir_rename = xv.dir_rename;
const alloc_lease = @import("../../data/sym_planes/alloc_lease.zig");
const plane_arm = @import("../../data/sym_planes/plane_arm.zig");
const block_grant = @import("../../data/sym_planes/block_grant.zig");
const data_bitmap = @import("../../data/sym_planes/data_bitmap.zig");
const sym_records = @import("../../data/sym_planes/records.zig");
const kvda_home = @import("kvda_home.zig");
const slots_lt = @import("slots/lease_table.zig");
const slots_handover = @import("slots/handover.zig");
const DataPlane = @import("../../data/plane.zig").DataPlane;
const router = @import("../../data/router.zig");
const set_config = @import("../../data/set_config.zig");
const lifecycle_stats = @import("../../ops/lifecycle/stats.zig").stats;
const joined = @import("joined.zig");
const wire_auth = @import("../../wire/auth.zig");
const listener = @import("../../wire/listener.zig");
const endpoint = @import("../../wire/endpoint.zig");
const service = @import("../../wire/service.zig");
const verbs = @import("../../wire/verbs.zig");
const frame = @import("../../wire/frame.zig");
const screen_mod = @import("../../wire/screen.zig");

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

/// The manager's appender id — the holder every joiner's divert keys the
/// manager's token plane by (`LocalResolver.homeOf`: identity = holder).
const MANAGER_HOLDER: u32 = @import("../journal.zig").FIXED_RING_APPENDER;

/// The longest `host:port` text (`[v6]:port` at full width).
pub const ENDPOINT_TEXT_MAX: usize = 64;

/// The token holder's table caps: objects = one per cached foreign inode
/// a reader may hold (the budget law caps the reader at `mem_budget/256`
/// bytes; 64 k objects × ~200 B ≈ 12 MiB on the holder side at the
/// fleet's 3 GiB budget — the reference's `token_table_objects`), clients
/// = the listener's connection cap (every reader is a connection).
pub const TOKEN_OBJECTS_CAP: u32 = 1 << 16;

/// The manager half's state: the verb faces, the owner shard, the death
/// ledger, the `dir_rename` lease — what a JOINED daemon has none of.
const ManagerHalf = struct {
    faces: []join_ladder.ManagerHandlers,
    managers: join_ladder.ManagerSet,
    owner: shard.Owner,
    owner_svc: shard.OwnerService,
    rows: RowStore,
    ledger_store: LedgerStore,
    ledger: death_ledger.Ledger,
    ledger_handlers: death_ledger.LedgerHandlers,
    projection: death_ledger.Projection,
    /// The recovery driver over each managed volume (`volume_bind`): the
    /// projection's hooks, so a dead member's region on this daemon's
    /// volumes is recovered by its poll.
    bindings: []*volume_bind.VolumeBinding,
    hooks: []death_ledger.VolumeHook,
    roll_forward: volume_bind.RollForwardBinding,
    rename_store: RenameStore,
    rename_lease: dir_rename.Lease,
    rename_svc: dir_rename.LeaseService,
    death_fanout: DeathFanout,
};

/// The owner's ONE death sink fanned out: the token holder sweeps the
/// dead member's grants FIRST (`noteMemberDeparted` — its outstanding
/// recalls complete `expired_with_lease`, nothing waits a TTL on a corpse;
/// a same-node successor under the same identity starts clean), then the
/// ledger records the death.
const DeathFanout = struct {
    plane: *WirePlane,
    ledger: shard.DeathSink,

    fn sink(self: *DeathFanout) shard.DeathSink {
        return .{ .ctx = self, .noteDeath = &noteDeath };
    }

    fn noteDeath(ctx: ?*anyopaque, dead: shard.DeadMember) void {
        const self: *DeathFanout = @ptrCast(@alignCast(ctx.?));
        self.plane.holder.noteMemberDeparted(self.plane.io, clientIdOf(dead.id));
        self.plane.peers.forgetMember(clientIdOf(dead.id));
        // Every volume this process manages stops resolving the corpse's
        // listener at the eviction, not at its recovery.
        if (dead.identity) |w| for (self.plane.set.volumes) |be| if (be.sym) |aset| aset.manager.retireEndpointOf(.{ .node_token = w.node_token, .mount_slot = w.mount_slot });
        self.ledger.noteDeath(self.ledger.ctx, dead);
    }
};

/// The joined half's state (charter §5.7 "a later RW mount JOINS it"):
/// this mount as a MEMBER of its home shard — the join with its own
/// endpoint, the heartbeat thread at the renew cadence (`T_self` is the
/// member's own clock: the ticks never wait on anything else), the park
/// gate's clocks from the grant, the endpoint published to the manager's
/// RAM table, the self-lease the divert reads.
const JoinedHalf = struct {
    plane: *WirePlane,
    js: *joined.JoinedSet,
    member: shard.Member,
    poison: Poison,
    beat: std.Thread = undefined,
    stop: std.atomic.Value(bool) = .init(false),
    /// The last tick's verdict class for `.stats` (0 idle, 1 renewed, 2
    /// parked, 3 fenced, 4 rejoined).
    last_verdict: std.atomic.Value(u8) = .init(0),
    renewals: std.atomic.Value(u64) = .init(0),
    /// The writer plane whose windows every renewal declares (published
    /// by `bindDataPlanes` with release; the beat reads it with acquire).
    windows: std.atomic.Value(?*plane_arm.WriterPlane) = .init(null),
    /// The declaration's frame bytes for the member's next renewal (the
    /// member borrows them for the call on this thread).
    decl_buf: []u8,
    /// The owner's census as this member sees it: pulled by the beat once
    /// per renew interval, read by the token lease oracle and the parked
    /// guard scopes' sweep for every member but the manager (whose word is
    /// this member's own lease).
    census: membership.census_view.CensusView,
    /// The members the last census commit retired (the beat's buffer).
    departed: std.ArrayList(membership.census_view.Departure) = .empty,

    /// A tenth of the shipped `T_self` floor (the smoke's `T_owner` of
    /// 3 s derives a 797 ms `T_self`; a tick every 80 ms meets a 265 ms
    /// renew cadence with three chances) — the member's own beat, never
    /// the daemon's slower cadences.
    const TICK_MS: u64 = 80;

    fn run(self: *JoinedHalf) void {
        var name_buf: @import("../../core/thread_name.zig").CommBuf = undefined;
        _ = @import("../../core/thread_name.zig").nameCurrentThread(&name_buf, "sqz-member");
        while (!self.stop.load(.acquire)) {
            self.pullCensusIfDue();
            // The windows this writer holds ride the renewal (charter §5.5):
            // refreshed every beat from the writer plane once it is armed.
            if (self.windows.load(.acquire)) |wp| {
                const d = wp.declareWindows(self.decl_buf);
                self.member.windows_bytes = d.bytes;
                self.member.n_windows = d.n_windows;
            }
            const t = clock.monotonicNs();
            const v = self.member.tick(t / std.time.ns_per_ms, t);
            self.last_verdict.store(switch (v) {
                .idle => 0,
                .renewed => 1,
                .parked => 2,
                .fenced => 3,
                .rejoined => 4,
            }, .release);
            if (v == .renewed) _ = self.renewals.fetchAdd(1, .monotonic);
            if (v == .fenced) {
                // Terminal: the poison latched the volumes FAILED.
                return;
            }
            // A reclaim admitted by a SUCCESSOR manager: its grant table
            // is fresh, so every token this mount holds from holder 0 is
            // unproven — the plane re-points (every token dropped, the
            // channel fail-closed until a round completes) before the held
            // acks drain into writes the successor never recalled.
            if (v == .rejoined) {
                _ = self.plane.divert.repoint(self.plane.io, MANAGER_HOLDER, MANAGER_HOLDER);
                // The successor's RAM endpoint table is fresh: this
                // listener is published again under the same appender id
                // (what the successor's divert and step sessions dial).
                _ = self.plane.publishToManager();
                log.info("cluster wire: the home shard's successor at {f} admitted the reclaim (epoch {d}) — the manager's token plane re-pointed, this listener re-published", .{ self.js.managerEndpoint(), self.member.epoch });
            }
            // The grant's set-config generation ahead of this daemon's
            // word: the manager added or removed a data volume online.
            if ((v == .renewed or v == .rejoined) and self.member.config_generation > lifecycle_stats.volume_config_generation.load()) self.plane.learnSetConfig(self.member.config_generation);
            _ = self.js.park_gate.expireIfDue(clock.monotonicNs(), self.poison.sink());
            // A plain OS thread of the daemon's own: the kernel's sleep,
            // never an `Io` handle (the lanes' timers are the lanes').
            const ts: linux.timespec = .{ .sec = 0, .nsec = @intCast(TICK_MS * std.time.ns_per_ms) };
            _ = linux.nanosleep(&ts, null);
        }
    }

    /// One census walk per renew interval (the renewal's own cadence; the
    /// pages ride the manager link on this thread, never a sweep's): the
    /// view the sweeps read for every other member's lease.
    fn pullCensusIfDue(self: *JoinedHalf) void {
        const renew_ns = (self.member.clocks orelse return).renew_ms * std.time.ns_per_ms;
        const now = clock.monotonicNs();
        if (!self.census.due(now, renew_ns)) return;
        var cursor: u64 = 0;
        var grace_open = false;
        while (true) {
            const rep = self.js.link.call(.{ .census = .{ .cursor = cursor, .limit = verbs.CENSUS_PAGE_MAX, .reserved = 0 } }) catch {
                self.census.abandon();
                return;
            };
            const pg = rep.census;
            grace_open = pg.head.grace_open != 0;
            var rows = pg.rows();
            // `state` passed the row screen at the decode (≤ 2).
            while (rows.next()) |row| self.census.stage(clientIdOf(row.head.id), @fromBackingInt(@intCast(row.head.state)), row.head.endpoint) catch {
                self.census.abandon();
                return;
            };
            if (pg.head.has_next == 0) break;
            cursor = pg.head.next;
        }
        self.departed.clearRetainingCapacity();
        // Out of memory naming the departures loses only the reader half
        // below; the holder's sweep judges by the verdict regardless.
        self.census.commit(grace_open, now, &self.departed) catch {};
        // The joiner's death fan-out: the census is the only word it gets
        // of an eviction, so the token holder sweeps against it at once —
        // a recall waiting on the dead reader completes now, not at the
        // pass's next expiry tick (11.25 s, past every caller's reply
        // bound while the pass holds a served step) — and every reader
        // plane bound to a departed member's listener drops its tokens.
        _ = self.plane.holder.sweepExpired(self.plane.io, clock.monotonicNs());
        for (self.departed.items) |d| self.plane.holderDeparted(d.endpoint);
    }

    fn selfLive(ctx: ?*anyopaque, _: u64) bool {
        const self: *JoinedHalf = @ptrCast(@alignCast(ctx.?));
        return !self.member.words.fenced.load(.acquire) and !self.js.park_gate.isExpired();
    }

    fn selfLease(self: *JoinedHalf) tokens.cache.SelfLease {
        return .{ .ctx = self, .live = &selfLive };
    }
};

/// The terminal poison of a joiner (`T_park_max` passed, `UnknownLease`
/// before `T_self`): every volume latches FAILED — EIO until remount,
/// never a silent weaker posture.
const Poison = struct {
    set: *MetaSet,

    fn poison(ctx: ?*anyopaque, reason: []const u8) void {
        const self: *Poison = @ptrCast(@alignCast(ctx.?));
        for (self.set.volumes) |be| _ = be.lane.latch.failStop();
        log.err("cluster wire: this joined mount is POISONED ({s}) — every volume latched FAILED (EIO until remount)", .{reason});
    }

    fn sink(self: *Poison) membership.park_reclaim.PoisonSink {
        return .{ .ctx = self, .poison = &poison };
    }
};

pub const WirePlane = struct {
    gpa: Allocator,
    io: Io,
    set: *MetaSet,
    posture: backend.Posture,
    svc: service.Service = .init,
    listener: *listener.Listener,
    advertised: endpoint.EndpointWire,
    endpoint_buf: [ENDPOINT_TEXT_MAX]u8 = undefined,
    endpoint_len: usize = 0,
    /// This mount's wire identity (volume 0's appender 0 on the manager;
    /// the joined region's on a joiner).
    identity: verbs.WireIdentity,
    mgr: ?*ManagerHalf,
    joined: ?*JoinedHalf,
    source: SetRecordSource,
    holder: tokens.table.Holder,
    oracle: LeaseOracle,
    peers: PeerLinks,
    pusher: tokens.divert.LinkPusher,
    binding: tokens.ConveyorBinding,
    hook: @import("../conveyor.zig").TokenHook,
    holder_svc: tokens.divert.HolderService,
    reader_svc: tokens.divert.ReaderService,
    resolver_ctx: LocalResolver,
    steps: xv.ops.StepService(MetaSet),
    /// The travelling guards' holder face (`XvGuards`/`XvRelease`) and the
    /// record-level ship's (`setattr/setxattr/removexattr/publish_layout`
    /// served under this holder's door, its kernel view invalidated).
    guards_svc: xv.guards.GuardsService,
    record_svc: xv.record_ship.RecordShipService(MetaSet),
    /// The striping's `SupplyStripeInos` served side (a creator mints
    /// stripe records in its slot on the directory's volume).
    supply_svc: xv.stripe.SupplyService(MetaSet),
    /// The data plane's publish face for shipped layout publishes
    /// (`bindServedPublish`; its address is what `record_svc` arms).
    served_publish: xv.record_ship.PublishFace = undefined,
    t_owner_ms: u64,
    /// This mount's read divert (charter §5.5 "Every writer is also a
    /// token client of the holders it writes to"): a slot another
    /// appender leases is read under a token or answers the typed EAGAIN
    /// — never off this mount's cache. Bound into the filesystem by
    /// `mount`; its kernel-invalidation half joins once the session is up.
    divert: tokens.Divert,
    sink: RecallSink,
    /// The data planes' arms (`bindDataPlanes`, after the listener): the
    /// holder's grant plane is published with a release store and read by
    /// the pre-registered grant verb handlers with an acquire load (the
    /// pair: `bindDataPlanes` store / `serveBlockGrant` load), so a verb
    /// arriving between the listener and the arm meets a `Busy` refusal,
    /// never a torn handler; a joiner's writer plane is the cadence's and
    /// the stop's alone.
    holder_plane: std.atomic.Value(?*plane_arm.HolderPlane) = .init(null),
    writer_plane: ?*plane_arm.WriterPlane = null,
    /// The holder's page sink home (`kvda_home.zig`): volume 0's heap and
    /// ring for the KVDA pages and their deltas; the set's heap geometries
    /// the lease verbs' ref screen reads. Null on a joiner.
    kvda: ?*kvda_home.KvdaHome = null,
    heaps: []data_bitmap.HeapGeometry = &.{},
    /// The data volumes the renewal's declarations are screened by.
    decl_bounds: []screen_mod.DataVolumeBounds = &.{},

    /// Arm the plane (module doc); null on a set whose volume 0 carries no
    /// symmetric plane (the `--single-writer` class: no wire). The posture
    /// is volume 0's: its appender set makes this daemon the manager, its
    /// joined set a member.
    pub fn start(gpa: Allocator, io: Io, set: *MetaSet, env: *const std.process.Environ.Map, raw_cpus: u32, mem_budget_bytes: u64, xv_plane: *xv.set_bind.SetPlane, read_only: bool) StartError!?*WirePlane {
        const be0 = set.volumes[0];
        if (be0.sym == null and be0.joined == null) return null;
        const bind = listener.bindFromEnv(env) catch return error.MalformedBind;
        if (join_ladder.declarationRungs(.{ .symmetric_knob_on = true, .volume_stamped = true, .bits = &.{}, .mw_bind = bind })) |r| {
            join_ladder.noteRefusal(r);
            if (!read_only) return error.MembershipOff;
            return null;
        }
        const secret = if (be0.joined) |js| js.secret else readEnrollSecret(gpa, be0) catch |e| return e;
        const t_owner_ms = shard.tOwnerMsFromEnv(env);
        const identity: verbs.WireIdentity = if (be0.sym) |s| .{ .node_token = s.identity.node_token, .writer_id = s.identity.writer_id, .mount_slot = s.identity.mount_slot, .reserved = 0 } else be0.joined.?.wire_identity;

        const self = try gpa.create(WirePlane);
        errdefer gpa.destroy(self);
        self.* = .{
            .gpa = gpa,
            .io = io,
            .set = set,
            .posture = if (be0.sym != null) .manager else .joined,
            .listener = undefined,
            .advertised = .none,
            .identity = identity,
            .mgr = null,
            .joined = null,
            .source = .{ .set = set },
            .holder = undefined,
            .oracle = .{ .plane = self },
            .peers = .{ .plane = self },
            .pusher = undefined,
            .binding = undefined,
            .hook = undefined,
            .holder_svc = undefined,
            .reader_svc = undefined,
            .resolver_ctx = .{ .plane = self },
            .steps = .{ .plane = xv_plane },
            .guards_svc = .{ .parked = &xv_plane.parked, .volume = 0 },
            .record_svc = .{ .plane = xv_plane },
            .supply_svc = .{ .plane = xv_plane },
            .t_owner_ms = t_owner_ms,
            .divert = undefined,
            .sink = .{},
        };
        errdefer self.peers.deinit();
        if (be0.sym != null) try self.startManagerHalf(t_owner_ms, identity) else try self.startJoinedHalfPre();
        errdefer self.stopHalf();
        // The parked guard scopes' sweep reads the owner's word on each
        // scope's client (the manager's roster; a joiner's census view);
        // the belt for a client it has no word on is the manager's
        // `T_owner` (a joiner learns the grant's).
        xv_plane.parked.bindLiveness(self.oracle.guardLiveness(), t_owner_ms * std.time.ns_per_ms);
        errdefer xv_plane.parked.bindLiveness(null, xv.guards.SCOPE_GRACE_NS);
        // The divert's self-lease: the manager IS its own membership owner
        // (never lapses); a joiner's is its member lease.
        const lease: tokens.cache.SelfLease = if (self.joined) |j| j.selfLease() else .always;
        self.divert = tokens.Divert.init(gpa, io, self.resolver_ctx.resolver(), clientIdOf(shard.writerMemberId(identity)), 0, self.sink.sink(), lease, tokens.cache.recordsBudgetBytes(mem_budget_bytes));
        // The token holder over this daemon's trees; the pusher over the
        // peers' dialled links.
        self.holder = tokens.table.Holder.init(gpa, io, identity.writer_id, self.oracle.oracle(), TOKEN_OBJECTS_CAP, listener.deriveMaxConnections(raw_cpus, listener.raiseNofile())) catch return error.OutOfMemory;
        errdefer self.holder.deinit();
        // The pushed frame names the holder by the word the readers key
        // their planes on: the holder's appender id on volume 0.
        self.pusher = .{ .volume = 0, .identity = if (be0.joined) |js| js.appenderId() else 0, .peers = self.peers.peers() };
        self.binding = .{ .io = io, .holder = &self.holder, .pusher = self.pusher.pusher() };
        self.hook = self.binding.hook();
        self.holder_svc = .{ .io = io, .holder = &self.holder, .source = self.source.source(), .resolver = self.resolver_ctx.resolver(), .custody = null };
        self.reader_svc = .{ .io = io, .divert = &self.divert };
        // The ONE service, every family registered, then the listener.
        if (self.mgr) |m| {
            m.managers.register(&self.svc);
            m.owner_svc.register(&self.svc);
            m.ledger_handlers.register(&self.svc);
            m.rename_svc.register(&self.svc);
            self.svc.register(.resolve_set_config, .{ .ctx = self, .serve = &serveResolveSetConfig });
        }
        self.holder_svc.register(&self.svc);
        self.reader_svc.register(&self.svc);
        self.steps.register(&self.svc);
        self.guards_svc.register(&self.svc);
        self.record_svc.register(&self.svc);
        self.supply_svc.register(&self.svc);
        // The data planes' grant verbs: registered now, armed by
        // `bindDataPlanes` once the data plane is open.
        self.svc.register(.block_grant, .{ .ctx = self, .serve = &serveBlockGrant });
        self.svc.register(.return_blocks, .{ .ctx = self, .serve = &serveReturnBlocks });
        self.svc.register(.free_blocks, .{ .ctx = self, .serve = &serveFreeBlocks });
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
        // The endpoint published: the manager into its RAM table + the
        // durable record per managed volume; a joiner to the manager's
        // table over the wire, then its membership join with it.
        if (self.mgr != null) try self.publishEndpoints() else try self.startJoinedHalfPost();
        // The recall hook on every volume's pass.
        for (set.volumes) |be| be.cv.armTokens(&self.hook);
        if (self.mgr) |m| {
            log.info("cluster wire: manager of {d} volume(s) listening at {s}; membership owner term {d} (T_owner {d} ms, grace {s})", .{ m.faces.len, self.endpointText(), m.owner.term, t_owner_ms, if (m.owner.grace != null) "open" else "closed" });
        } else {
            log.info("cluster wire: JOINED mount listening at {s} (appender {d} of {d} volume(s)); member of the home shard at {f}, renew every {d} ms, T_self {d} ms", .{ self.endpointText(), be0.joined.?.appenderId(), set.volumes.len, be0.joined.?.managerEndpoint(), self.joined.?.member.clocks.?.renew_ms, self.joined.?.member.clocks.?.t_self_ms });
        }
        return self;
    }

    fn startManagerHalf(self: *WirePlane, t_owner_ms: u64, identity: verbs.WireIdentity) StartError!void {
        const gpa = self.gpa;
        const set = self.set;
        const sym0 = set.volumes[0].sym.?;
        const m = try gpa.create(ManagerHalf);
        errdefer gpa.destroy(m);
        const faces = try gpa.alloc(join_ladder.ManagerHandlers, set.volumes.len);
        errdefer gpa.free(faces);
        var n_faces: usize = 0;
        for (set.volumes, 0..) |be, v| {
            const s = be.sym orelse continue;
            faces[n_faces] = .{ .mgr = &s.manager, .table = &s.handlers, .set_width = @intCast(set.volumes.len), .volume = @intCast(v) };
            n_faces += 1;
        }
        // The owner derives with RTT 0 (no member to measure yet) and the
        // checkpoint landing ceiling as the revalidation cadence.
        const clocks = shard.LeaseClocks.derive(t_owner_ms, 0, shard.CHECKPOINT_LANDING_CEILING_MS) catch return error.UnsafeClocks;
        m.* = .{
            .faces = faces[0..n_faces],
            .managers = .{ .faces = faces[0..n_faces] },
            .owner = undefined,
            .owner_svc = undefined,
            .rows = .{ .plane = self },
            .ledger_store = .{ .plane = self },
            .ledger = undefined,
            .ledger_handlers = undefined,
            .projection = undefined,
            .bindings = &.{},
            .hooks = &.{},
            .roll_forward = .{ .plane = self.steps.plane },
            .rename_store = .{ .plane = self },
            .rename_lease = undefined,
            .rename_svc = undefined,
            .death_fanout = undefined,
        };
        self.mgr = m;
        // The owner shard at the manager's term, the predecessor's roster
        // as the grace window's expectation.
        var expected: std.ArrayList(shard.MemberId) = .empty;
        defer expected.deinit(gpa);
        self.priorRoster(&expected) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            else => error.Io,
        };
        const term = sym0.manager.term;
        m.owner = shard.Owner.arm(gpa, clocks, 0, term, term -| 1, expected.items, nowMs()) catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            // `term ≤ prior` cannot happen: prior is `term − 1` by construction.
            error.StaleTerm => unreachable,
        };
        errdefer m.owner.deinit();
        m.owner.bindRows(m.rows.store());
        m.ledger = death_ledger.Ledger.init(gpa, m.ledger_store.store(), @intCast(set.volumes.len), identity, t_owner_ms);
        errdefer m.ledger.deinit();
        m.death_fanout = .{ .plane = self, .ledger = m.ledger.deathSink() };
        m.owner.bindDeathSink(m.death_fanout.sink());
        m.owner_svc = .{ .owner = &m.owner, .now = &nowMsFn, .generation = .{ .ctx = null, .read = &configGenerationFn } };
        m.ledger_handlers = .{ .ledger = &m.ledger, .census = &m.owner, .now = &nowMsFn, .is_vol0_manager = &isVol0ManagerFn, .now_ctx = self };
        // The recovery driver bound over every managed volume: the poll's
        // hooks (`releaseRecovered` → `recoverDead` → `regionPending`) and
        // the set-level roll-forward of the recovered slots' open intents.
        const bindings = try gpa.alloc(*volume_bind.VolumeBinding, n_faces);
        errdefer gpa.free(bindings);
        const hooks = try gpa.alloc(death_ledger.VolumeHook, n_faces);
        errdefer gpa.free(hooks);
        var n_bound: usize = 0;
        errdefer for (bindings[0..n_bound]) |b| b.destroy();
        for (set.volumes) |be| {
            if (be.sym == null) continue;
            bindings[n_bound] = try volume_bind.VolumeBinding.create(gpa, be, &m.ledger, &m.owner, clocks.skew_max_ms, clocks.renew_ms);
            hooks[n_bound] = bindings[n_bound].hook();
            n_bound += 1;
        }
        m.bindings = bindings;
        m.hooks = hooks;
        m.projection = .{ .ledger = &m.ledger, .volumes = hooks[0..n_bound], .vol_tags = &.{}, .roll_forward = m.roll_forward.seam() };
        m.rename_lease = .{ .io = self.io, .store = m.rename_store.store(), .term = term, .own_id = sym0.region0().appenderId() };
        m.rename_svc = .{ .lease = &m.rename_lease };
        // The holder-decided handover over the wire (charter §5.5): every
        // served step / record verb feeds the slot's judge; an offer the
        // rule makes rides the requester's next renewal grant.
        self.steps.served_ship = .{ .ctx = self, .note = &servedShipFn };
        self.record_svc.served_ship = .{ .ctx = self, .note = &servedShipFn };
        for (set.volumes) |be| {
            if (be.sym == null) continue;
            be.slots.verbs.offer_push = .{ .ctx = self, .push = &offerPushFn };
        }
    }

    /// A served ship by `peer` on `slot` of `volume`: the peer's appender
    /// id on that volume (the directory's Live page under its identity),
    /// the judge, the offer (the push rides `offerSlot`).
    fn servedShipFn(ctx: *anyopaque, volume: u16, slot: u16, peer: verbs.WireIdentity) void {
        const self: *WirePlane = @ptrCast(@alignCast(ctx));
        if (volume >= self.set.volumes.len) return;
        const be = self.set.volumes[volume];
        const s = be.sym orelse return;
        const requester = s.manager.appenderOfIdentity(.{ .node_token = peer.node_token, .mount_slot = peer.mount_slot }) orelse return;
        _ = be.slots.noteWireShip(slot, requester);
    }

    /// `offerSlot`'s push: the offer to appender `to` of the volume whose
    /// plane made it, carried on `to`'s member's next renewal grant.
    fn offerPushFn(ctx: *anyopaque, to: slots_lt.AppenderId, slot: u16, holder: slots_lt.AppenderId) void {
        const self: *WirePlane = @ptrCast(@alignCast(ctx));
        const m = self.mgr orelse return;
        const be = self.set.volumeOfSlot(slot);
        const s = be.sym orelse return;
        // A writer's member id IS its peer id (node, mount slot).
        const identity = s.manager.identityOfAppender(to) orelse return;
        const wire_id: verbs.WireIdentity = .{ .node_token = identity.node_token, .writer_id = 0, .mount_slot = identity.mount_slot, .reserved = 0 };
        m.owner.pushOffer(shard.writerMemberId(wire_id), slot, holder) catch return;
        slots_handover.stats.slot_offers_pushed.inc();
    }

    /// The joined half before the listener: the member's words (its
    /// endpoint is the listener's, learnt after it is up).
    fn startJoinedHalfPre(self: *WirePlane) StartError!void {
        const gpa = self.gpa;
        const js = self.set.volumes[0].joined.?;
        const j = try gpa.create(JoinedHalf);
        errdefer gpa.destroy(j);
        const local = @import("../guard/proc.zig").localIdentity();
        // The declaration frame: the renewal's class cap (one `WindowDecl`
        // per data volume, its runs the window's and the in-flight
        // registrations').
        const decl_buf = try gpa.alloc(u8, frame.CONTROL_CAP);
        errdefer gpa.free(decl_buf);
        j.* = .{
            .plane = self,
            .js = js,
            .member = undefined,
            .poison = .{ .set = self.set },
            .decl_buf = decl_buf,
            .census = membership.census_view.CensusView.init(gpa, self.io),
        };
        j.member = .{
            .gpa = gpa,
            .id = shard.writerMemberId(js.wire_identity),
            .role = .writer,
            .link = js.link.link(),
            .pid = @intCast(linux.getpid()),
            .boot = local.boot_id,
            .endpoint = .none,
            .pr_key = 0,
            .mount = js.mountpoint,
            .gate = &js.park_gate,
            .poison = j.poison.sink(),
            .carriage = self.set.volumes[0].slots.carriageSink(),
        };
        self.joined = j;
    }

    /// The joined half after the listener: the endpoint to the manager's
    /// RAM table (every volume's), the membership join with the endpoint
    /// (the holder's pusher reaches this listener through it), the park
    /// gate's clocks from the grant, the heartbeat thread.
    fn startJoinedHalfPost(self: *WirePlane) StartError!void {
        const j = self.joined.?;
        if (!self.publishToManager()) return error.Io;
        j.member.endpoint = self.advertised;
        const now = nowMs();
        switch (j.member.join(now, null)) {
            .granted => {},
            .grace_refused => {
                log.err("cluster wire: the home shard refused the membership join — its grace window stands (MembershipPending; retry the mount)", .{});
                return error.Io;
            },
            .collision => {
                log.err("cluster wire: the home shard answered Busy to the membership join — a member with this identity stands (another mount at this mount point?)", .{});
                return error.Io;
            },
            .unknown_lease, .failed => {
                log.err("cluster wire: the membership join failed", .{});
                return error.Io;
            },
        }
        const clocks = j.member.clocks.?;
        // `T_park_max = failover_bound + grace` at the grant's `T_owner`.
        j.js.park_gate.t_park_max_ms = membership.park_reclaim.tParkMaxMs(@import("extent_grant.zig").failoverBoundMs(0, 0), clocks.grace_ms);
        j.census.setRenewInterval(clocks.renew_ms * std.time.ns_per_ms);
        // The parked guard scopes' grace belt is the grant's `T_owner`.
        self.steps.plane.parked.bindLiveness(self.oracle.guardLiveness(), clocks.t_owner_ms * std.time.ns_per_ms);
        j.beat = std.Thread.spawn(.{}, JoinedHalf.run, .{j}) catch return error.OutOfMemory;
    }

    /// A joiner's listener into the manager's RAM endpoint table (every
    /// joined volume's); false when any publication failed (logged).
    fn publishToManager(self: *WirePlane) bool {
        var ok = true;
        for (self.set.volumes) |be| {
            const js = be.joined orelse continue;
            _ = js.publishEndpoint(self.advertised) catch |e| {
                log.err("cluster wire: PublishEndpoint to the manager failed ({s})", .{@errorName(e)});
                ok = false;
                continue;
            };
            stats.wire_endpoint_publications.inc();
        }
        return ok;
    }

    fn stopHalf(self: *WirePlane) void {
        if (self.mgr) |m| {
            for (m.bindings) |b| b.destroy();
            self.gpa.free(m.bindings);
            self.gpa.free(m.hooks);
            m.ledger.deinit();
            m.owner.deinit();
            self.gpa.free(m.faces.ptr[0..self.set.volumes.len]);
            self.gpa.destroy(m);
            self.mgr = null;
        }
        if (self.joined) |j| {
            j.census.deinit();
            j.departed.deinit(self.gpa);
            self.gpa.free(j.decl_buf);
            self.gpa.destroy(j);
            self.joined = null;
        }
    }

    pub fn stop(self: *WirePlane) void {
        for (self.set.volumes) |be| be.cv.disarmTokens();
        // A joiner's pending shipped frees reach the holder while its link
        // and membership stand; then its clean leave of the shard FIRST
        // (the manager's owner deletes the member row; a leave over a park
        // is refused — the crash shape, the ledger's to judge). The beat is
        // joined BEFORE the writer plane goes: every tick declares that
        // plane's windows (`declareWindows` on a destroyed plane was the
        // joiner's SIGSEGV at umount); the membership stands until the
        // leave — a few wire calls without a renewal sit far inside T_self.
        if (self.joined) |j| {
            j.stop.store(true, .release);
            j.beat.join();
            j.windows.store(null, .release);
        }
        if (self.writer_plane) |w| {
            w.destroy();
            self.writer_plane = null;
        }
        if (self.joined) |j| {
            j.member.leave() catch log.err("cluster wire: the membership leave was refused over a standing park — the home shard records this mount's death", .{});
        }
        self.listener.stop();
        stats.wire_listener_up.set(0);
        stats.wire_listener_connections.set(0);
        self.divert.deinit();
        self.holder.deinit();
        self.peers.deinit();
        // The holder's grant plane after the listener (no verb in flight)
        // and before the manager half (its control store releases the
        // leases).
        if (self.holder_plane.swap(null, .acq_rel)) |hp| {
            // The page sink leaves the cycle before the plane it writes
            // through goes; the leases released after (their records
            // name the pages until then).
            self.set.volumes[0].installKvdaSink(null);
            self.releaseAllocLeases(hp);
            hp.destroy();
        }
        if (self.kvda) |k| self.gpa.destroy(k);
        self.gpa.free(self.heaps);
        self.gpa.free(self.decl_bounds);
        // The sweep's word goes before the oracle it reads.
        self.steps.plane.parked.bindLiveness(null, xv.guards.SCOPE_GRACE_NS);
        self.stopHalf();
        self.gpa.destroy(self);
    }

    // ---- the set config across the fleet ----

    /// The owner's grant carries this daemon's `set_config` generation
    /// (`lifecycle_stats.volume_config_generation`: set at the mount's read
    /// and at every lifecycle commit).
    fn configGenerationFn(_: ?*anyopaque) u64 {
        return lifecycle_stats.volume_config_generation.load();
    }

    /// `ResolveSetConfig` (the manager's face): volume 0's `set_config`
    /// record as this manager reads it off tree 0 — a joiner's pull at the
    /// generation bump its renewal grant carried. The bytes travel in the
    /// reply scratch (the record's own total codec decodes them there).
    fn serveResolveSetConfig(ctx: *anyopaque, req: *const service.Request, scratch: []u8) service.Answer {
        const self: *WirePlane = @ptrCast(@alignCast(ctx));
        const r = req.payload.resolve_set_config;
        if (r.volume != 0 or self.mgr == null) return .{ .status = .{ .status = .not_manager } };
        const bytes = (self.set.volumes[0].lookup(self.gpa, key.TREE_CONTROL, set_config.CONTROL_KEY) catch return service.Answer.refused(error.InputOutput, "set_config")) orelse
            return .{ .ok = .{ .resolve_set_config = .{ .head = .{ .generation = 0, .len = 0, .reserved = 0 }, .bytes = &.{} } } };
        defer self.gpa.free(bytes);
        const generation = set_config.peekGeneration(bytes) orelse return service.Answer.refused(error.InputOutput, "set_config head");
        if (bytes.len > scratch.len or bytes.len > verbs.SET_CONFIG_MAX) return service.Answer.refused(error.ValueTooLarge, "set_config record exceeds the reply scratch");
        @memcpy(scratch[0..bytes.len], bytes);
        return .{ .ok = .{ .resolve_set_config = .{ .head = .{ .generation = generation, .len = @intCast(bytes.len), .reserved = 0 }, .bytes = scratch[0..bytes.len] } } };
    }

    /// A member the owner's census retired, by the listener it advertised
    /// (a joiner's death fan-out): the tokens this mount holds from the
    /// dead member's HOLDER role are void — its slots go to the manager's
    /// recovery and a new writer's commits recall nobody's tokens from
    /// that table — so every reader plane bound to the listener is
    /// re-pointed with every token dropped (the next serve re-resolves the
    /// slot and fetches from its new home), and its sessions retire.
    fn holderDeparted(self: *WirePlane, ep: endpoint.EndpointWire) void {
        var holders: std.ArrayList(u32) = .empty;
        defer holders.deinit(self.gpa);
        self.peers.retireListener(ep, &holders) catch {};
        for (holders.items) |h| {
            if (self.divert.repoint(self.io, h, h)) joined.stats.joined_dead_holder_planes_dropped.inc();
        }
    }

    /// A joiner learns an online `volume add-data` / `remove-data`
    /// (charter §8): the record pulled from the manager at the bump, then
    /// per data volume — one this plane lacks is ATTACHED (opened, the
    /// router row disabled), its writer row armed over the manager link
    /// (the window pulls its first grant at the first mint), then its
    /// recorded state published; one it has takes the record's state
    /// (`draining` / `retired` / `active` — the router's lattice decides
    /// the legality). A volume with a fabric endpoint record is left to
    /// the next mount (the daemon's own connect is the mount's act), said
    /// loud. The word moves to the record's generation only when every
    /// volume applied; a failure leaves it behind, so the next renewal
    /// retries (counted).
    pub fn learnSetConfig(self: *WirePlane, generation: u64) void {
        const gpa = self.gpa;
        const js = self.set.volumes[0].joined orelse return;
        const wp = self.writer_plane orelse return;
        const dp = wp.plane;
        const rep = js.link.call(.{ .resolve_set_config = .{ .volume = 0, .reserved = @splat(0) } }) catch |e| {
            lifecycle_stats.volume_config_pull_failures.inc();
            log.warn("set config: the manager's record at generation {d} could not be pulled ({s}) — the next renewal retries", .{ generation, @errorName(e) });
            return;
        };
        lifecycle_stats.volume_config_pulls.inc();
        const r = rep.resolve_set_config;
        if (r.head.len == 0) return;
        var diag: set_config.Refusal = undefined;
        var cfg = set_config.decode(gpa, r.bytes, &diag) catch {
            lifecycle_stats.volume_config_pull_failures.inc();
            log.err("set config: the manager's record at generation {d} did not decode — this joiner keeps generation {d}", .{ r.head.generation, lifecycle_stats.volume_config_generation.load() });
            return;
        };
        defer cfg.deinit(gpa);
        var complete = true;
        for (cfg.data) |d| {
            const want: router.VolumeState = switch (d.state) {
                .active => .active,
                .draining => .draining,
                .retired => .retired,
                .disabled => .disabled,
            };
            if (dp.rt.volume(d.vol_tag)) |v| {
                if (v.state == want) continue;
                dp.setVolumeState(d.vol_tag, want) catch |e| {
                    log.warn("set config: vol {x} {t} → {t} refused on this joiner ({s})", .{ d.vol_tag, v.state, want, @errorName(e) });
                    complete = false;
                    continue;
                };
                lifecycle_stats.volume_online_flips.inc();
                // Off placement: the window goes back to the holder (the
                // drain's retire waits on every bit it carved).
                if (want == .draining or want == .retired) _ = wp.returnWindowOf(d.vol_tag);
                continue;
            }
            if (d.state == .retired or d.uri.len == 0) continue;
            if (d.fabric != null) {
                log.warn("set config: vol {x} was added with a fabric endpoint record — this joiner connects it at its next mount (the daemon's own connect is the mount's act)", .{d.vol_tag});
                complete = false;
                continue;
            }
            const ordinal = dp.attachVolume(d) catch |e| {
                lifecycle_stats.volume_config_pull_failures.inc();
                log.err("set config: vol {x} ({s}) could not be attached to this joiner's plane ({s}) — it joins at the next mount", .{ d.vol_tag, d.uri, @errorName(e) });
                complete = false;
                continue;
            };
            wp.armVolume(ordinal);
            dp.setVolumeState(d.vol_tag, want) catch |e| {
                log.warn("set config: vol {x} attached but its state {t} was refused ({s})", .{ d.vol_tag, want, @errorName(e) });
                complete = false;
                continue;
            };
            lifecycle_stats.volume_online_attached.inc();
            log.info("set config: vol {x} ({s}) joined this mount ONLINE as ordinal {d} ({t}) at generation {d} — the manager's `volume add-data` learnt at the renewal", .{ d.vol_tag, d.uri, ordinal, want, cfg.config_generation });
        }
        if (complete) lifecycle_stats.volume_config_generation.set(cfg.config_generation);
    }

    // ---- the data planes' arms ----

    pub const BindDataError = error{ OutOfMemory, Io };

    /// Arm the data planes over the wire (charter §5.5 "Data-plane planes
    /// under the symmetric model"; `sym_data_planes.md` §3–§4): the
    /// manager acquires the allocation lease of every data volume on tree
    /// 0 (first-come at term 1; its own replay `already`; a dead
    /// predecessor's succession once recovered) and stands the holder's
    /// grant plane up; a joiner stands its writer plane up over the
    /// manager link. A lease refused `busy` or `deferred` arms NO holder
    /// plane: the grant verbs answer `Busy` and the joiners' writes wait
    /// (EAGAIN) — never two mints over one volume.
    pub fn bindDataPlanes(self: *WirePlane, dp: *DataPlane) BindDataError!void {
        const gpa = self.gpa;
        if (self.mgr) |m| {
            // The home: volume 0's heap, ring and tree 0, with every
            // member's heap geometry for the refs screen.
            const heaps = try gpa.alloc(data_bitmap.HeapGeometry, self.set.volumes.len);
            errdefer gpa.free(heaps);
            for (self.set.volumes, heaps) |be, *h| h.* = .{ .vol = be.index, .heap_start = be.sb.heap.start, .heap_len = be.sb.heap.len, .node_size = be.sb.node_size };
            self.heaps = heaps;
            const kv = try gpa.create(kvda_home.KvdaHome);
            errdefer gpa.destroy(kv);
            kv.* = .{ .gpa = gpa, .be = self.set.volumes[0], .aset = self.set0(), .control = m.ledger_store.store().control, .screen = self.leaseScreen(dp), .identity = self.writerIdentity() };
            self.kvda = kv;
            const n = dp.vols.n();
            const terms = try gpa.alloc(u64, n);
            defer gpa.free(terms);
            const holds = try gpa.alloc(plane_arm.HoldInput, n);
            defer gpa.free(holds);
            var refs = try gpa.alloc(alloc_lease.Refs, n);
            defer gpa.free(refs);
            for (dp.vols.rows[0..n], 0..) |row, i| {
                const g = (try self.acquireAllocLease(dp, row.?.vol_tag, row.?.alloc.capacityBlocks())) orelse return;
                terms[i] = g.term;
                refs[i] = g.refs;
                holds[i] = .{ .refs = refs[i].slice(), .already = g.already };
            }
            const hp = plane_arm.HolderPlane.create(gpa, self.io, dp, .{ .ctx = self, .isMember = &isMemberFn, .liveWriters = &liveWritersFn, .foreignSlots = &foreignSlotsFn }, kv.home(), terms, holds, m.owner.members.items.len + 1) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.Io, error.Corrupt => error.Io,
            };
            // Paired with the acquire loads in the grant verb handlers.
            self.holder_plane.store(hp, .release);
            // The renewal's declarations reach the holder (screened against
            // the plane's volumes first); the KVDA pages ride volume 0's
            // cycle before its barrier #1.
            const bounds = try gpa.alloc(screen_mod.DataVolumeBounds, n);
            errdefer gpa.free(bounds);
            for (dp.vols.rows[0..n], bounds) |row, *b| b.* = .{ .vol_tag = row.?.vol_tag, .blocks = row.?.alloc.capacityBlocks() };
            self.decl_bounds = bounds;
            m.owner_svc.data_volumes = bounds;
            m.owner_svc.windows = .{ .ctx = hp, .declare = &declareWindowsFn };
            self.set.volumes[0].installKvdaSink(.{ .ctx = hp, .writePages = &plane_arm.HolderPlane.writeKvdaPages });
            return;
        }
        const js = self.set.volumes[0].joined.?;
        self.writer_plane = try plane_arm.WriterPlane.create(gpa, self.io, dp, js.link.tokenLink(), self.identity);
        // The member's renewals declare the writer's held windows from now
        // on (the beat thread reads the pointer with acquire).
        if (self.joined) |j| j.windows.store(self.writer_plane, .release);
    }

    /// The ONLINE `volume add-data` arm (charter §8): the volume the plane
    /// just attached at `ordinal` gets its allocation lease on tree 0 and
    /// its row in the holder's grant plane — the manager's act (a joiner
    /// learns a new volume at its next mount; stated in the evidence).
    pub fn armDataVolume(self: *WirePlane, dp: *DataPlane, ordinal: u16) BindDataError!void {
        const m = self.mgr orelse return error.Io;
        const hp = self.holder_plane.load(.acquire) orelse return error.Io;
        const row = dp.vols.rows[ordinal].?;
        const g = (try self.acquireAllocLease(dp, row.vol_tag, row.alloc.capacityBlocks())) orelse return error.Io;
        hp.armVolume(ordinal, g.term, .{ .refs = g.refs.slice(), .already = g.already }, m.owner.members.items.len + 1) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io, error.Corrupt => error.Io,
        };
        log.info("data planes: vol {x} armed online as ordinal {d} under the allocation lease at term {d}", .{ row.vol_tag, ordinal, g.term });
    }

    fn leaseScreen(self: *WirePlane, dp: *DataPlane) alloc_lease.Screen {
        return .{ .ctx = dp, .dataVolumeBlocks = &dataVolumeBlocksFn, .set_width = @intCast(self.set.volumes.len), .heaps = self.heaps };
    }

    /// `shard.WindowSink`'s target: the holder adopts the member's declared
    /// windows under the writer key of its identity (the wire runs copied
    /// into the ledger's shape; a renewal is never a hot path).
    fn declareWindowsFn(ctx: *anyopaque, identity: verbs.WireIdentity, vol_tag: u64, ranges: []align(1) const verbs.GrantRunWire) void {
        const hp: *plane_arm.HolderPlane = @ptrCast(@alignCast(ctx));
        const runs = hp.gpa.alloc(block_grant.BlockGrant, ranges.len) catch return;
        defer hp.gpa.free(runs);
        for (ranges, runs) |r, *o| o.* = .{ .start = r.start, .len = r.len };
        hp.declareWindows(plane_arm.writerKeyOf(.{ .node_token = identity.node_token, .mount_slot = identity.mount_slot, .writer_id = identity.writer_id }), vol_tag, runs);
    }

    /// The allocation lease of ONE data volume on tree 0 (charter §5.5
    /// "first-come at term 1; a dead holder succeeded only after
    /// `dead_member:` AND `recovered:`"): the same-node succession takes a
    /// lease the PREDECESSOR MANAGER of this mount point holds — the
    /// identity page 0 carried when this open read it, proven dead by the
    /// ladder this open won — over at `term + 1`: its death recorded EARLY,
    /// its pages' window the fixed ring this open already replayed, the
    /// granted ranges re-learnt from the live writers' declarations. Null
    /// = `busy` / `deferred`: NO grant plane armed (logged); the grant verbs
    /// answer `Busy` and the joiners' writes wait (EAGAIN) — never two mints
    /// over one volume.
    fn acquireAllocLease(self: *WirePlane, dp: *DataPlane, vol_tag: u64, blocks: u64) BindDataError!?alloc_lease.Granted {
        const gpa = self.gpa;
        const m = self.mgr orelse return error.Io;
        const control = m.ledger_store.store().control;
        const screen = self.leaseScreen(dp);
        const sym0 = self.set.volumes[0].sym.?;
        const me = self.writerIdentity();
        const me_page: join_ladder.Identity = .{ .node_token = me.node_token, .mount_slot = me.mount_slot, .writer_id = me.writer_id };
        var proof: join_ladder.SameNodeProof = .{ .page0_at_open = sym0.page0_at_open orelse me_page, .me = me_page };
        var rctx: RecoverCtx = .{ .plane = self, .vol_tag = vol_tag };
        const recover: alloc_lease.Recover = .{ .ctx = &rctx, .recover = &predecessorBitmapRecovered };
        switch (alloc_lease.acquireAndHold(control, gpa, screen, true, .{ .vol_tag = vol_tag, .identity = me, .home_vol = 0, .appender_id = 0, .blocks = blocks }, proof.d0Proof(), recover, @intCast(clock.realtimeCoarseNs()))) {
            .granted => |g| {
                if (g.refs.n != 0) log.info("data planes: the allocation lease of vol {x} at term {d} names {d} bitmap page ref(s) ({s}) — the pages are loaded at the arm; what they carry beyond the refs census is deferred to the live writers' declarations", .{ vol_tag, g.term, g.refs.n, if (g.already) "this holder's own" else "the predecessor's, folded" });
                return g;
            },
            .busy => {
                log.warn("data planes: the allocation lease of vol {x} is held by a LIVE holder that is not this manager — no grant plane armed; grants answer Busy until it releases", .{vol_tag});
                return null;
            },
            .deferred => {
                log.warn("data planes: the allocation lease of vol {x} names a dead holder not yet recovered — no grant plane armed; grants answer Busy until the recovery lands", .{vol_tag});
                return null;
            },
            .rejected => |why| {
                log.err("data planes: the allocation lease acquire of vol {x} was REJECTED ({t})", .{ vol_tag, why });
                return error.Io;
            },
            .attempts_exhausted => {
                log.err("data planes: the allocation lease of vol {x} could not be taken over from this mount's predecessor in {d} attempts", .{ vol_tag, alloc_lease.SAME_NODE_TAKEOVER_ATTEMPTS });
                return error.Io;
            },
            .not_manager, .io_error => return error.Io,
        }
    }

    const RecoverCtx = struct { plane: *WirePlane, vol_tag: u64 };

    /// `alloc_lease.Recover` for the predecessor manager's bitmap: its
    /// ring IS the fixed ring this open replayed at the mount, so the
    /// KEPT WINDOW's deltas of its term are folded onto its pages through
    /// the record's refs (`kvda_home.KvdaHome.foldPredecessor`) before the
    /// lease moves — the successor's arm then loads pages that carry every
    /// carve the predecessor journaled.
    fn predecessorBitmapRecovered(ctx: ?*anyopaque, dead: sym_records.WriterIdentity, _: u16, term: u64) bool {
        const rc: *RecoverCtx = @ptrCast(@alignCast(ctx.?));
        const self = rc.plane;
        const kv = self.kvda orelse return true;
        const m = self.mgr orelse return false;
        const control = m.ledger_store.store().control;
        const lease = (alloc_lease.readLeaseRefs(control, self.gpa, rc.vol_tag) catch return false) orelse return true;
        if (!lease.holder.sameMount(dead) or lease.term != term) return true;
        return kv.foldPredecessor(rc.vol_tag, lease.refs.slice(), lease.blocks, term);
    }

    fn writerIdentity(self: *const WirePlane) sym_records.WriterIdentity {
        return .{ .node_token = self.identity.node_token, .mount_slot = self.identity.mount_slot, .writer_id = self.identity.writer_id };
    }

    fn releaseAllocLeases(self: *WirePlane, hp: *plane_arm.HolderPlane) void {
        const m = self.mgr orelse return;
        const control = m.ledger_store.store().control;
        for (0..hp.armed()) |i| {
            const h = hp.held(i);
            switch (alloc_lease.release(control, self.gpa, h.vol_tag, self.writerIdentity(), h.term)) {
                .released, .already => {},
                .deferred, .io_error => log.warn("data planes: the allocation lease of vol {x} (term {d}) could not be released at the stop — the successor acquires it through the death ledger", .{ h.vol_tag, h.term }),
            }
        }
    }

    fn dataVolumeBlocksFn(ctx: ?*anyopaque, vol_tag: u64) ?u64 {
        const dp: *DataPlane = @ptrCast(@alignCast(ctx.?));
        const v = dp.vols.byTag(vol_tag) orelse return null;
        return v.alloc.capacityBlocks();
    }

    /// The live WRITERS of the home shard (the declaration protocol's
    /// convergence term; readers hold no window).
    fn liveWritersFn(ctx: *anyopaque) u64 {
        const self: *WirePlane = @ptrCast(@alignCast(ctx));
        const m = self.mgr orelse return 0;
        var n: u64 = 0;
        for (m.owner.members.items) |mem| n += @intFromBool(mem.role == .writer);
        return n;
    }

    /// Slot trees another appender holds in volume 0's lease table (a
    /// lease, an offer, a release in flight, a dead lessee's).
    fn foreignSlotsFn(ctx: *anyopaque) u64 {
        const self: *WirePlane = @ptrCast(@alignCast(ctx));
        const table = self.set.volumes[0].slots.table;
        return table.leasedCount() -| table.heldCount(slots_lt.MANAGER);
    }

    /// A grant is served to a live member of the home shard (or the
    /// manager itself).
    fn isMemberFn(ctx: *anyopaque, id: sym_records.WriterIdentity) bool {
        const self: *WirePlane = @ptrCast(@alignCast(ctx));
        const wire_id: verbs.WireIdentity = .{ .node_token = id.node_token, .writer_id = id.writer_id, .mount_slot = id.mount_slot, .reserved = 0 };
        if (std.meta.eql(wire_id, self.identity)) return true;
        const m = self.mgr orelse return false;
        return m.owner.find(shard.writerMemberId(wire_id)) != null;
    }

    fn serveBlockGrant(ctx: *anyopaque, req: *const service.Request, scratch: []u8) service.Answer {
        const self: *WirePlane = @ptrCast(@alignCast(ctx));
        const hp = self.holder_plane.load(.acquire) orelse return service.Answer.refused(error.Busy, "no grant plane armed on this mount (not the allocation holder)");
        return plane_arm.HolderPlane.serveGrant(hp, req, scratch);
    }

    fn serveReturnBlocks(ctx: *anyopaque, req: *const service.Request, scratch: []u8) service.Answer {
        const self: *WirePlane = @ptrCast(@alignCast(ctx));
        const hp = self.holder_plane.load(.acquire) orelse return service.Answer.refused(error.Busy, "no grant plane armed on this mount (not the allocation holder)");
        return plane_arm.HolderPlane.serveReturn(hp, req, scratch);
    }

    fn serveFreeBlocks(ctx: *anyopaque, req: *const service.Request, scratch: []u8) service.Answer {
        const self: *WirePlane = @ptrCast(@alignCast(ctx));
        const hp = self.holder_plane.load(.acquire) orelse return service.Answer.refused(error.Busy, "no grant plane armed on this mount (not the allocation holder)");
        return plane_arm.HolderPlane.serveFrees(hp, req, scratch);
    }

    pub fn endpointText(self: *const WirePlane) []const u8 {
        return self.endpoint_buf[0..self.endpoint_len];
    }

    /// The data plane's foreign-layout face: a foreign slot's `layout`
    /// xattr off the holder's token page (the divert's serve; a `local`
    /// verdict reads the store).
    pub fn foreignLayout(self: *WirePlane) @import("../../data/plane.zig").DataPlane.ForeignLayout {
        return .{ .ctx = self, .lookup = &foreignLayoutFn };
    }

    /// The data plane's publish SHIP (charter §5.5 "the layout publish"
    /// of a foreign-slot file): ONE `record_ship` verb to the holder with
    /// the window's DMA proofs.
    pub fn foreignPublish(self: *WirePlane) @import("../../data/plane.zig").DataPlane.ForeignPublish {
        return .{ .ctx = self, .ship = &foreignPublishFn };
    }

    fn foreignPublishFn(ctx: *anyopaque, ino: u64, value: []const u8, proofs: []const verbs.DmaProofWire) errors.Error!void {
        const self: *WirePlane = @ptrCast(@alignCast(ctx));
        const p = self.steps.plane;
        const rep = xv.record_ship.ship(p.resolver, p.volume, ino, .{ .publish_layout = .{ .value = value, .proofs = proofs } }, 0) catch |e| return xv.record_ship.coreError(e);
        // `null` = the slot is ours after a redirect: the merge re-runs
        // locally on the caller's retry.
        if (rep == null) return error.SlotMoved;
    }

    /// Arm the holder side of shipped publishes with the data plane's
    /// face (after the plane is open; the record service loads it with
    /// an acquire).
    pub fn bindServedPublish(self: *WirePlane, face: @import("xv/record_ship.zig").PublishFace) void {
        self.served_publish = face;
        self.record_svc.armPublish(&self.served_publish);
    }

    fn foreignLayoutFn(ctx: *anyopaque, gpa: Allocator, ino: u64) errors.Error!@import("../../data/plane.zig").DataPlane.ForeignLayout.Verdict {
        const self: *WirePlane = @ptrCast(@alignCast(ctx));
        return foreignLayoutOf(&self.divert, self.io, gpa, ino);
    }

    /// The §5.9 recovery driver over the `vol`-th managed volume (null on
    /// a joiner, which recovers nothing: the ledger is the manager's).
    pub fn recoveryBinding(self: *WirePlane, vol: usize) ?*volume_bind.VolumeBinding {
        const m = self.mgr orelse return null;
        return if (vol < m.bindings.len) m.bindings[vol] else null;
    }

    /// The death ledger this manager writes and polls (null on a joiner).
    pub fn deathLedger(self: *WirePlane) ?*death_ledger.Ledger {
        const m = self.mgr orelse return null;
        return &m.ledger;
    }

    /// The cadence (the reclaim fiber's tick): on the manager the shard's
    /// eviction at `T_owner` (each death to the ledger's writer), the
    /// grace reap, the ledger's projection; a joiner's beat is its own
    /// thread — the tick samples the listener alone.
    pub fn tick(self: *WirePlane) void {
        stats.wire_plane_ticks.inc();
        stats.wire_listener_connections.set(self.listener.liveConnections());
        if (self.writer_plane) |w| w.tick();
        if (self.holder_plane.load(.acquire)) |hp| hp.tick();
        const m = self.mgr orelse return;
        const now = nowMs();
        const evicted = m.owner.expireDue(now);
        const reaped = m.owner.reapGraceDeadline(now, &rosterKey);
        const rep = m.projection.run(now, @intCast(clock.realtimeCoarseNs()));
        if (evicted != 0 or reaped != 0 or rep.deaths != 0 or rep.released != 0 or rep.retired != 0) {
            log.info("cluster wire: tick — {d} evicted at T_owner, {d} reaped at the grace deadline; ledger: {d} death record(s) standing, {d} acted, {d} region(s) released, {d} retired, {d} parked record(s) drained", .{ evicted, reaped, rep.deaths, rep.acted, rep.released, rep.retired, rep.drained });
        }
        if (rep.role_released) log.err("cluster wire: volume 0 unreachable for longer than T_owner — the manager role is released (the mount serves until its fence)", .{});
    }

    /// A `.stats` family: the listener and the shard (the manager's) or
    /// the member (a joiner's).
    pub fn writeStatsJson(self: *WirePlane, w: *Io.Writer) Io.Writer.Error!void {
        if (self.mgr) |m| {
            try w.print("{{\"wire_posture\":\"manager\",\"wire_endpoint\":\"{s}\",\"wire_listener_connections\":{d},\"wire_managed_volumes\":{d},\"membership_members\":{d},\"membership_term\":{d},\"membership_grace_remaining_ms\":{d},\"wire_peer_links\":{d}", .{ self.endpointText(), self.listener.liveConnections(), m.faces.len, m.owner.members.items.len, m.owner.term, m.owner.graceRemainingMs(nowMs()), self.peers.count() });
        } else {
            const j = self.joined.?;
            try w.print("{{\"wire_posture\":\"joined\",\"wire_endpoint\":\"{s}\",\"wire_listener_connections\":{d},\"wire_managed_volumes\":0,\"membership_members\":0,\"membership_term\":0,\"membership_grace_remaining_ms\":0,\"wire_peer_links\":{d},\"member_epoch\":{d},\"member_renewals\":{d},\"member_last_verdict\":{d},\"appender_parked\":{d}", .{ self.endpointText(), self.listener.liveConnections(), self.peers.count(), j.member.epoch, j.renewals.load(.monotonic), j.last_verdict.load(.acquire), @intFromBool(j.js.park_gate.isParked()) });
        }
        // The data planes' arm: the holder's per-volume words or the
        // writer's windows (`df` per the rule reads these).
        try w.writeAll(",\"block_grant_holder\":");
        if (self.holder_plane.load(.acquire)) |hp| try hp.writeStatsJson(w) else try w.writeAll("[]");
        try w.writeAll(",\"block_grant_writer\":");
        if (self.writer_plane) |wp| try wp.writeStatsJson(w) else try w.writeAll("[]");
        try w.writeByte('}');
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

/// The reader side's ack half (`token_cache.RecallSink`; spec §1.7 step
/// 2): the kernel view of a recalled object invalidated through the
/// session's notify seam once the session is up (a recall before it owes
/// no kernel view), and the data plane's read tiers purged of every block
/// key the recalled object's carried `layout` names — before the ack, so
/// the holder's free (and any member's re-mint) of those blocks follows
/// it (`DataPlane.recallPurgeLayout`). An object with no entry held, or a
/// layout that names its references elsewhere, answers `no_entry`: the
/// census over every foreign key, once per recall. This mount keeps no
/// write-side state the recall could discard (a foreign file's publish
/// ships to its holder), so nothing is ever `dirty_kept` here.
pub const RecallSink = struct {
    notify: ?tokens.divert.NotifySeam = null,
    /// The data plane whose tiers a recall purges: bound once it is open
    /// (release here, acquire at the recall); a recall before it purges
    /// nothing — no read has deposited yet.
    plane: std.atomic.Value(?*DataPlane) = .init(null),

    pub fn bindSession(self: *RecallSink, seam: tokens.divert.NotifySeam) void {
        self.notify = seam;
    }

    pub fn bindPlane(self: *RecallSink, dp: *DataPlane) void {
        self.plane.store(dp, .release);
    }

    pub fn sink(self: *RecallSink) tokens.cache.RecallSink {
        return .{ .ctx = self, .discardLayout = &discardLayout, .purgeCensus = &purgeCensus, .noteRecalled = &noteRecalled };
    }

    fn discardLayout(ctx: *anyopaque, _: u64, entry: ?*const tokens.cache.Entry) tokens.cache.DiscardVerdict {
        const self: *RecallSink = @ptrCast(@alignCast(ctx));
        const dp = self.plane.load(.acquire) orelse return .{ .discarded = 0 };
        const e = entry orelse return .no_entry;
        // No layout carried: a directory, a symlink, a never-written file —
        // it names no block.
        const value = e.xattr(@import("../../data/layout.zig").LAYOUT_XATTR_NAME) orelse return .{ .discarded = 0 };
        return if (dp.recallPurgeLayout(value)) |n| .{ .discarded = n } else .no_entry;
    }

    fn purgeCensus(ctx: *anyopaque) void {
        const self: *RecallSink = @ptrCast(@alignCast(ctx));
        const dp = self.plane.load(.acquire) orelse return;
        _ = dp.recallPurgeCensus();
    }

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

/// The holder's lease oracle: on the manager a client id is a member's
/// token id (`clientIdOf`), its verdict the shard's three-valued
/// liveness; on a joiner the manager's client is `live` while this
/// mount's own lease is (the manager IS the owner — its death parks this
/// mount), every other client `unknown` (their liveness is the owner's
/// word, judged at the recall deadline).
const LeaseOracle = struct {
    plane: *WirePlane,

    fn oracle(self: *LeaseOracle) tokens.table.LeaseOracle {
        return .{ .ctx = self, .verdict = &verdict, .remaining = &remaining };
    }

    fn find(self: *LeaseOracle, client: tokens.table.ClientId) ?shard.MemberId {
        const m = self.plane.mgr orelse return null;
        for (m.owner.members.items) |ms| if (clientIdOf(ms.id) == client) return ms.id;
        return null;
    }

    /// The owner's word on a member's lease — ONE law for the token
    /// holder's recalls and the parked guard scopes' sweep: the roster on
    /// the manager; on a joiner the manager's own lease is this member's
    /// (the manager cannot be evicted by anyone this member hears from),
    /// every other member's is the owner's census as the beat last saw it
    /// (`since_ns`: the instant the client was last known to be in
    /// contact — a census older than it cannot call its silence a death).
    fn word(self: *LeaseOracle, client: tokens.table.ClientId, since_ns: u64, now_ns: u64) tokens.table.LeaseVerdict {
        if (self.plane.joined) |j| {
            if (client == clientIdOf(shard.writerMemberId(j.js.manager_identity))) return if (JoinedHalf.selfLive(j, now_ns)) .live else .expired;
            return j.census.verdict(client, since_ns, now_ns);
        }
        const m = self.plane.mgr.?;
        const id = self.find(client) orelse return .unknown;
        return switch (m.owner.liveness(id, WirePlane.nowMs())) {
            .live => .live,
            .dead => .expired,
            .unknown => .unknown,
        };
    }

    /// The token lane's verdict: `since_ns` is the client's last grant
    /// ask (`now` for an ask in hand — a census that does not list an
    /// asking client is older than its join: unknown, never a refusal); a
    /// census newer than a reader's last ask that does not list it is its
    /// eviction, so a recall to a corpse completes at the census instead
    /// of the 45 s deadline.
    fn verdict(ctx: ?*anyopaque, client: tokens.table.ClientId, since_ns: u64, now_ns: u64) tokens.table.LeaseVerdict {
        const self: *LeaseOracle = @ptrCast(@alignCast(ctx.?));
        return self.word(client, since_ns, now_ns);
    }

    /// The parked guard scopes' face (`xv.guards.Liveness`): a scope's
    /// client was in contact when it parked; the roster not listing it on
    /// the manager, or a census newer than the scope not listing it on a
    /// joiner, is its eviction — the scope goes with the lease (§5.9).
    fn guardLiveness(self: *LeaseOracle) xv.guards.Liveness {
        return .{ .ctx = self, .verdict = &guardVerdict };
    }

    fn guardVerdict(ctx: *anyopaque, client: u64, since_ns: u64, now_ns: u64) xv.guards.Verdict {
        const self: *LeaseOracle = @ptrCast(@alignCast(ctx));
        if (self.plane.mgr) |m| if (self.find(client) == null) {
            return if (m.owner.graceOpenForReassert(WirePlane.nowMs())) .unknown else .dead;
        };
        return switch (self.word(client, since_ns, now_ns)) {
            .live => .live,
            .expired => .dead,
            .unknown => .unknown,
        };
    }

    fn remaining(ctx: ?*anyopaque, client: tokens.table.ClientId, _: u64) ?u64 {
        const self: *LeaseOracle = @ptrCast(@alignCast(ctx.?));
        const m = self.plane.mgr orelse return null;
        const id = self.find(client) orelse return null;
        const ms = m.owner.find(id) orelse return null;
        const now = WirePlane.nowMs();
        return if (ms.deadline_ms > now) (ms.deadline_ms - now) * std.time.ns_per_ms else 0;
    }
};

/// A member's token client id: hash64 of its 16 B member id (a reader's id
/// is random, a writer's is its stable identity; both hash injectively
/// enough for a table keyed per holder).
pub fn clientIdOf(id: shard.MemberId) tokens.table.ClientId {
    return checksum.hash64(&id);
}

/// The dialled links to this daemon's peers, ONE session per peer, bound
/// lazily off durable/derived state: by HOLDER appender id (the divert's
/// and the cross-owner plane's step sessions — the endpoint from the
/// manager's RAM table here, `ResolveEndpoint` over the wire on a joiner;
/// holder 0 is the manager's listener) and by token CLIENT id (the
/// holder's recall push — the member's declared endpoint on the manager;
/// the manager's listener on a joiner). A peer with no endpoint is
/// `unreachable_`. Every link re-resolves its peer's endpoint off the same
/// state before each (re)dial (`joined.EndpointResolver`), and the death
/// fan-out retires a dead member's session at once: a same-node successor
/// — the same appender id and client id at a new ephemeral port — is
/// reached at its first contact, never at the dead incarnation's port.
const PeerLinks = struct {
    plane: *WirePlane,
    lock: Io.Mutex = .init,
    by_holder: std.AutoHashMapUnmanaged(u32, *Peer) = .empty,
    by_client: std.AutoHashMapUnmanaged(tokens.table.ClientId, *Peer) = .empty,

    /// One peer's link with the key its endpoint is re-resolved by.
    const Peer = struct {
        links: *PeerLinks,
        key: union(enum) { holder: u32, client: tokens.table.ClientId },
        link: joined.SharedLink,

        fn resolveFn(ctx: *anyopaque) ?endpoint.EndpointWire {
            const self: *Peer = @ptrCast(@alignCast(ctx));
            return switch (self.key) {
                .holder => |h| self.links.holderEndpoint(h),
                .client => |c| self.links.clientEndpoint(c),
            };
        }

        fn destroy(self: *Peer) void {
            self.link.deinit();
            self.links.plane.gpa.destroy(self);
        }
    };

    fn deinit(self: *PeerLinks) void {
        var it = self.by_holder.valueIterator();
        while (it.next()) |p| p.*.destroy();
        self.by_holder.deinit(self.plane.gpa);
        var ic = self.by_client.valueIterator();
        while (ic.next()) |p| p.*.destroy();
        self.by_client.deinit(self.plane.gpa);
    }

    fn count(self: *PeerLinks) usize {
        self.lock.lockUncancelable(self.plane.io);
        defer self.lock.unlock(self.plane.io);
        return self.by_holder.count() + self.by_client.count();
    }

    fn secret(self: *PeerLinks) wire_auth.Secret {
        const be0 = self.plane.set.volumes[0];
        if (be0.joined) |js| return js.secret;
        return readEnrollSecret(self.plane.gpa, be0) catch @splat(0);
    }

    fn create(self: *PeerLinks, k: @FieldType(Peer, "key"), ep: endpoint.EndpointWire) ?*Peer {
        const p = self.plane.gpa.create(Peer) catch return null;
        p.* = .{ .links = self, .key = k, .link = joined.SharedLink.init(self.plane.gpa, self.plane.io, ep, self.secret(), self.plane.identity) };
        p.link.resolver = .{ .ctx = p, .resolve = &Peer.resolveFn };
        return p;
    }

    /// The death fan-out: the evicted member's session (its token client
    /// link, and every holder link at the same listener — one listener
    /// per daemon serves both roles) retired now; the next contact
    /// re-resolves and dials the successor's port.
    fn forgetMember(self: *PeerLinks, client: tokens.table.ClientId) void {
        self.lock.lockUncancelable(self.plane.io);
        defer self.lock.unlock(self.plane.io);
        const p = self.by_client.get(client) orelse return;
        const ep = p.link.endpointNow();
        p.link.invalidate();
        joined.stats.peer_links_invalidated.inc();
        var it = self.by_holder.valueIterator();
        while (it.next()) |h| {
            if (h.*.link.endpointNow().eql(ep)) {
                h.*.link.invalidate();
                joined.stats.peer_links_invalidated.inc();
            }
        }
    }

    /// A departed member's listener: every holder link at it (and its
    /// token client link) retired now; the holders it served are
    /// appended to `out` (the reader planes to drop).
    fn retireListener(self: *PeerLinks, ep: endpoint.EndpointWire, out: *std.ArrayList(u32)) Allocator.Error!void {
        self.lock.lockUncancelable(self.plane.io);
        defer self.lock.unlock(self.plane.io);
        var it = self.by_holder.iterator();
        while (it.next()) |e| {
            if (!e.value_ptr.*.link.endpointNow().eql(ep)) continue;
            e.value_ptr.*.link.invalidate();
            joined.stats.peer_links_invalidated.inc();
            try out.append(self.plane.gpa, e.key_ptr.*);
        }
        var ic = self.by_client.valueIterator();
        while (ic.next()) |p| if (p.*.link.endpointNow().eql(ep)) {
            p.*.link.invalidate();
            joined.stats.peer_links_invalidated.inc();
        };
    }

    /// The endpoint of `holder`'s listener.
    fn holderEndpoint(self: *PeerLinks, holder: u32) ?endpoint.EndpointWire {
        const be0 = self.plane.set.volumes[0];
        if (be0.joined) |js| {
            if (holder == js.appenderId()) return null;
            if (holder == 0) return js.managerEndpoint();
            return js.resolveEndpoint(holder) catch null;
        }
        const s = be0.sym orelse return null;
        const rep = s.manager.resolveEndpoint(.{ .appender_id = holder }) catch return null;
        const ep = rep.endpoint orelse return null;
        if (ep.len != @sizeOf(endpoint.EndpointWire)) return null;
        return std.mem.bytesToValue(endpoint.EndpointWire, ep.bytes[0..@sizeOf(endpoint.EndpointWire)]);
    }

    /// The listener of the member whose token client id is `client`: the
    /// owner's roster on the manager; on a joiner the manager's listener
    /// for the manager's own id, the owner's CENSUS over the wire for any
    /// other member (a reader of this holder's slots declared its
    /// listener at its join; the push of its recall needs it).
    fn clientEndpoint(self: *PeerLinks, client: tokens.table.ClientId) ?endpoint.EndpointWire {
        const be0 = self.plane.set.volumes[0];
        if (be0.joined) |js| {
            if (client == clientIdOf(shard.writerMemberId(js.manager_identity))) return js.managerEndpoint();
            return self.clientEndpointByCensus(js, client);
        }
        const m = self.plane.mgr orelse return null;
        for (m.owner.members.items) |ms| if (clientIdOf(ms.id) == client) {
            if (ms.endpoint.isNone()) return null;
            return ms.endpoint;
        };
        return null;
    }

    fn clientEndpointByCensus(self: *PeerLinks, js: *joined.JoinedSet, client: tokens.table.ClientId) ?endpoint.EndpointWire {
        _ = self;
        var cursor: u64 = 0;
        while (true) {
            const rep = js.link.call(.{ .census = .{ .cursor = cursor, .limit = verbs.CENSUS_PAGE_MAX, .reserved = 0 } }) catch return null;
            const pg = rep.census;
            var rows = pg.rows();
            while (rows.next()) |row| {
                if (clientIdOf(row.head.id) != client) continue;
                if (row.head.endpoint.isNone()) return null;
                return row.head.endpoint;
            }
            if (pg.head.has_next == 0) return null;
            cursor = pg.head.next;
        }
    }

    pub fn linkForHolder(self: *PeerLinks, holder: u32) ?tokens.cache.Link {
        self.lock.lockUncancelable(self.plane.io);
        defer self.lock.unlock(self.plane.io);
        if (self.by_holder.get(holder)) |p| return p.link.tokenLink();
        const ep = self.holderEndpoint(holder) orelse return null;
        const p = self.create(.{ .holder = holder }, ep) orelse return null;
        self.by_holder.put(self.plane.gpa, holder, p) catch {
            p.destroy();
            return null;
        };
        return p.link.tokenLink();
    }

    fn linkForClient(ctx: *anyopaque, client: tokens.table.ClientId) ?tokens.cache.Link {
        const self: *PeerLinks = @ptrCast(@alignCast(ctx));
        self.lock.lockUncancelable(self.plane.io);
        defer self.lock.unlock(self.plane.io);
        if (self.by_client.get(client)) |p| return p.link.tokenLink();
        const ep = self.clientEndpoint(client) orelse return null;
        const p = self.create(.{ .client = client }, ep) orelse return null;
        self.by_client.put(self.plane.gpa, client, p) catch {
            p.destroy();
            return null;
        };
        return p.link.tokenLink();
    }

    fn peers(self: *PeerLinks) tokens.divert.Peers {
        return .{ .ctx = self, .linkFor = &linkForClient };
    }

    /// The cross-owner plane's step sessions (`xv.set_bind.Dialer`).
    pub fn xvDialer(self: *PeerLinks) xv.set_bind.Dialer {
        return .{ .ctx = self, .linkFor = &xvLinkFor };
    }

    fn xvLinkFor(ctx: *anyopaque, holder: u32) ?xv.ops.Link {
        const self: *PeerLinks = @ptrCast(@alignCast(ctx));
        return self.linkForHolder(holder);
    }
};

/// A foreign file's layout record off its holder's token through `divert`
/// (the data plane's `ForeignLayout` face on every posture): `.local` for
/// a slot this mount reads itself, `.gone` for an object its holder no
/// longer has, else the size and the layout xattr record.
pub fn foreignLayoutOf(divert: *tokens.Divert, io: Io, gpa: Allocator, ino: u64) errors.Error!@import("../../data/plane.zig").DataPlane.ForeignLayout.Verdict {
    const served = (divert.serve(io, ino, .{ .records = true }) catch |e| return tokens.divert.coreError(e)) orelse return .local;
    defer served.release(io);
    const e = served.entry orelse return .gone;
    var layout_rec: ?[]u8 = null;
    for (e.xattrs) |x| if (std.mem.eql(u8, x.name, @import("../../data/layout.zig").LAYOUT_XATTR_NAME)) {
        // The record's wire shape is the xattr VALUE record (name +
        // value) the store holds: re-encoded for the layout decoder.
        const v = record.XattrValue{ .name = x.name, .value = x.value };
        layout_rec = v.encodeAlloc(gpa) catch |err| return switch (err) {
            error.OutOfMemory => error.OutOfMemory,
            else => error.InputOutput,
        };
        break;
    };
    return .{ .foreign = .{ .size = e.attrs.size, .layout = layout_rec } };
}

/// The token service's resolver over the set: a slot this process leases
/// (every slot of a volume it manages whose lease is its own) is local;
/// another appender's lease names the holder. On a JOINER an unleased
/// slot is the MANAGER's (it maintains every unleased tree; a lessee
/// since answers `NotHolder{current}`, followed once through `refresh`'s
/// `ResolveSlot`), never a local read of the projection.
const LocalResolver = struct {
    plane: *WirePlane,

    fn resolver(self: *LocalResolver) tokens.divert.Resolver {
        return .{ .ctx = self, .homeOf = &homeOf, .refresh = &refresh, .dial = &dial };
    }

    fn homeOf(ctx: *anyopaque, slot: u16) tokens.divert.Home {
        const self: *LocalResolver = @ptrCast(@alignCast(ctx));
        const be = self.plane.set.volumeOfSlot(slot);
        if (!be.slots.armed) return .local;
        return switch (be.slots.cache.stepHome(slot)) {
            .local => blk: {
                if (be.joined != null and !be.slots.table.view(slot).leasedBy(be.slots.me)) break :blk .{ .foreign = .{ .holder = 0, .identity = 0 } };
                break :blk .local;
            },
            // The holder's token identity is its appender id on this
            // volume (what a joiner's divert dials by; the reader side
            // keys its planes on the same word).
            .foreign => |f| .{ .foreign = .{ .holder = f.holder, .identity = f.holder } },
            .unreachable_ => |u| .{ .unreachable_ = .{ .holder = u.holder } },
        };
    }

    /// After a `NotHolder{current}`: a joiner asks the manager, the
    /// manager re-reads its own table.
    fn refresh(ctx: *anyopaque, slot: u16, _: u32) tokens.divert.Home {
        const self: *LocalResolver = @ptrCast(@alignCast(ctx));
        const be = self.plane.set.volumeOfSlot(slot);
        if (be.joined) |js| {
            _ = be.slots.cache.resolveViaManager(.{ .ctx = &js.link, .call = &joined.SharedLink.callFn }, be.index, slot) catch {};
        }
        return homeOf(ctx, slot);
    }

    fn dial(ctx: *anyopaque, _: tokens.table.ClientId, holder: u32) ?tokens.cache.Link {
        const self: *LocalResolver = @ptrCast(@alignCast(ctx));
        return self.plane.peers.linkForHolder(holder);
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
const slots_acquire = @import("slots/acquire.zig");

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

const TestPoison = struct {
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
        var gate = membership.park_reclaim.ParkGate.arm(s.io(), 0, membership.park_reclaim.tParkMaxMs(46_500, plane.mgr.?.owner.clocks.grace_ms));
        var member: shard.Member = .{ .gpa = gpa, .id = shard.writerMemberId(joinerIdentity()), .role = .writer, .link = tl.link(), .pid = 4242, .boot = @splat(1), .endpoint = .none, .pr_key = 0, .mount = "/mnt-b", .gate = &gate, .poison = TestPoison.sink(), .carriage = joiner.carriageSink() };
        const now = clock.monotonicNs() / std.time.ns_per_ms;
        try testing.expectEqual(shard.Member.JoinOutcome.granted, member.join(now, null));
        try testing.expectEqual(@as(usize, 1), plane.mgr.?.owner.members.items.len);
        try testing.expectEqual(shard.Liveness.live, plane.mgr.?.owner.liveness(member.id, now));
        const row = (try be0.lookup(gpa, key.TREE_CONTROL, cr.memberKey(.{ .node_token = joinerIdentity().node_token, .mount_slot = joinerIdentity().mount_slot }).slice())).?;
        defer gpa.free(row);
        try testing.expectEqual(@as(u32, 4242), (try shard.MemberRow.decode(row)).pid);
        plane.tick();
        try testing.expectEqual(@as(usize, 1), plane.mgr.?.owner.members.items.len);
        // The renewal beat over the wire: idle before the cadence, renewed
        // at it (the shard's deadline moves; nothing is evicted).
        try testing.expectEqual(shard.TickVerdict.idle, member.tick(now + 1, 0));
        const renewals0 = shard.stats.membership_renewals.load();
        try testing.expectEqual(shard.TickVerdict.renewed, member.tick(now + plane.mgr.?.owner.clocks.renew_ms + 1, 0));
        try testing.expectEqual(renewals0 + 1, shard.stats.membership_renewals.load());
        try testing.expectEqual(shard.Liveness.live, plane.mgr.?.owner.liveness(member.id, now + plane.mgr.?.owner.clocks.renew_ms + 1));
        // The token holder's lease oracle sees the member as live.
        try testing.expectEqual(tokens.table.LeaseVerdict.live, plane.oracle.oracle().verdict(plane.oracle.oracle().ctx, clientIdOf(member.id), 0, 0));
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
        try testing.expectEqual(@as(usize, 0), plane.mgr.?.owner.members.items.len);
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
        try testing.expect(plane.mgr.?.owner.grace == null);
        try testing.expect(!std.mem.eql(u8, ep_text_buf[0..ep_text_len], plane.endpointText()));
    }
}

test "linux: A JOINER'S CLEAN UNMOUNT AND REJOIN while the manager stays up — B joins over TCP (its rotor slot leased as appender 1) and writes into it, the manager checkpoints (tree 0's projection names B's lease), B unmounts clean (the lease released, the page Free: the live tree 0 Unleased, the projection on the device still Leased{1}), B mounts again under the same identity before the manager's next record: the hint hands it appender 1 again, the projection's row naming appender 1 is a previous incarnation's and loads unleased — the rejoin holds exactly its new grant, its first cycle publishes nothing the manager refuses, it mounts and writes" {
    const gpa = testing.allocator;
    const s = try meta_set.TestSet.create(gpa, "rejoin", 1);
    defer s.destroy();
    try s.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    // The manager's cadence held past the row: its one record below is the
    // row's own, so the projection B reads at its rejoin is the stale one
    // by construction (the venue's race: B remounting inside the manager's
    // 1 s cadence).
    try s.env.put("SQUEEZEFS_CHECKPOINT_MAX_AGE_MS", "600000");
    // One rotor slot: one grant, one release — no tree-0 split carries the
    // released row's leaf onto the device between the manager's records.
    try s.env.put("SQUEEZEFS_SYM_MINT_SLOTS", "1");
    const set = try s.open();
    defer set.close();
    var locks = try dlm.Manager.init(gpa, 64);
    defer locks.deinit();
    const bound = try xv.set_bind.Bound.create(gpa, s.io(), set, &locks, &s.env);
    defer bound.destroy(gpa);
    const plane = (try WirePlane.start(gpa, s.io(), set, &s.env, 4, 1 << 30, &bound.plane, false)).?;
    defer plane.stop();
    const be0 = set.volumes[0];
    const join_opts: backend.JoinOptions = .{ .mountpoint = "/build/tmp/mnt-rejoin-b", .endpoint = plane.advertised };
    const open_opts: meta_set.OpenOptions = .{ .env = &s.env, .cache_budget_bytes = 64 << 20, .cpus = 4, .join = join_opts };
    var first: std.ArrayList(u16) = .empty;
    defer first.deinit(gpa);
    var me: u32 = 0;
    {
        const b = try meta_set.MetaSet.open(gpa, s.io(), s.pathList(), open_opts);
        defer b.close();
        const bj = b.volumes[0];
        try testing.expectEqual(backend.Posture.joined, bj.posture);
        me = bj.joined.?.appenderId();
        try bj.slots.table.heldBy(me, &first);
        try testing.expectEqual(@as(usize, 1), first.items.len);
        // B writes into its slot (the tree its release names).
        const local = try bj.allocateInoInSlot(first.items[0]);
        const t = try bj.begin(@src());
        try t.stagePut(key.TREE_INODES, &key.inodeKey(key.guestLocalIno(first.items[0], local)), "first");
        try bj.commit(t);
        try bj.checkpointNow();
        // The manager's record: tree 0's projection names B's lease.
        try be0.checkpointNow();
    }
    // The precondition: the live tree 0 Unleased, the projection on the
    // device (what a rejoin reads) still Leased{appender 1}.
    try testing.expectEqual(slots_lt.State.unleased, be0.slots.table.view(first.items[0]).state);
    {
        const rd = try KvBackend.openReader(gpa, s.io(), s.paths[0], .{ .env = &s.env, .cache_budget_bytes = 16 << 20 });
        defer rd.close();
        const row = (try rd.lookup(gpa, key.TREE_CONTROL, cr.slotStateKey(first.items[0]).slice())).?;
        defer gpa.free(row);
        const st = try cr.SlotStateWire.decode(row);
        try testing.expect(st.variant == .leased and st.appender_id == me);
    }
    const stale0 = slots_acquire.stats.slot_projection_stale_self_rows.load();
    {
        const b = try meta_set.MetaSet.open(gpa, s.io(), s.pathList(), open_opts);
        defer b.close();
        const bj = b.volumes[0];
        try testing.expectEqual(me, bj.joined.?.appenderId());
        try testing.expectEqual(stale0 + 1, slots_acquire.stats.slot_projection_stale_self_rows.load());
        // Exactly this incarnation's grant is held — the previous one's row
        // reads unleased to the door (a first touch asks the manager).
        var held: std.ArrayList(u16) = .empty;
        defer held.deinit(gpa);
        try bj.slots.table.heldBy(me, &held);
        try testing.expectEqual(@as(usize, 1), held.items.len);
        try testing.expect(be0.slots.table.view(held.items[0]).leasedBy(me));
        if (held.items[0] != first.items[0]) try testing.expectEqual(slots_lt.State.unleased, bj.slots.table.view(first.items[0]).state);
        // It writes: a record in its new slot commits and the next cycle's
        // publication passes the manager's screen.
        const local = try bj.allocateInoInSlot(held.items[0]);
        const t = try bj.begin(@src());
        try t.stagePut(key.TREE_INODES, &key.inodeKey(key.guestLocalIno(held.items[0], local)), "rejoined");
        try bj.commit(t);
        try bj.checkpointNow();
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

/// A listener nobody serves: the dead incarnation's port (the dial is
/// refused at once — ECONNREFUSED is `HolderUnreachable`, never a wait).
fn deadEndpoint() endpoint.EndpointWire {
    var e: endpoint.EndpointWire = .none;
    e.family = endpoint.FAMILY_IP4;
    e.port = 1;
    e.addr[0..4].* = .{ 127, 0, 0, 1 };
    return e;
}

test "linux: THE SAME-NODE SUCCESSOR IS REACHED — a member joins at a dead incarnation's port and the plane's links to it (by token client id and by holder appender id) dial that port; its eviction retires the sessions and the manager's endpoint row through the death fan-out; the successor joins under the SAME member id and appender id at a new port and the next call on either link re-resolves off the roster / the RAM endpoint table and lands — never a frame into the dead port, never the lease TTL" {
    const gpa = testing.allocator;
    const s = try meta_set.TestSet.create(gpa, "wire-succ", 1);
    defer s.destroy();
    try s.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    const set = try s.open();
    defer set.close();
    var locks = try dlm.Manager.init(gpa, 64);
    defer locks.deinit();
    const bound = try xv.set_bind.Bound.create(gpa, s.io(), set, &locks, &s.env);
    defer bound.destroy(gpa);
    const plane = (try WirePlane.start(gpa, s.io(), set, &s.env, 4, 1 << 30, &bound.plane, false)).?;
    defer plane.stop();
    const be0 = set.volumes[0];
    const m = plane.mgr.?;
    // The region under the successor-shared identity (an in-process
    // joiner: the page is what `PublishEndpoint` binds the endpoint to).
    const r1 = try be0.sym.?.joinInProcess(0x5002);
    const ident = r1.identity;
    const wire_ident: verbs.WireIdentity = .{ .node_token = ident.node_token, .writer_id = ident.writer_id, .mount_slot = ident.mount_slot, .reserved = 0 };
    const id = shard.writerMemberId(wire_ident);
    const client = clientIdOf(id);
    const now = clock.monotonicNs() / std.time.ns_per_ms;
    // The first incarnation: a member at the dead port, appender 1's
    // endpoint published as the same port.
    try testing.expect(m.owner.join(.{ .id = id, .role = .writer, .endpoint = deadEndpoint(), .pid = 4242, .boot = @splat(1), .prior_epoch = null, .pr_key = 0, .mount = "/mnt-b", .row = null }, now) == .granted);
    var dead_ep: manager.Endpoint = .{};
    const dead_bytes = std.mem.asBytes(&deadEndpoint());
    @memcpy(dead_ep.bytes[0..dead_bytes.len], dead_bytes);
    dead_ep.len = @intCast(dead_bytes.len);
    _ = try be0.sym.?.manager.publishEndpoint(.{ .identity = ident, .appender_id = r1.appenderId(), .endpoint = dead_ep, .pr_key = 0 });
    const by_client = PeerLinks.linkForClient(&plane.peers, client).?;
    const by_holder = plane.peers.linkForHolder(r1.appenderId()).?;
    try testing.expectError(error.HolderUnreachable, by_client.call(by_client.ctx, .{ .ping = .{} }));
    try testing.expectError(error.HolderUnreachable, by_holder.call(by_holder.ctx, .{ .ping = .{} }));
    try testing.expectEqual(@as(usize, 2), plane.peers.count());
    // The eviction at T_owner: the fan-out retires both sessions — and
    // the manager's row of the corpse's listener (rung 10 recovery item 5:
    // it stood until the region was recovered, naming the dead port).
    const invalidated0 = joined.stats.peer_links_invalidated.load();
    const retired0 = manager.stats.manager_endpoints_retired.load();
    try testing.expect((try be0.sym.?.manager.resolveEndpoint(.{ .appender_id = r1.appenderId() })).endpoint != null);
    try testing.expectEqual(@as(u32, 1), m.owner.expireDue(now + m.owner.clocks.t_owner_ms + 1));
    try testing.expectEqual(invalidated0 + 2, joined.stats.peer_links_invalidated.load());
    try testing.expect((try be0.sym.?.manager.resolveEndpoint(.{ .appender_id = r1.appenderId() })).endpoint == null);
    try testing.expectEqual(retired0 + 1, manager.stats.manager_endpoints_retired.load());
    // The successor: the same member id and appender id, the plane's own
    // listener as its port (a listener that answers).
    try testing.expect(m.owner.join(.{ .id = id, .role = .writer, .endpoint = plane.advertised, .pid = 4243, .boot = @splat(1), .prior_epoch = null, .pr_key = 0, .mount = "/mnt-b", .row = null }, now + m.owner.clocks.t_owner_ms + 2) == .granted);
    var live_ep: manager.Endpoint = .{};
    const live_bytes = std.mem.asBytes(&plane.advertised);
    @memcpy(live_ep.bytes[0..live_bytes.len], live_bytes);
    live_ep.len = @intCast(live_bytes.len);
    _ = try be0.sym.?.manager.publishEndpoint(.{ .identity = ident, .appender_id = r1.appenderId(), .endpoint = live_ep, .pr_key = 0 });
    // The SAME links (one session per peer) re-resolve and land.
    const repointed0 = joined.stats.peer_links_repointed.load();
    _ = try by_client.call(by_client.ctx, .{ .ping = .{} });
    _ = try by_holder.call(by_holder.ctx, .{ .ping = .{} });
    try testing.expectEqual(repointed0 + 2, joined.stats.peer_links_repointed.load());
    try testing.expectEqual(@as(usize, 2), plane.peers.count());
    try testing.expect(plane.peers.by_client.get(client).?.link.endpointNow().eql(plane.advertised));
    try testing.expect(plane.peers.by_holder.get(r1.appenderId()).?.link.endpointNow().eql(plane.advertised));
}

test "client ids: one per member id, stable, distinct across two members; the parked scopes' client key of a session's proven peer IS the token client id of that member (one membership verdict for both sweeps)" {
    const a: shard.MemberId = @splat(1);
    var b: shard.MemberId = @splat(1);
    b[15] = 2;
    try testing.expectEqual(clientIdOf(a), clientIdOf(a));
    try testing.expect(clientIdOf(a) != clientIdOf(b));
    const identity: verbs.WireIdentity = .{ .node_token = 0xa1afa2cc30f28bb6, .writer_id = 0x192f88cd, .mount_slot = 0xf44a79f8, .reserved = 0 };
    // The handshake proves the 12 B peer id; the writer id never travels.
    const proven = verbs.WireIdentity.fromPeerId(&identity.peerId()).?;
    try testing.expectEqual(clientIdOf(shard.writerMemberId(identity)), xv.guards.clientOf(proven));
}

test "the start refusals name their rung and cause" {
    try testing.expect(std.mem.indexOf(u8, startReason(error.MembershipOff), "rung 3") != null);
    try testing.expect(std.mem.indexOf(u8, startReason(error.EnrollMissing), "job:enroll") != null);
    try testing.expect(std.mem.indexOf(u8, startReason(error.MalformedBind), "SQUEEZEFS_MW_BIND") != null);
}
