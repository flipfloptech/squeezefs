//! The JOINED appender's symmetric plane (Linux; charter §5.7 "a later RW
//! mount JOINS it (`JoinAppender` over the wire presenting its own `(node,
//! mount_slot)` identity)"; §5.5 "Appenders", "Extent grants", "The
//! record-seq space is monotone across rings", PARK-AND-RECLAIM;
//! `docs/porting/sym_wire_membership_recovery.md` §5.2 the joined door;
//! `sym_appenders_manager.md` §2.5 what a checkpoint writes into the
//! page, §3.1 the joiner's side): what a second `squeezefs mount` of a
//! symmetric set holds beside its `KvBackend` — the counterpart of
//! `bind.AppenderSet` for a mount that did NOT win the D0 ladder.
//!
//! The joined door, in order (the spec's step table): no flock (the claim
//! IS the join: the manager's `JoinAppender` + the member row); the
//! manager's endpoint and the set's `job:enroll` secret read off tree 0's
//! checkpointed projection BEFORE any wire is held; ONE dialled session to
//! the manager (`SharedLink`: a thread-safe link any thread calls, a
//! `redial` any failed verb sets); `JoinAppender` → the region (ring
//! segments, the seq continuation, the node-seq incarnation) from the
//! REPLY's words — the RAM grant from the reply's grant word, never the
//! page read; `AcquireSlots(M)` → the rotor; the extent-grant POOL as a
//! grant MIRROR allocator (every extent of the volume claimed except the
//! granted runs: SMOs and mints draw from it, retired images recycle into
//! it on the joiner's OWN tail, the refill at 50 % and the surplus return
//! at the quiet cadence ride the cycle); the checkpoint writes the PAGE
//! as its ledger record (`checkpoint_cycle.Deps.ledger = null`) under the
//! directory-first law — roots, cursors, `ckpt_seq`, `ledger_tail_seq`,
//! the pool's largest runs; never the fixed ledger, the bitmap, tree 0 or
//! page 0. A clean `umount` releases every lease over the wire (words +
//! leaf tails), writes the page naming no slot, drains the ring at a
//! fixpoint cycle and leaves (`LeaveAppender` → the page Free).
//!
//! `writers_known` for `M` is the directory census at the join (Live pages
//! + this one): the joiner sees no manager RAM word.

const std = @import("std");
const fuzzkit = @import("../../core/fuzzkit.zig");
const builtin = @import("builtin");
const Allocator = std.mem.Allocator;
const Io = std.Io;

const counters = @import("../../core/counters.zig");
const clock = @import("../../core/clock.zig");
const log = @import("../../core/log.zig");
const knobs = @import("../../core/knobs.zig");
const errors = @import("../../core/errors.zig");
const checksum = @import("../../core/checksum.zig");
const sync = @import("../../sync/sync.zig");
const alloc_ext = @import("../alloc_ext.zig");
const backend = @import("../backend.zig");
const key = @import("../key.zig");
const ledger = @import("../ledger.zig");
const node_seq = @import("../node_seq.zig");
const smo = @import("../smo.zig");
const checkpoint_cycle = @import("../checkpoint_cycle.zig");
const claim = @import("../guard/claim.zig");
const proc = @import("../guard/proc.zig");
const sym = @import("../sym.zig");
const page = @import("appender_page.zig");
const dir = @import("appender_dir.zig");
const region = @import("region.zig");
const grant = @import("extent_grant.zig");
const ring_growth = @import("ring_growth.zig");
const checkpoint = @import("../checkpoint.zig");
const conveyor = @import("../conveyor.zig");
const join = @import("join.zig");
const manager = @import("manager.zig");
const cr = @import("control_records.zig");
const slots = @import("slots.zig");
const join_ladder = @import("membership/join_ladder.zig");
const ro_mount = @import("tokens/ro_mount.zig");
const shard = @import("membership/shard.zig");
const park = @import("membership/park_reclaim.zig");
const wire_auth = @import("../../wire/auth.zig");
const dialer = @import("../../wire/dialer.zig");
const endpoint = @import("../../wire/endpoint.zig");
const verbs = @import("../../wire/verbs.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("sym/joined is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

const KvBackend = backend.KvBackend;

pub const stats = struct {
    /// Volumes this process holds as a JOINED appender.
    pub var joined_volumes: counters.Gauge = .{};
    /// The pool's words (closure `granted ≡ held + unclaimed + returned`
    /// read off the mirror; the identity is asserted at every refill).
    pub var extent_grant_refills: counters.ShardedCounter = .{};
    pub var extent_grant_returns: counters.ShardedCounter = .{};
    pub var extent_grant_closure_violations: counters.Tripwire = .init("extent_grant_closure_violations");
    /// The measured SMO rate the pool target derives from (milli-images/s).
    pub var extent_grant_smo_ewma_milli_per_s: counters.Gauge = .{};
    /// Asks the manager answered `Rejected` as past its heap-share cap —
    /// re-asked as the cap itself.
    pub var extent_grant_asks_capped: counters.ShardedCounter = .{};
    /// Pool cadences the mirror's low-water hook kicked (the proactive
    /// refill ahead of the checkpoint clock).
    pub var extent_grant_refill_kicks: counters.ShardedCounter = .{};
    /// The manager's endpoint was not yet published when the joiner read
    /// tree 0, or the published one answered no dial (a previous
    /// incarnation's): one bounded re-read of the projection per poll.
    pub var joined_endpoint_polls: counters.ShardedCounter = .{};
    /// Leases released over the wire at the clean leave.
    pub var joined_leave_releases: counters.ShardedCounter = .{};
    /// A peer link re-pointed at a listener that moved (a same-node
    /// successor's new port under the old identity included).
    pub var peer_links_repointed: counters.ShardedCounter = .{};
    /// Peer links retired at a member's eviction (the death fan-out).
    pub var peer_links_invalidated: counters.ShardedCounter = .{};
    /// Reader planes a joiner dropped (every token void) because the
    /// owner's census retired the member whose listener they were bound
    /// to: a dead holder's grants are recalled by no one once its slots
    /// are recovered.
    pub var joined_dead_holder_planes_dropped: counters.ShardedCounter = .{};
    /// Ring growths this joiner applied (`growRingIfDue`: the manager's
    /// segment named by the page and the journal swapped onto it).
    pub var joined_ring_grows: counters.ShardedCounter = .{};
    /// The manager declined a growth (no room under the ring budget, a
    /// short run, the table full): asked again only at a fresh stall or a
    /// changed ring.
    pub var joined_ring_grow_declined: counters.ShardedCounter = .{};
    /// A growth whose drain did not reach the fixpoint inside one landing
    /// ceiling, or whose verb failed: the gate reopened, retried next cycle.
    pub var joined_ring_grow_deferrals: counters.ShardedCounter = .{};
    /// The measured commit rate the ring derives from (bytes/s, EWMA).
    pub var joined_commit_ewma_bytes_per_s: counters.Gauge = .{};
};

/// `mount_slot` of a joiner (charter §5.7 "a remount at another mount
/// point is a same-node successor": the slot derives from the mount
/// point): bit 31 set so it never collides with a manager's slot — the
/// manager's is its lane index (< 65,536) — then the mount point's hash.
pub const JOINER_MOUNT_SLOT_BIT: u32 = 1 << 31;

pub fn joinerMountSlot(mountpoint: []const u8) u32 {
    return JOINER_MOUNT_SLOT_BIT | (@as(u32, @truncate(checksum.hash64(mountpoint))) & (JOINER_MOUNT_SLOT_BIT - 1));
}

/// How long a joiner polls the projection for the manager's endpoint
/// record: three checkpoint cadences (the record is a ring-0 control
/// entry the manager's next cycle lands; the joiner has no ring replay).
pub const ENDPOINT_POLL_MAX_MS: u64 = 3 * checkpoint.CHECKPOINT_MAX_AGE_MS;
pub const ENDPOINT_POLL_STEP_MS: u64 = 100;

/// The grant mirror's compaction reserve: one split's two images — the
/// joiner's SMO floor under the smallest pool (8 + M); the manager's
/// refill is the pool's liveness, so the heap law's 2 % reserve (sized to
/// a whole heap) would starve a 12-extent pool.
pub const MIRROR_RESERVE_EXTENTS: u64 = 2;

/// A wire reply's widest payload: the control frame's cap — a token grant
/// page is budgeted at HALF of it (`token_table.GRANT_PAGE_BUDGET`: the
/// dentries of one page of a wide directory), and a reply wider than the
/// caller's buffer is `ValueTooLarge` at the mux, which the reader plane
/// took for a dead channel (a 4,000-entry directory read from a peer
/// answered EIO, then EAGAIN for ever at the 64 KiB cap). Per thread,
/// touched only by threads that call (the TLS pages stay virtual).
pub const REPLY_CAP: usize = @import("../../wire/frame.zig").CONTROL_CAP;

comptime {
    std.debug.assert(REPLY_CAP >= 2 * sym.tokens.table.GRANT_PAGE_BUDGET);
}

pub const OpenError = error{
    /// No manager endpoint published on tree 0 within the poll bound (and
    /// none given): the D0 winner has not armed its wire yet, or died.
    ManagerUnreachable,
    /// The manager named the volume count as the lever.
    Capacity,
    /// A `Joined` word failed the screen, or the wire schema refused.
    Rejected,
    /// This mount slot's previous incarnation is Live and unrecovered:
    /// its ring is the recovery driver's before a rejoin may replay it.
    PredecessorLive,
    Io,
    Corrupt,
    OutOfMemory,
};

// ---------------------------------------------------------------------------
// The shared link: one dialled session, any thread.
// ---------------------------------------------------------------------------

/// The peer's CURRENT listener off authoritative state (the manager's RAM
/// endpoint table, the owner's roster, `ResolveEndpoint` on a joiner),
/// consulted before every (re)dial — charter §5.5 "Readers' planes FOLLOW
/// a holder that moved its listener (re-resolve off durable state,
/// re-point in place)": a same-node successor under the same identity
/// publishes a new ephemeral port under the old appender id and client
/// id, and a link that kept dialling the dead incarnation's port answered
/// `unreachable_` for every recall and step until the lease TTL. Null =
/// no listener is known: unreachable, never a dial of a stale one.
pub const EndpointResolver = struct {
    ctx: *anyopaque,
    resolve: *const fn (ctx: *anyopaque) ?endpoint.EndpointWire,
};

/// A thread-safe `Link` over ONE dialled session to a peer: the reply
/// lands in the calling thread's own buffer (valid until that thread's
/// next call — the `TcpLink` contract per thread), the mux multiplexes the
/// calls, a poisoned mux is replaced under the lock and retired (never
/// freed under a caller still reading its cell). A link with a resolver
/// re-resolves the endpoint before every dial; one without dials its
/// fixed endpoint (the joiner's manager link).
pub const SharedLink = struct {
    gpa: Allocator,
    io: Io,
    ep: endpoint.EndpointWire,
    secret: wire_auth.Secret,
    identity: verbs.WireIdentity,
    resolver: ?EndpointResolver = null,
    lock: Io.Mutex = .init,
    mux: ?*dialer.Mux = null,
    retired: std.ArrayList(*dialer.Mux) = .empty,

    /// The reply a call decodes into (borrowed until this thread's next
    /// call): per thread, mapped at its first call (`mem/lazy.zig` — a
    /// `threadlocal` array was 1 MiB resident in every thread of the
    /// process).
    const ReplyBuf = @import("../../mem/lazy.zig").PerThread(REPLY_CAP);

    pub fn init(gpa: Allocator, io: Io, ep: endpoint.EndpointWire, secret: wire_auth.Secret, identity: verbs.WireIdentity) SharedLink {
        return .{ .gpa = gpa, .io = io, .ep = ep, .secret = secret, .identity = identity };
    }

    pub fn deinit(self: *SharedLink) void {
        if (self.mux) |m| m.close();
        self.mux = null;
        for (self.retired.items) |m| m.close();
        self.retired.deinit(self.gpa);
    }

    /// The peer's listener is gone (the owner evicted the member): the
    /// session is retired NOW, the next call re-resolves and dials afresh
    /// — never a frame into a dead incarnation's socket.
    pub fn invalidate(self: *SharedLink) void {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        const m = self.mux orelse return;
        self.retired.append(self.gpa, m) catch m.close();
        self.mux = null;
    }

    /// The endpoint the next dial takes (the current one; for tests).
    pub fn endpointNow(self: *SharedLink) endpoint.EndpointWire {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        return self.ep;
    }

    /// Dial now (re-resolving first) without a verb: the session stands
    /// for the next call.
    pub fn dial(self: *SharedLink) errors.Error!void {
        _ = try self.ensure();
    }

    fn ensure(self: *SharedLink) errors.Error!*dialer.Mux {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        if (self.mux) |m| {
            if (!m.isPoisoned()) return m;
            self.retired.append(self.gpa, m) catch m.close();
            self.mux = null;
            @import("../../wire/stats.zig").cluster_wire_reconnects.inc();
        }
        if (self.resolver) |r| {
            const ep = r.resolve(r.ctx) orelse return error.HolderUnreachable;
            if (!ep.eql(self.ep)) {
                stats.peer_links_repointed.inc();
                self.ep = ep;
            }
        }
        const peer_id = self.identity.peerId();
        const m = try dialer.connect(self.gpa, self.io, self.ep, &self.secret, &peer_id);
        self.mux = m;
        return m;
    }

    pub fn call(self: *SharedLink, req: verbs.Request) errors.Error!verbs.Reply {
        const m = try self.ensure();
        const reply_buf = ReplyBuf.get() catch return error.OutOfMemory;
        return m.callTyped(req, reply_buf);
    }

    pub fn callFn(ctx: *anyopaque, req: verbs.Request) errors.Error!verbs.Reply {
        const self: *SharedLink = @ptrCast(@alignCast(ctx));
        return self.call(req);
    }

    pub fn link(self: *SharedLink) shard.Link {
        return .{ .ctx = self, .call = &callFn };
    }

    pub fn tokenLink(self: *SharedLink) sym.tokens.cache.Link {
        return .{ .ctx = self, .call = &callFn };
    }
};

// ---------------------------------------------------------------------------
// The joined set.
// ---------------------------------------------------------------------------

/// What the backend reads off the join to stand its ring and allocator up.
pub const Standing = struct {
    appender_id: u32,
    ring_layout: @import("../journal.zig").RingLayout,
    geo: @import("../journal_core.zig").CoreGeometry,
    head_pos: u64,
    tail_pos: u64,
    seq_offset: u64,
    ckpt_seq: u64,
    seq: node_seq.NodeSeqHandle,
    /// The pool as granted, extent indices.
    granted: []const @import("../alloc_ext.zig").ExtentAllocator.GrantedRun,
};

pub const JoinedSet = struct {
    gpa: Allocator,
    be: *KvBackend,
    identity: page.Identity,
    wire_identity: verbs.WireIdentity,
    /// The manager's identity as appender 0's page carried it at the join
    /// (its member id, its token client id).
    manager_identity: verbs.WireIdentity,
    /// The mount point (the member row's `mount`).
    mountpoint: []u8,
    secret: wire_auth.Secret,
    link: SharedLink,
    /// The manager link's re-resolution off the device (the S5 epoch
    /// poll + the `endpoint:{0}` record at the newest checkpoint).
    manager_resolver: ManagerResolver,
    wire: join_ladder.ManagerWire,
    joiner: join_ladder.Joiner,
    /// Live pages on the directory at the join (other appenders) + 1.
    writers_known: u64,
    /// The mint-slot count the arm granted (`M`).
    m: u64 = 0,
    /// A re-join in place: the evicted predecessor's appender id (its
    /// projection rows load unleased, as this appender's own).
    predecessor: ?u32 = null,
    /// Extent indices granted since the join (every run absorbed) and
    /// returned to the manager — the closure's two census words; `held`
    /// and `unclaimed` are read off the mirror.
    granted: u64 = 0,
    returned: u64 = 0,
    granted_bits: std.DynamicBitSetUnmanaged,
    /// Guards `granted_bits`, `granted` and `returned` across bit
    /// operations only — never across a wire call — so the SMO context's
    /// retirement guard (`grantHolds`) reads under it without waiting on
    /// the manager. Order: `pool_lock` → `bits_lock`; the SMO lock →
    /// `bits_lock`.
    bits_lock: Io.Mutex = .init,
    pool_lock: Io.Mutex = .init,
    /// The measured SMO rate (milli-images/s; charter §5.5 "a WIRE joiner
    /// measures its own SMO rate and names its size"), folded per cadence
    /// off the mirror's claims (0 until the first cadence: the floor).
    smo_ewma_milli_per_s: u64 = 0,
    /// The fold's marks: the mirror's claim count and the clock at the
    /// last cadence; surplus withdrawals are claims that are not images.
    rate_mark_claims: u64 = 0,
    rate_mark_ns: u64 = 0,
    rate_withdrawn: u64 = 0,
    /// The pool a CAPPED grant left (`grant.joinedCappedTarget`): the
    /// target's ceiling until an ask fits under the manager's cap again.
    /// Under `pool_lock`.
    capped_pool: ?u64 = null,
    /// The proactive refill's flight: the mirror's low-water hook kicks
    /// ONE pool cadence when a claim crosses the refill mark (the pool's
    /// state, not the checkpoint clock, decides).
    refill: sync.TaskSet = .init,
    refill_busy: std.atomic.Value(bool) = .init(false),
    /// The mirror carries the hook (`armPoolRefill`): uninstalled at deinit.
    pool_armed: bool = false,
    /// The last page write's words (for `.stats`).
    tail_written: u64 = 0,
    entries: u64 = 0,
    left: bool = false,
    /// The park-and-reclaim gate (charter §5.5 "A manager's death is its
    /// home shard's outage"): the conveyor's pre-admission gate; the
    /// member's `T_self` action parks it, the reclaim releases it,
    /// `T_park_max` expires it. Armed here with the env-derived clocks; the
    /// membership grant's `T_owner` refines `t_park_max_ms`.
    park_gate: park.ParkGate,
    /// The ring-growth gate (`ring_growth.zig`): the conveyor's passes
    /// enter it; `growRingIfDue` closes it to drain and swap the ring.
    gate: ring_growth.Gate,
    /// The growth decision's words (spec §3.3; touched by the checkpoint
    /// task alone): the stall count as of the last decision, the ring size
    /// the manager last declined at, the commit-rate EWMA and its fold
    /// marks (the head and the clock at the last fold).
    stalls_at_last_grow: u64 = 0,
    grow_declined_at: u64 = ring_growth.NEVER_DECLINED,
    commit_ewma_bytes_per_s: u64 = 0,
    head_at_last_fold: ?u64 = null,
    fold_mark_ns: u64 = 0,
    /// The checkpoint seq the growth was last judged at: one judgment per
    /// CYCLE (the reference's cadence — a stall's park-kicked cycle lands
    /// first, then the growth), never per 50 ms tick.
    grow_judged_at_seq: u64 = 0,

    /// The joined door (module doc): the census, the endpoint and the
    /// secret off the projection, the dial, `JoinAppender`. The region's
    /// words are read through `standing()`; the slots through
    /// `acquireRotor`. Nothing of the backend's ring exists yet.
    pub fn open(be: *KvBackend, mountpoint: []const u8, explicit_ep: ?endpoint.EndpointWire, predecessor: ?u32, set_secret: ?wire_auth.Secret) OpenError!*JoinedSet {
        const gpa = be.gpa;
        const self = try gpa.create(JoinedSet);
        errdefer gpa.destroy(self);
        const local = proc.localIdentity();
        const mount_slot = joinerMountSlot(mountpoint);
        const identity = join.identityFor(local, mount_slot);
        const wire_identity: verbs.WireIdentity = .{ .node_token = identity.node_token, .writer_id = identity.writer_id, .mount_slot = identity.mount_slot, .reserved = 0 };
        // 1. The census: `writers_known`, a Live page under THIS mount slot
        // (a predecessor's unrecovered ring), the manager's identity off
        // appender 0's page.
        var writers_known: u64 = 1;
        var own_live: ?page.Page = null;
        try self.census(be, identity, &writers_known, &own_live);
        if (own_live) |p| {
            log.err("meta volume {s}: appender {d} (term {d}) is LIVE on the directory under this mount point's identity — a previous incarnation's ring is unrecovered; the recovery ladder recovers it before a rejoin may replay it", .{ be.path, p.appender_id, p.term });
            return error.PredecessorLive;
        }
        holdJoinAppenderIfArmed(be);
        const page0 = (try readAppender0(gpa, be)) orelse {
            log.err("meta volume {s}: appender 0's page is blank — no manager has ever mounted this volume; the D0 ladder is the first writer's", .{be.path});
            return error.ManagerUnreachable;
        };
        if (page0.state != .live) {
            log.err("meta volume {s}: appender 0's page is {s} — no live manager to join; the D0 ladder is this mount's", .{ be.path, @tagName(page0.state) });
            return error.ManagerUnreachable;
        }
        // 2. The secret (the set's, off volume 0's tree 0: a member past
        // volume 0 carries none of its own) and the manager's endpoint off
        // this volume's projection.
        const secret = set_secret orelse (try readControl(gpa, be, wire_auth.ENROLL_KEY, wire_auth.Secret, wire_auth.decodeEnroll)) orelse {
            log.err("meta volume {s}: tree 0 carries no decodable job:enroll record — formatted before the cluster wire existed; reformat", .{be.path});
            return error.Corrupt;
        };
        const ep = explicit_ep orelse try awaitManagerEndpoint(gpa, be);
        const owned_mp = try gpa.dupe(u8, mountpoint);
        errdefer gpa.free(owned_mp);
        self.* = .{
            .gpa = gpa,
            .be = be,
            .identity = identity,
            .wire_identity = wire_identity,
            .manager_identity = .{ .node_token = page0.identity.node_token, .writer_id = page0.identity.writer_id, .mount_slot = page0.identity.mount_slot, .reserved = 0 },
            .mountpoint = owned_mp,
            .secret = secret,
            .link = SharedLink.init(gpa, be.io, ep, secret, wire_identity),
            .manager_resolver = undefined,
            .wire = undefined,
            .joiner = undefined,
            .writers_known = writers_known,
            .predecessor = predecessor,
            .granted_bits = .{},
            .park_gate = park.ParkGate.arm(be.io, be.stamp.member_position, park.tParkMaxMs(grant.failoverBoundMs(0, 0), park.graceMs(shard.tOwnerMsFromEnv(be.env)))),
            .gate = ring_growth.Gate.init(be.io),
        };
        errdefer self.link.deinit();
        // `--join` names a listener the record may not yet carry: the
        // poll starts at the mounted record, so a later record (a
        // successor manager's first checkpoint) is what re-points.
        self.manager_resolver = ManagerResolver.init(be, &self.link);
        self.link.resolver = self.manager_resolver.face();
        self.granted_bits = std.DynamicBitSetUnmanaged.initEmpty(gpa, be.sb.totalExtents()) catch return error.OutOfMemory;
        errdefer self.granted_bits.deinit(gpa);
        self.wire = .{ .gpa = gpa, .link = self.link.link(), .volume = be.index, .identity = wire_identity, .pages = .{ .ctx = self, .read = &readPageFn } };
        self.joiner = join_ladder.Joiner.init(gpa, &self.wire);
        errdefer self.joiner.deinit();
        try self.awaitManagerListener(page0);
        // 3. Rung 5: the region over the wire. A Deferred join — the
        // manager's ring 0 full for a beat, or this pair's predecessor still
        // owed or in its recovery (its death record standing, decision 0003
        // §CY) — is retried up to the park bound.
        const join_deadline = clock.monotonicNs() + self.park_gate.t_park_max_ms * std.time.ns_per_ms;
        var join_deferrals: u64 = 0;
        while (true) {
            self.joiner.joinRegion(.{
                .identity = identity,
                .ring_want_bytes = try ringAsk(be),
                .home_volume = be.stamp.member_position,
                .volume_base = node_seq.nodeSeqBase(be.sb.uuid),
                .grain = be.dev.mode.grain(),
                .ring0_head = be.mounted.journal_tail_seq,
            }) catch |e| {
                if (e == error.Deferred and clock.monotonicNs() < join_deadline) {
                    if (join_deferrals == 0) log.info("meta volume {s}: JoinAppender deferred by the manager — its ring 0 full, or this mount's predecessor (node 0x{x}, mount slot 0x{x}) not yet recovered; retrying for up to {d} ms", .{ be.path, identity.node_token, identity.mount_slot, self.park_gate.t_park_max_ms });
                    join_deferrals += 1;
                    be.io.sleep(.fromMilliseconds(ENDPOINT_POLL_STEP_MS), .awake) catch {};
                    continue;
                }
                return switch (e) {
                    error.Deferred => blk: {
                        log.err("meta volume {s}: JoinAppender still deferred after {d} ms ({d} retries) — the manager has not recovered this mount's predecessor", .{ be.path, self.park_gate.t_park_max_ms, join_deferrals });
                        break :blk error.Io;
                    },
                    error.Capacity => blk: {
                        log.err("meta volume {s}: JoinAppender refused for capacity — the lever is the metadata volume COUNT", .{be.path});
                        break :blk error.Capacity;
                    },
                    error.Rejected => error.Rejected,
                    error.Io => error.Io,
                    error.Corrupt => error.Corrupt,
                    error.OutOfMemory => error.OutOfMemory,
                };
            };
            break;
        }
        const jr = &self.joiner.region.?;
        for (jr.grant.unclaimed.items) |e| {
            self.granted_bits.set(@intCast(e));
            self.granted += 1;
        }
        stats.joined_volumes.add(1);
        sym.stats.appenders_live.add(1);
        log.info("meta volume {s}: JOINED as appender {d} (term {d}, node 0x{x}, mount slot 0x{x}) at {f}: ring {d} B in {d} segment(s), head {d}, grant {d} extent(s), incarnation base {x}; {d} writer(s) known", .{ be.path, jr.appenderId(), jr.core.page.term, identity.node_token, identity.mount_slot, ep, jr.core.ringBytes(), jr.core.table.len, jr.core.core.headPos(), self.granted, jr.seq.load(), writers_known });
        return self;
    }

    /// `SQUEEZEFS_TEST_JOIN_APPENDER_HOLD`: the open parks between its
    /// census and its JoinAppender, when the FIFO is present, until a
    /// writer opens it.
    fn holdJoinAppenderIfArmed(be: *KvBackend) void {
        const path = knobs.parseStr(be.env.get("SQUEEZEFS_TEST_JOIN_APPENDER_HOLD")) orelse return;
        Io.Dir.cwd().access(be.io, path, .{}) catch return;
        log.warn("meta volume {s}: SQUEEZEFS_TEST_JOIN_APPENDER_HOLD — the JoinAppender holds (the census read, no page Live under this mount's identity yet) until a writer opens {s}", .{ be.path, path });
        const f = Io.Dir.cwd().openFile(be.io, path, .{}) catch |e| {
            log.warn("meta volume {s}: the JoinAppender hold could not open {s} ({t}); not holding", .{ be.path, path, e });
            return;
        };
        f.close(be.io);
        log.warn("meta volume {s}: SQUEEZEFS_TEST_JOIN_APPENDER_HOLD released — the JoinAppender proceeds", .{be.path});
    }

    /// What a joiner may rely on from the manager across its incarnations:
    /// appender 0's page names WHO manages (rewritten at every incarnation's
    /// bind — term + 1, its writer id — before its listener exists), never
    /// WHERE it listens. `endpoint:{0}` is read off the projection, which
    /// names the listener whose publish the newest checkpoint covers; the
    /// manager lands a moved listener's publish before its plane arms
    /// (`wire_plane.publishEndpoints`), but a join racing the manager's
    /// bring-up (its page Live under the new term, its publish not yet
    /// checkpointed) reads the previous incarnation's dead port. So the
    /// projected endpoint is a HINT until a dial proves it: a dial nobody
    /// answers re-resolves (the resolver re-reads `endpoint:{0}` at every
    /// newer checkpoint) and redials, bounded by the endpoint poll; past
    /// it the manager is unreachable, never a device error.
    fn awaitManagerListener(self: *JoinedSet, page0: page.Page) OpenError!void {
        const deadline = clock.monotonicNs() + ENDPOINT_POLL_MAX_MS * std.time.ns_per_ms;
        while (true) {
            self.link.dial() catch |e| switch (e) {
                error.HolderUnreachable, error.ListenerRefused => {
                    if (clock.monotonicNs() >= deadline) {
                        log.err("meta volume {s}: the manager's listener {f} (endpoint:{{0}} at the newest checkpoint) answered no dial within {d} ms — appender 0's page is Live under term {d}, but no checkpoint names a listener that answers", .{ self.be.path, self.link.endpointNow(), ENDPOINT_POLL_MAX_MS, page0.term });
                        return error.ManagerUnreachable;
                    }
                    stats.joined_endpoint_polls.inc();
                    self.be.io.sleep(.fromMilliseconds(ENDPOINT_POLL_STEP_MS), .awake) catch {};
                    continue;
                },
                error.OutOfMemory => return error.OutOfMemory,
                else => return error.Io,
            };
            return;
        }
    }

    pub fn deinit(self: *JoinedSet) void {
        self.refill.cancel(self.be.io);
        if (self.pool_armed) self.be.alloc.uninstallLowWater();
        self.joiner.deinit();
        self.link.deinit();
        self.granted_bits.deinit(self.gpa);
        self.gpa.free(self.mountpoint);
        stats.joined_volumes.sub(1);
        sym.stats.appenders_live.sub(1);
        self.gpa.destroy(self);
    }

    pub fn appenderId(self: *const JoinedSet) u32 {
        return self.joiner.region.?.appenderId();
    }

    pub fn core(self: *JoinedSet) *region.RegionCore {
        return &self.joiner.region.?.core;
    }

    /// The region's words the backend stands its ring and allocator on.
    pub fn standing(self: *JoinedSet) Allocator.Error!Standing {
        const jr = &self.joiner.region.?;
        var runs: std.ArrayList(grant.GrantRun) = .empty;
        defer runs.deinit(self.gpa);
        try jr.grant.unclaimedRuns(self.gpa, &runs);
        const granted = try self.gpa.alloc(@import("../alloc_ext.zig").ExtentAllocator.GrantedRun, runs.items.len);
        for (runs.items, granted) |r, *g| g.* = .{ .start = r.start, .len = r.len };
        return .{
            .appender_id = jr.appenderId(),
            .ring_layout = jr.core.ring_layout,
            .geo = jr.core.geo,
            .head_pos = jr.core.core.headPos(),
            .tail_pos = jr.core.page.ledger_tail_seq,
            .seq_offset = jr.core.page.seq_offset,
            .ckpt_seq = jr.core.page.ckpt_seq,
            .seq = jr.seq,
            .granted = granted,
        };
    }

    /// Rung 6: `M` rotor slots over the wire (the grants are the plane's
    /// to install; owned by the caller).
    pub fn acquireRotor(self: *JoinedSet) errors.Error![]const join_ladder.ManagerWire.SlotGrant {
        self.m = slots.acquire.resolveMintSlots(knobs.optIntKnob("SQUEEZEFS_SYM_MINT_SLOTS", u64, self.be.env), self.writers_known);
        _ = try self.joiner.acquireRotor(@intCast(@min(self.m, std.math.maxInt(u16))));
        return self.joiner.leased.items;
    }

    /// The manager's listener as this mount reaches it NOW (the link's
    /// current endpoint; a re-resolution moves it).
    pub fn managerEndpoint(self: *JoinedSet) endpoint.EndpointWire {
        return self.link.endpointNow();
    }

    // ---- the census ----

    fn census(self: *JoinedSet, be: *KvBackend, identity: page.Identity, writers_known: *u64, own_live: *?page.Page) OpenError!void {
        _ = self;
        var links: std.ArrayList(dir.Link) = .empty;
        defer links.deinit(be.gpa);
        var reader_ctx: DevicePages = .{ .be = be };
        const reader: dir.PageReader = .{ .ctx = &reader_ctx, .read = &DevicePages.read };
        dir.walkChain(be.gpa, reader, .{ .start = be.sb.appender_dir.start, .len = be.sb.appender_dir.len }, be.sb.node_size, .{ .start = be.sb.heap.start, .len = be.sb.heap.len }, be.sb.totalExtents(), &links) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
            error.Corrupt => blk: {
                log.err("meta volume {s}: the appender directory chain does not verify — refusing the join", .{be.path});
                break :blk error.Corrupt;
            },
        };
        for (links.items) |l| {
            var pair: u32 = 0;
            while (pair < l.header.pairs) : (pair += 1) {
                const imgs = dir.readPair(reader, l, pair) catch return error.Io;
                if (imgs.foreign) return error.Corrupt;
                const p = imgs.newest() orelse continue;
                if (p.state != .live) continue;
                writers_known.* += 1;
                if (p.identity.sameMount(identity)) own_live.* = p;
            }
        }
        // Appender 0 (the manager) is Live by definition of a joined open;
        // the directory holds appenders ≥ 1, so it is counted here.
        writers_known.* += 1;
    }

    const DevicePages = struct {
        be: *KvBackend,

        fn read(ctx: *anyopaque, offset: u64, out: *[page.PAGE_LEN]u8) error{Io}!void {
            const self: *DevicePages = @ptrCast(@alignCast(ctx));
            const img = self.be.dev.readRange(self.be.gpa, offset, page.PAGE_LEN) catch return error.Io;
            defer self.be.gpa.free(img);
            @memcpy(out, img[0..page.PAGE_LEN]);
        }
    };

    fn readPageFn(ctx: *anyopaque, offset: u64, out: *[page.PAGE_LEN]u8) error{Io}!void {
        const self: *JoinedSet = @ptrCast(@alignCast(ctx));
        var pages: DevicePages = .{ .be = self.be };
        return DevicePages.read(&pages, offset, out);
    }

    /// `SQUEEZEFS_SYM_RING_KB`: explicit wins inside the volume's ceiling
    /// and refuses above it; absent = 0 = the manager derives.
    fn ringAsk(be: *KvBackend) OpenError!u64 {
        const kib = knobs.optIntKnob("SQUEEZEFS_SYM_RING_KB", u64, be.env) orelse return 0;
        return region.ringBytesExplicit(kib, be.sb.volume_len) catch {
            log.err("meta volume {s}: SQUEEZEFS_SYM_RING_KB={d} is above this volume's ring ceiling of {d} bytes — refusing the join", .{ be.path, kib, region.ringCeiling(be.sb.volume_len) });
            return error.Rejected;
        };
    }

    // ---- the pool: the grant mirror's census, the refill, the return ----

    /// The pool's unclaimed runs as extent indices (granted ∧ free in the
    /// mirror), coalesced ascending.
    pub fn unclaimedRuns(self: *JoinedSet, out: *std.ArrayList(grant.GrantRun)) Allocator.Error!void {
        self.bits_lock.lockUncancelable(self.be.io);
        defer self.bits_lock.unlock(self.be.io);
        var it = self.granted_bits.iterator(.{});
        while (it.next()) |idx| {
            if (self.be.alloc.isAllocated(idx)) continue;
            if (out.items.len > 0 and out.items[out.items.len - 1].end() == idx) {
                out.items[out.items.len - 1].len += 1;
            } else try out.append(self.gpa, .{ .start = idx, .len = 1 });
        }
    }

    fn unclaimedCount(self: *JoinedSet) u64 {
        self.bits_lock.lockUncancelable(self.be.io);
        defer self.bits_lock.unlock(self.be.io);
        return self.unclaimedCountLocked();
    }

    fn unclaimedCountLocked(self: *JoinedSet) u64 {
        var n: u64 = 0;
        var it = self.granted_bits.iterator(.{});
        while (it.next()) |idx| {
            if (!self.be.alloc.isAllocated(idx)) n += 1;
        }
        return n;
    }

    /// `held + unclaimed + returned ≡ granted` over the mirror: held = the
    /// granted extents the mirror shows claimed (images, pending frees).
    fn checkClosure(self: *JoinedSet) void {
        if (!self.closureHolds()) stats.extent_grant_closure_violations.fire("the joiner's grant pool lost or gained an extent outside a grant or a return");
    }

    pub fn closureHolds(self: *JoinedSet) bool {
        self.bits_lock.lockUncancelable(self.be.io);
        defer self.bits_lock.unlock(self.be.io);
        const unclaimed = self.unclaimedCountLocked();
        const held = self.granted_bits.count() - unclaimed;
        return held + unclaimed + self.returned == self.granted;
    }

    /// A slot lease's custody move as this mount's grant sees it (0003
    /// §AD.1; the manager's entry moved the record): IN — the installed
    /// tree's live images join the grant as HELD (the mirror shows them
    /// claimed: they are images), so an SMO of ours that retires one
    /// recycles an extent the record grants; OUT — the released tree's
    /// images leave it (the closure reads them as returned; the mirror
    /// keeps them claimed — the tree's next maintainer owns them).
    pub fn transferImages(self: *JoinedSet, images: []const u64, way: manager.Manager.ImagesMove) void {
        {
            self.bits_lock.lockUncancelable(self.be.io);
            defer self.bits_lock.unlock(self.be.io);
            var n: u64 = 0;
            for (images) |e| {
                const idx: usize = @intCast(e);
                switch (way) {
                    .gain => if (!self.granted_bits.isSet(idx)) {
                        self.granted_bits.set(idx);
                        n += 1;
                    },
                    .lose => if (self.granted_bits.isSet(idx)) {
                        self.granted_bits.unset(idx);
                        n += 1;
                    },
                }
            }
            switch (way) {
                .gain => self.granted += n,
                .lose => self.returned += n,
            }
        }
        self.checkClosure();
    }

    /// The SMO context's retirement guard (`smo.GrantHolds`): an image this
    /// mount's SMO retires recycles into the pool only if the grant holds
    /// it — after the custody moves every image of a tree this mount
    /// leases does.
    pub fn grantHolds(self: *JoinedSet) smo.GrantHolds {
        return .{ .ctx = self, .holds = &holdsFn };
    }

    fn holdsFn(ctx: *anyopaque, extent: u64) bool {
        const self: *JoinedSet = @ptrCast(@alignCast(ctx));
        self.bits_lock.lockUncancelable(self.be.io);
        defer self.bits_lock.unlock(self.be.io);
        return extent < self.granted_bits.bit_length and self.granted_bits.isSet(@intCast(extent));
    }

    /// The pool's target (charter §5.5 "Extent grants"): the explicit
    /// knob, else `clamp(max(2 × ewma_smo_rate × failover_bound_s +
    /// promised, 8 + M), promised, GRANT_EXTENTS_MAX)` — the joiner
    /// measures its own rate and names its size; the heap-share cap is
    /// the manager's (an ask past it is `Rejected`; the joiner then asks
    /// the cap). A storm's joiner holds a failover's worth of images, so a
    /// manager outage shorter than the bound never answers the app
    /// `GrantExhausted` (0002 row 353).
    fn poolTarget(self: *JoinedSet) u64 {
        const bound = grant.failoverBoundMs(self.be.gate_ns / std.time.ns_per_ms, self.be.replayStats().replay_ns / std.time.ns_per_ms);
        const derived = grant.joinedGrantDerived(self.smo_ewma_milli_per_s, bound);
        return grant.resolveGrantExtents(knobs.optIntKnob("SQUEEZEFS_SYM_GRANT_EXTENTS", u64, self.be.env), grant.joinedPoolTarget(derived, self.be.cache.heapPromised(), grant.joinedPoolFloor(self.m), grant.GRANT_EXTENTS_MAX));
    }

    /// The SMO-rate fold at the cadence: the mirror's claims since the
    /// last mark, less the surplus withdrawals (claims that are returns,
    /// not images), over the elapsed time.
    fn foldSmoRate(self: *JoinedSet) void {
        self.pool_lock.lockUncancelable(self.be.io);
        defer self.pool_lock.unlock(self.be.io);
        const now = clock.monotonicNs();
        const claims = self.be.alloc.claimsTotal() -| self.rate_withdrawn;
        if (self.rate_mark_ns != 0) {
            self.smo_ewma_milli_per_s = grant.smoRateFold(self.smo_ewma_milli_per_s, claims -| self.rate_mark_claims, now -| self.rate_mark_ns);
            stats.extent_grant_smo_ewma_milli_per_s.set(self.smo_ewma_milli_per_s);
        }
        self.rate_mark_claims = claims;
        self.rate_mark_ns = now;
    }

    /// The mirror's low-water hook (`alloc_ext.LowWaterHook`), armed by the
    /// backend once the mirror stands: a claim that crosses the refill
    /// mark kicks the pool cadence at once on its own flight — the
    /// PROACTIVE refill the spec names (§5.2), driven by the pool's state
    /// rather than the checkpoint clock (a storm right after the join
    /// drains the floor's SMO headroom inside one cadence; a manager
    /// death in that window would otherwise answer the app `GrantExhausted`).
    pub fn armPoolRefill(self: *JoinedSet) void {
        self.be.alloc.installLowWater(.{ .ctx = self, .run = &lowWaterRun }, grant.joinedRefillMark(self.poolTarget(), self.be.cache.heapPromised()));
        self.pool_armed = true;
    }

    fn lowWaterRun(ctx: *anyopaque) void {
        const self: *JoinedSet = @ptrCast(@alignCast(ctx));
        if (self.left or self.refill_busy.swap(true, .acq_rel)) return;
        self.refill.spawn(self.be.io, refillRun, .{self}) catch self.refill_busy.store(false, .release);
    }

    fn refillRun(self: *JoinedSet) Io.Cancelable!void {
        defer self.refill_busy.store(false, .release);
        stats.extent_grant_refill_kicks.inc();
        self.poolCadence();
    }

    /// At the cadence (the checkpoint's page write) and on the low-water
    /// kick: refill when the unclaimed headroom halves — the top-up to
    /// the target (the wire carries no page word, so the joiner names the
    /// difference; past the manager's cap the pool it got is the target's
    /// ceiling); return the surplus above twice the target (a pool that
    /// outgrew its target after a storm of frees); the mirror's mark
    /// follows the target.
    pub fn poolCadence(self: *JoinedSet) void {
        self.pool_lock.lockUncancelable(self.be.io);
        defer self.pool_lock.unlock(self.be.io);
        const want_target = self.poolTarget();
        const target = grant.joinedCappedTarget(want_target, self.capped_pool);
        const unclaimed = self.unclaimedCount();
        self.be.alloc.setLowWater(grant.joinedRefillMark(target, self.be.cache.heapPromised()));
        if (grant.joinedRefillDue(unclaimed -| self.be.cache.heapPromised(), target)) {
            var runs: std.ArrayList(grant.GrantRun) = .empty;
            defer runs.deinit(self.gpa);
            // The law's want, not the ceiling: an ask that fits the cap again lifts it.
            var want: u32 = @intCast(@min(want_target -| unclaimed, grant.GRANT_EXTENTS_MAX));
            if (want == 0) return;
            self.wire.extentGrant(self.appenderId(), want, &runs) catch |e| switch (e) {
                error.Rejected => {
                    // Past the manager's heap-share cap: the cap itself (0).
                    stats.extent_grant_asks_capped.inc();
                    want = 0;
                    self.wire.extentGrant(self.appenderId(), 0, &runs) catch |e2| {
                        log.debug("meta volume {s}: ExtentGrant(cap) did not land this cadence ({s}); the pool holds {d} unclaimed of a target of {d}", .{ self.be.path, @errorName(e2), unclaimed, target });
                        return;
                    };
                },
                else => {
                    log.debug("meta volume {s}: ExtentGrant({d}) did not land this cadence ({s}); the pool holds {d} unclaimed of a target of {d}", .{ self.be.path, want, @errorName(e), unclaimed, target });
                    return;
                },
            };
            for (runs.items) |r| {
                {
                    self.bits_lock.lockUncancelable(self.be.io);
                    defer self.bits_lock.unlock(self.be.io);
                    var e = r.start;
                    while (e < r.end()) : (e += 1) {
                        if (self.granted_bits.isSet(@intCast(e))) continue;
                        self.granted_bits.set(@intCast(e));
                        self.granted += 1;
                    }
                }
                self.be.alloc.absorbGranted(.{ .start = r.start, .len = r.len });
            }
            self.capped_pool = if (want == 0) self.unclaimedCount() else null;
            stats.extent_grant_refills.inc();
            self.checkClosure();
            return;
        }
        if (unclaimed > 2 * target) {
            var surplus: std.ArrayList(u64) = .empty;
            defer surplus.deinit(self.gpa);
            while (self.unclaimedCount() > target) {
                const e = self.be.alloc.withdrawGranted() orelse break;
                surplus.append(self.gpa, e) catch break;
            }
            // Withdrawals are claims on the mirror, not images.
            self.rate_withdrawn += surplus.items.len;
            if (surplus.items.len == 0) return;
            var wire_runs: std.ArrayList(verbs.GrantRunWire) = .empty;
            defer wire_runs.deinit(self.gpa);
            std.mem.sort(u64, surplus.items, {}, std.sort.asc(u64));
            for (surplus.items) |e| {
                if (wire_runs.items.len > 0 and wire_runs.items[wire_runs.items.len - 1].start + wire_runs.items[wire_runs.items.len - 1].len == e) {
                    wire_runs.items[wire_runs.items.len - 1].len += 1;
                } else wire_runs.append(self.gpa, .{ .start = e, .len = 1, .reserved = 0 }) catch break;
            }
            _ = self.wire.returnExtents(self.appenderId(), wire_runs.items) catch {
                // The return did not land: the extents go back to the pool.
                for (surplus.items) |e| self.be.alloc.absorbGranted(.{ .start = e, .len = 1 });
                return;
            };
            {
                self.bits_lock.lockUncancelable(self.be.io);
                defer self.bits_lock.unlock(self.be.io);
                for (surplus.items) |e| self.granted_bits.unset(@intCast(e));
                self.returned += surplus.items.len;
            }
            stats.extent_grant_returns.inc();
            sym.stats.extent_grant_recycled.add(surplus.items.len);
            self.checkClosure();
        }
    }

    // ---- the page ----

    /// The page plan executed: the shadow registry carries the backend's
    /// seq offset, every slot the directory-first law names, one barrier
    /// unless the cycle's own covers it.
    pub fn writePages(self: *JoinedSet, from_cycle: bool) error{ Io, Corrupt, OutOfMemory }!void {
        const r = self.core();
        r.reg.recoverSeqOffset(self.be.reg.seqOffset());
        var plan = r.planPageWrite() catch return error.Corrupt;
        for (plan.slice()) |w| {
            self.be.pool.writeAt(self.be.path, w.offset, &w.image) catch return error.Io;
            sym.stats.appender_page_writes.inc();
        }
        if (!from_cycle) self.be.dev.barrier() catch return error.Io;
        r.pageWritten(&plan);
    }

    /// `RecordSource.appenderPages` on a joiner — the record's ONE durable
    /// home (spec §2.5): the ring's head and tail, the cycle's seq, the
    /// pool's largest runs, the slot plane's ranked entries; then the pool
    /// cadence and the slot cadence's flight.
    pub fn onCheckpoint(self: *JoinedSet, rec: *const ledger.LedgerRecord) checkpoint_cycle.IoError!void {
        const r = self.core();
        r.page.head_hint = self.be.core.headPos();
        r.page.ledger_tail_seq = rec.journal_tail_seq;
        r.page.ckpt_seq = rec.seq;
        var runs: std.ArrayList(grant.GrantRun) = .empty;
        defer runs.deinit(self.gpa);
        self.unclaimedRuns(&runs) catch return error.Io;
        r.page.grants = grant.pageRunsOf(runs.items);
        self.be.slots.planPage(self.appenderId(), r) catch return error.Io;
        self.writePages(true) catch return error.Io;
        self.tail_written = rec.journal_tail_seq;
        self.be.slots.pageLanded(r);
        // The rates fold on the checkpoint clock alone (a kicked cadence
        // would sample a fraction of a cycle).
        self.foldSmoRate();
        self.foldCommitRate();
        self.poolCadence();
        self.be.slots.kickCadence();
    }

    // ---- the ring's growth (spec §3.3, the joiner's side) ----

    /// The commit-rate fold per cycle (`ring_growth.foldCommitRate`): the
    /// ring derivation's measured input (charter §5.5 "`clamp(2 ×
    /// ewma_commit_bytes/s × CHECKPOINT_MAX_AGE, …)`").
    fn foldCommitRate(self: *JoinedSet) void {
        const now = clock.monotonicNs();
        const head = self.be.core.headPos();
        self.commit_ewma_bytes_per_s = ring_growth.foldCommitRate(self.commit_ewma_bytes_per_s, head, self.head_at_last_fold, now -| self.fold_mark_ns);
        self.head_at_last_fold = head;
        self.fold_mark_ns = now;
        stats.joined_commit_ewma_bytes_per_s.set(self.commit_ewma_bytes_per_s);
    }

    /// The conveyor's growth-gate seam.
    pub fn growthGateSeam(self: *JoinedSet) conveyor.GrowthGate {
        return .{ .ctx = &self.gate, .enter = &gateEnter, .leave = &gateLeave, .windowLeft = &gateWindowLeft };
    }

    fn gateEnter(ctx: *anyopaque) void {
        const g: *ring_growth.Gate = @ptrCast(@alignCast(ctx));
        g.enter();
    }

    fn gateLeave(ctx: *anyopaque) void {
        const g: *ring_growth.Gate = @ptrCast(@alignCast(ctx));
        g.leave();
    }

    fn gateWindowLeft(ctx: *anyopaque) void {
        const g: *ring_growth.Gate = @ptrCast(@alignCast(ctx));
        g.noteWindowLeft();
    }

    /// The checkpoint task's `after_tick` seam: the growth is judged once
    /// per cycle that landed (an idle tick judges nothing).
    pub fn afterTick(ctx: *anyopaque) void {
        const self: *JoinedSet = @ptrCast(@alignCast(ctx));
        const seq = self.be.cp.checkpointSeq();
        if (seq == self.grow_judged_at_seq) return;
        self.grow_judged_at_seq = seq;
        self.growRingIfDue();
    }

    /// The pass count FIRST, then the window count: a pass tracks its
    /// window before it leaves the gate (`park_core_model.RetireModel`).
    fn windowEmpty(self: *JoinedSet) bool {
        return self.gate.passesInside() == 0 and self.be.cv.windowsInflight() == 0;
    }

    // ---- the retirement (a live member evicted re-joins in place) ----

    /// What the retirement leaves for the eligibility judgement (decision
    /// 0003 §BS): the old ring's landed head and whether everything this
    /// incarnation admitted landed inside the bound.
    pub const Retirement = struct {
        /// Every pass left and every window landed within one landing
        /// ceiling.
        drained: bool,
        /// The ring's head after the drain: past it nothing is written.
        head: u64,
        /// Reservations still open (a range never landed).
        open: usize,
    };

    /// THE RETIREMENT of this incarnation (charter §5.5 "A live member
    /// evicted …"): the home shard evicted it and recovered (or is
    /// recovering) its region, so nothing more may be admitted or written
    /// through it. In order: the latch refuses every new mutation
    /// (`Retired`: the conveyor's `Deferred`, nothing written, re-issued on
    /// the successor), the park gate RETIRED (a parked pass wakes and
    /// refuses its batch), the growth gate closed and drained — a pass
    /// that read RUNNING is inside it until its window is tracked
    /// (`park_core_model.RetireModel`) — bounded by one landing ceiling;
    /// then, under the SMO lock (a cycle in flight finishes first), the
    /// latch FAILED: no cycle, page or landing writes again. The growth
    /// gate re-opens onto the latch (later passes refuse at once).
    pub fn retireIncarnation(self: *JoinedSet) Retirement {
        const be = self.be;
        be.lane.latch.retire();
        _ = self.park_gate.retire();
        _ = self.gate.close();
        const ceiling_ns = checkpoint.checkpointLandingCeilingMs(be.cp.cfg.max_age_ms, be.cp.cfg.flush_interval_ms) * std.time.ns_per_ms;
        const drained = self.gate.awaitDrained(clock.monotonicNs() + ceiling_ns, self, windowEmpty);
        be.smo_lock.lockUncancelable(be.io);
        _ = be.lane.latch.failStop();
        be.smo_lock.unlock(be.io);
        self.gate.open();
        return .{ .drained = drained, .head = be.core.headPos(), .open = be.reg.openCount() };
    }

    /// This appender's page as the device holds it NOW: the newest valid
    /// image over its directory pair and — once a directory image names a
    /// first segment — the two ring-side slots (what the successor's
    /// recovery writes through). Null when the directory does not reach
    /// the id.
    pub fn readOwnPage(self: *JoinedSet) OpenError!?page.Page {
        const be = self.be;
        var links: std.ArrayList(dir.Link) = .empty;
        defer links.deinit(be.gpa);
        var reader_ctx: DevicePages = .{ .be = be };
        const reader: dir.PageReader = .{ .ctx = &reader_ctx, .read = &DevicePages.read };
        dir.walkChain(be.gpa, reader, .{ .start = be.sb.appender_dir.start, .len = be.sb.appender_dir.len }, be.sb.node_size, .{ .start = be.sb.heap.start, .len = be.sb.heap.len }, be.sb.totalExtents(), &links) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
            error.Corrupt => error.Corrupt,
        };
        const loc = dir.locate(self.appenderId(), dir.pairCount(be.sb.node_size));
        if (loc.chain_index >= links.items.len) return null;
        const dir_slots = dir.pairOffsets(links.items[loc.chain_index].extent, loc.pair);
        var bufs: [page.PAGE_SLOTS][page.PAGE_LEN]u8 = undefined;
        var imgs: [page.PAGE_SLOTS]?[]const u8 = .{ null, null, null, null };
        for (dir_slots, 0..) |off, i| {
            DevicePages.read(&reader_ctx, off, &bufs[i]) catch return error.Io;
            imgs[i] = &bufs[i];
        }
        const dir_best = page.newestValid(.{ imgs[0], imgs[1], null, null }) catch return error.Corrupt;
        if (dir_best) |p| if (p.segments.len > 0) {
            for (page.ringSideOffsets(p.segments.items[0]), 2..) |off, i| {
                DevicePages.read(&reader_ctx, off, &bufs[i]) catch return error.Io;
                imgs[i] = &bufs[i];
            }
        };
        return page.newestValid(imgs) catch error.Corrupt;
    }

    /// The successor's recovery of this (retired) region as the device
    /// shows it: the page `Recovered` under this identity, and the head
    /// its ring scan reached.
    pub const RecoveredPage = union(enum) {
        /// `Recovered{ledger_tail_seq = head}` (recovery step 8).
        recovered: struct { head: u64, term: u64 },
        /// Not (yet) recovered: the state the page shows.
        pending: page.State,
        /// The page names another identity: the region is not ours.
        foreign,
        unreadable,
    };

    /// Poll the page until the successor's recovery wrote it `Recovered`
    /// or `deadline_ns` passed (every `ENDPOINT_POLL_STEP_MS`): the
    /// manager's death-ledger poll recovers the region within its cadence
    /// after the eviction, and nothing of this incarnation is written
    /// meanwhile (it is retired).
    pub fn awaitRecovered(self: *JoinedSet, deadline_ns: u64) RecoveredPage {
        while (true) {
            const verdict: RecoveredPage = if (self.readOwnPage()) |maybe| blk: {
                const p = maybe orelse break :blk .unreadable;
                if (!p.identity.sameMount(self.identity)) break :blk .foreign;
                if (p.state == .recovered) break :blk .{ .recovered = .{ .head = p.ledger_tail_seq, .term = p.recovered_by_term } };
                break :blk .{ .pending = p.state };
            } else |_| .unreadable;
            switch (verdict) {
                .pending, .unreadable => if (clock.monotonicNs() < deadline_ns) {
                    self.be.io.sleep(.fromMilliseconds(ENDPOINT_POLL_STEP_MS), .awake) catch {};
                    continue;
                },
                else => {},
            }
            return verdict;
        }
    }

    /// `joined_grow_ring_if_due` (spec §3.3), on the checkpoint task after
    /// a cadence tick, under the SMO lock: the decision (`ring_growth.
    /// judge`: derived above current, or a fresh ring-admission stall, and
    /// no standing decline at this size) → DRAIN-THEN-GROW: the gate closes
    /// (every pass parks before its first read of the ring), the window
    /// empties — cycles run meanwhile so a pass parked at ring admission
    /// can advance and leave — bounded by one landing ceiling; cover cycles
    /// to the FIXPOINT (`head == reusable_upto`, nothing open), bounded
    /// `FIXPOINT_COVER_CYCLES_MAX`; `GrowRing(step)` over the wire; the page
    /// names the grown table in BOTH directory slots + barrier BEFORE any
    /// position is written under the new map (the directory-first law);
    /// the swap (`KvBackend.swapRing`). A page write that fails leaves the
    /// carve under the manager's pending witness (re-answered verbatim at
    /// the next ask, returned at the leave); a refused swap re-writes the
    /// page WITHOUT the segment (never in place). Nothing here is the
    /// cycle's error: the tick landed before growth was decided.
    pub fn growRingIfDue(self: *JoinedSet) void {
        const be = self.be;
        if (self.left or be.cv.isClosing() or be.lane.latch.isFailed() or self.park_gate.word() != .running) return;
        const r = self.core();
        const current = r.ringBytes();
        const stalls = be.cv.stats.full_stalls.load();
        const derived = region.ringBytesDerived(self.commit_ewma_bytes_per_s, be.cp.cfg.max_age_ms, be.sb.volume_len);
        const step = switch (ring_growth.judge(.{
            .current = current,
            .derived = derived,
            .ceiling = region.ringCeiling(be.sb.volume_len),
            .stalls = stalls,
            .stalls_at_last_grow = self.stalls_at_last_grow,
            .declined_at = self.grow_declined_at,
            .segments = r.table.len,
            .node_size = be.sb.node_size,
        })) {
            .idle => return,
            .at_ceiling => {
                self.stalls_at_last_grow = stalls;
                return;
            },
            .table_full => {
                self.stalls_at_last_grow = stalls;
                self.grow_declined_at = current;
                stats.joined_ring_grow_declined.inc();
                log.debug("meta volume {s}: joined appender {d}'s ring wants more than {d} B (derived {d}) but its table is at {d} segments — declined (joined_ring_grow_declined)", .{ be.path, self.appenderId(), current, derived, r.table.len });
                return;
            },
            .ask => |s| s,
        };
        // The gate: passes park from here; the window empties.
        _ = self.gate.close();
        var opened = false;
        defer if (!opened) self.gate.open();
        const ceiling_ns = checkpoint.checkpointLandingCeilingMs(be.cp.cfg.max_age_ms, be.cp.cfg.flush_interval_ms) * std.time.ns_per_ms;
        const round_ns = checkpoint.checkpointTickPeriodMs(be.cp.cfg.flush_interval_ms) * std.time.ns_per_ms;
        const deadline = clock.monotonicNs() + ceiling_ns;
        while (!self.windowEmpty()) {
            // A pass inside may be parked at ring admission (the storm filled
            // the ring behind the tick that just ran): a cycle advances the
            // tail it waits for, one per round.
            be.cp.checkpointCycle(true) catch |e| {
                stats.joined_ring_grow_deferrals.inc();
                log.warn("meta volume {s}: joined appender {d}'s growth drain cycle failed ({s}) — growth retried next cycle", .{ be.path, self.appenderId(), @errorName(e) });
                return;
            };
            const now = clock.monotonicNs();
            if (self.gate.awaitDrained(@min(deadline, now + round_ns), self, windowEmpty)) break;
            if (clock.monotonicNs() >= deadline or be.cv.isClosing()) {
                stats.joined_ring_grow_deferrals.inc();
                log.debug("meta volume {s}: joined appender {d}'s window did not empty inside the {d} ms landing ceiling ({d} pass(es) inside, {d} window(s) in flight) — growth retried next cycle", .{ be.path, self.appenderId(), ceiling_ns / std.time.ns_per_ms, self.gate.passesInside(), be.cv.windowsInflight() });
                return;
            }
        }
        // The cover to the fixpoint: a cycle's own flush journals its SMOs
        // past the head its tail was computed from, so the cycle after it
        // covers them; with the gate closed the term converges.
        var covers: u64 = 0;
        while (!be.ringDrained() and covers < checkpoint.FIXPOINT_COVER_CYCLES_MAX) : (covers += 1) {
            be.cp.checkpointCycle(true) catch |e| {
                stats.joined_ring_grow_deferrals.inc();
                log.warn("meta volume {s}: joined appender {d}'s growth cover cycle failed ({s}) — growth retried next cycle", .{ be.path, self.appenderId(), @errorName(e) });
                return;
            };
        }
        if (!be.ringDrained()) {
            stats.joined_ring_grow_deferrals.inc();
            log.debug("meta volume {s}: joined appender {d}'s ring is not drained after {d} cover cycles (head {d}, reusable_upto {d}, {d} reservation(s) open) — growth retried next cycle", .{ be.path, self.appenderId(), covers, be.core.headPos(), be.core.reusableUpto(), be.reg.openCount() });
            return;
        }
        const head = be.core.headPos();
        const own = self.appenderId();
        const answer = self.wire.growRing(own, step) catch |e| {
            stats.joined_ring_grow_deferrals.inc();
            switch (e) {
                // The manager's `Deferred` (its ring 0 full for a beat) and
                // a manager out of reach (its death is the membership's
                // business: `T_self` parks this appender) are the cadence's;
                // anything else is the wire's failure, loud.
                error.Deferred, error.MembershipPending, error.HolderUnreachable, error.ConnectionLost, error.ListenerRefused => log.debug("meta volume {s}: joined appender {d}'s GrowRing did not land this cycle ({s}) — retried next cycle", .{ be.path, own, @errorName(e) }),
                else => log.warn("meta volume {s}: joined appender {d}'s GrowRing failed ({s}) — retried next cycle", .{ be.path, own, @errorName(e) }),
            }
            return;
        };
        const seg = answer orelse {
            self.stalls_at_last_grow = stalls;
            self.grow_declined_at = current;
            stats.joined_ring_grow_declined.inc();
            log.info("meta volume {s}: the manager declined joined appender {d}'s GrowRing ({d} B wanted at {d} B) — the ring stays until a fresh stall or a changed ring (joined_ring_grow_declined)", .{ be.path, own, step, current });
            return;
        };
        // The page names the grown table: a TABLE CHANGE, both directory
        // slots + barrier before any position is written under the new map.
        const old_table = r.table;
        var grown = r.table;
        grown.push(seg) catch {
            // Screened by `judge` (the table had room); the carve stays
            // under the manager's pending witness.
            stats.joined_ring_grow_deferrals.inc();
            return;
        };
        r.setTable(grown);
        r.page.head_hint = head;
        r.page.ledger_tail_seq = head;
        self.writePages(false) catch |e| {
            r.setTable(old_table);
            log.err("meta volume {s}: joined appender {d}'s page could not name its grown ring ({s}) — the segment at 0x{x} ({d} B) the manager carved is named by no page; the manager answers it again at the next GrowRing and returns it at this identity's leave or death", .{ be.path, own, @errorName(e), seg.start, seg.len });
            return;
        };
        be.swapRing(&grown, own) catch |e| {
            // The never-in-place law: the page must not name a table the
            // ring does not write under — the segment comes off again.
            r.setTable(old_table);
            const undone: []const u8 = if (self.writePages(false)) |_| "ok" else |_| "failed";
            log.err("meta volume {s}: joined appender {d}'s ring swap refused ({s}) after its page named the grown table — the page rewritten without it ({s}); the segment at 0x{x} ({d} B) stays under the identity's pending witness for the manager's next verb to answer or return", .{ be.path, own, @errorName(e), undone, seg.start, seg.len });
            return;
        };
        r.ring_layout = be.ring_layout;
        r.geo = be.geo;
        self.tail_written = head;
        self.stalls_at_last_grow = stalls;
        self.grow_declined_at = ring_growth.NEVER_DECLINED;
        stats.joined_ring_grows.inc();
        self.gate.open();
        opened = true;
        log.info("meta volume {s}: joined appender {d}'s ring grew by one segment of {d} B at 0x{x} ({d} → {d} B, {d} segment(s); derived {d} B/s × 2 × {d} ms, stalls {d}; joined_ring_grows, joined_wire_ring_grows)", .{ be.path, own, seg.len, seg.start, current, r.ringBytes(), r.table.len, self.commit_ewma_bytes_per_s, be.cp.cfg.max_age_ms, stalls });
    }

    // ---- the slot verbs over the wire ----

    pub const Touch = join_ladder.Joiner.Touch;

    /// A first touch of `slot` (the door's seam): granted ⇒ the plane
    /// installs the words; busy ⇒ the holder's word for the resolver.
    pub fn acquireSlot(self: *JoinedSet, slot: u16) errors.Error!join_ladder.ManagerWire.AcquireReply {
        return self.wire.acquireSlot(self.appenderId(), slot);
    }

    pub fn acquireSlots(self: *JoinedSet, want: u16) errors.Error!join_ladder.ManagerWire.AcquireReply {
        return self.wire.acquireSlots(self.appenderId(), want);
    }

    pub fn releaseSlot(self: *JoinedSet, slot: u16, g: u32, words: slots.lease_table.SlotWords, tails: []const cr.TailEntry) errors.Error!bool {
        var wire_tails: std.ArrayList(verbs.LeafTailWire) = .empty;
        defer wire_tails.deinit(self.gpa);
        for (tails) |t| try wire_tails.append(self.gpa, .{ .leaf_addr = t.leaf_addr, .tail = t.tail, .reserved = 0 });
        return self.wire.releaseSlot(self.appenderId(), slot, g, words, wire_tails.items);
    }

    /// The page-budget overflow's publication (charter §5.5): the live
    /// words into tree 0 through the manager.
    pub fn publishRoots(self: *JoinedSet, roots: []const verbs.SlotRootWire) errors.Error!manager.RootsPublished {
        return self.wire.publishRoots(self.appenderId(), roots);
    }

    pub fn resolveEndpoint(self: *JoinedSet, appender_id: u32) errors.Error!?endpoint.EndpointWire {
        return self.wire.resolveEndpoint(appender_id);
    }

    pub fn publishEndpoint(self: *JoinedSet, ep: endpoint.EndpointWire) errors.Error!bool {
        return self.wire.publishEndpoint(self.appenderId(), ep, 0);
    }

    // ---- the leave ----

    /// The clean leave (the manager's screen: no slot named, nothing
    /// leased, the ring drained): every lease released by the plane first,
    /// the page naming no slot + barrier, the fixpoint cycles, then
    /// `LeaveAppender` with the pool's unclaimed runs — the page goes Free
    /// under the manager's pen.
    pub fn leave(self: *JoinedSet) void {
        if (self.left) return;
        self.left = true;
        const be = self.be;
        // A FENCED member's dismount is the CRASH shape (charter §5.5: a
        // park's expiry is the terminal poison; `leaveAllowed`'s law): its
        // home shard evicted it, or will, and the manager recovers its
        // region from the ring and the page as they stand — a release or a
        // page write from here would land ON a recovered region (a
        // poisoned member's stop shipped 61 `ReleaseSlot`s for slots the
        // manager had recovered, and re-wrote the page the manager had set
        // Recovered). The same for a park that stands: nothing durable.
        if (!self.park_gate.leaveAllowed()) {
            log.warn("meta volume {s}: appender {d} stops {s} — the leave writes nothing (the crash shape: its region is the manager's to recover; the page stays as the manager left it)", .{ be.path, self.appenderId(), switch (self.park_gate.word()) {
                .expired => "FENCED (poisoned)",
                .retired => "RETIRED (re-joined in place)",
                else => "PARKED",
            } });
            return;
        }
        // No refill lands under the leave (the pool's word is read below).
        self.refill.cancel(be.io);
        be.slots.releaseAllForLeave(self.appenderId());
        // Every release's cycles landed the roots; the page without the
        // slots, barriered, before the manager screens it.
        self.core().page.setSlots(&.{}) catch {};
        self.writePages(false) catch |e| {
            log.err("meta volume {s}: the leave's page write failed ({s}) — the page stays Live for the next open's recovery", .{ be.path, @errorName(e) });
            return;
        };
        be.checkpointNow() catch {};
        if (self.core().page.ledger_tail_seq != be.core.headPos()) be.checkpointNow() catch {};
        // The pool's word for the leave: the mirror's unclaimed runs. The
        // leave frame carries `GRANT_WIRE_MAX` runs; a pool a storm
        // fragmented wider (found by the ring-growth row: 20 runs after
        // 572 commits) returns them through `ReturnExtents` first — the
        // same screened return law, no run cap — and leaves with none.
        var runs: std.ArrayList(grant.GrantRun) = .empty;
        defer runs.deinit(self.gpa);
        self.unclaimedRuns(&runs) catch {};
        var returned_ahead: usize = 0;
        if (runs.items.len > join_ladder.GRANT_WIRE_MAX) {
            var wire_runs: std.ArrayList(verbs.GrantRunWire) = .empty;
            defer wire_runs.deinit(self.gpa);
            for (runs.items) |r| wire_runs.append(self.gpa, .{ .start = r.start, .len = r.len, .reserved = 0 }) catch break;
            if (wire_runs.items.len == runs.items.len) {
                if (self.wire.returnExtents(self.appenderId(), wire_runs.items)) |_| {
                    returned_ahead = runs.items.len;
                    {
                        self.bits_lock.lockUncancelable(be.io);
                        defer self.bits_lock.unlock(be.io);
                        for (runs.items) |r| {
                            var e = r.start;
                            while (e < r.end()) : (e += 1) self.granted_bits.unset(@intCast(e));
                            self.returned += r.len;
                        }
                    }
                    stats.extent_grant_returns.inc();
                    runs.clearRetainingCapacity();
                } else |e| {
                    log.warn("meta volume {s}: appender {d}'s {d} unclaimed run(s) did not return ahead of its leave ({s}) — the leave carries them", .{ be.path, self.appenderId(), runs.items.len, @errorName(e) });
                }
            }
        }
        var jr = &self.joiner.region.?;
        jr.grant.deinit();
        jr.grant = grant.RegionGrant.init(self.gpa);
        _ = jr.grant.absorb(runs.items) catch {};
        const left = self.joiner.leave() catch |e| {
            log.err("meta volume {s}: appender {d} could not leave cleanly ({s}) — its page stays Live for the next open's recovery", .{ be.path, self.appenderId(), @errorName(e) });
            return;
        };
        log.info("meta volume {s}: appender {d} left ({s}); {d} extent run(s) returned with the page, {d} ahead of it", .{ be.path, self.appenderId(), if (left.already) "already" else "page Free", runs.items.len, returned_ahead });
    }

    /// The per-volume words of the `.stats` family on a joiner.
    pub fn writeStatsJson(self: *JoinedSet, w: *Io.Writer) Io.Writer.Error!void {
        const r = self.core();
        const unclaimed = self.unclaimedCount();
        try w.print("{{\"manager_lease\":\"joined\",\"appenders_live\":1,\"appenders_known\":{d},\"appenders_capacity\":0,\"appender_ring_budget_remaining_bytes\":0,\"appender_live_pages_at_mount\":0,\"appender_unrecovered\":0,\"manager_endpoint\":\"{f}\",\"manager_repoints\":{d},\"regions\":[{{\"appender_id\":{d},\"term\":{d},\"ring_bytes\":{d},\"ring_segments\":{d},\"seq_offset\":{d},\"grant_unclaimed\":{d},\"grant_held\":{d},\"grant_granted\":{d},\"grant_returned\":{d},\"entries\":{d}}}],\"slots\":", .{ self.writers_known, self.managerEndpoint(), self.manager_resolver.repoints.load(.monotonic), self.appenderId(), r.page.term, r.ringBytes(), r.table.len, self.be.reg.seqOffset(), unclaimed, self.granted_bits.count() - unclaimed, self.granted, self.returned, self.entries });
        try self.be.slots.writeStatsJson(w);
        try w.writeByte('}');
    }
};

// ---------------------------------------------------------------------------
// The manager link's resolver: the S5 epoch poll as the control plane.
// ---------------------------------------------------------------------------

/// Before every (re)dial of the manager link (charter §5.5 "Readers'
/// planes FOLLOW a holder that moved its listener (re-resolve off durable
/// state, re-point in place)"; "The S5 epoch-poll of the fixed ledger
/// survives as the CONTROL plane only"): ONE predicted-slot read of the
/// fixed ledger; a record newer than the last adopted names a checkpoint
/// this mount has not seen — a successor manager's first (its
/// `endpoint:{0}` lands in it) — so `endpoint:{0}` is re-read at that
/// root through a transient cache. A dead manager's port stays the answer
/// until a successor checkpoints: the dial refuses at once and the
/// member's beat decides (`T_self` → PARK, the reclaim beat → the
/// successor). The poll starts at the mounted checkpoint, so a `--join`
/// listener is honoured until the record first moves past it.
pub const ManagerResolver = struct {
    be: *KvBackend,
    link: *SharedLink,
    poll: ro_mount.EpochPoll,
    repoints: std.atomic.Value(u64) = .init(0),

    /// Over `be`'s device, polling from its mounted checkpoint; installed
    /// on `link` by the caller (`link.resolver = resolver.face()`).
    pub fn init(be: *KvBackend, link: *SharedLink) ManagerResolver {
        return .{ .be = be, .link = link, .poll = ro_mount.EpochPoll.init(be.gpa, .{ .ctx = be, .readSlot = &readSlot, .readAll = &readAll }, be.mounted.seq) };
    }

    pub fn face(self: *ManagerResolver) EndpointResolver {
        return .{ .ctx = self, .resolve = &resolve };
    }

    fn resolve(ctx: *anyopaque) ?endpoint.EndpointWire {
        const self: *ManagerResolver = @ptrCast(@alignCast(ctx));
        const be = self.be;
        const current = self.link.ep;
        const out = self.poll.poll() catch return current;
        var rec = out.newest orelse return current;
        defer rec.deinit(be.gpa);
        const bytes = (be.lookupControlAtRecord(be.gpa, &rec, cr.endpointKey(0).slice()) catch return current) orelse return current;
        defer be.gpa.free(bytes);
        const ep = decodeEndpoint(bytes) catch return current;
        if (!ep.eql(current)) {
            _ = self.repoints.fetchAdd(1, .monotonic);
            log.info("meta volume {s}: the manager's listener moved to {f} (checkpoint seq {d}) — the manager link re-points", .{ be.path, ep, rec.seq });
        }
        return ep;
    }

    pub fn readSlot(ctx: *anyopaque, slot: u64, out: *[ro_mount.SLOT_LEN]u8) ro_mount.IoError!void {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        const img = be.dev.readRange(be.gpa, be.sb.root_ledger.start + slot * ro_mount.SLOT_LEN, ro_mount.SLOT_LEN) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
        };
        defer be.gpa.free(img);
        @memcpy(out, img[0..ro_mount.SLOT_LEN]);
    }

    pub fn readAll(ctx: *anyopaque, out: []u8) ro_mount.IoError!void {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        const img = be.dev.readRange(be.gpa, be.sb.root_ledger.start, out.len) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
        };
        defer be.gpa.free(img);
        @memcpy(out, img[0..out.len]);
    }
};

// ---------------------------------------------------------------------------
// Tree 0's projection: the secret and the endpoint before any wire.
// ---------------------------------------------------------------------------

pub fn readControl(gpa: Allocator, be: *KvBackend, k: []const u8, comptime T: type, comptime decode: anytype) OpenError!?T {
    const bytes = (be.lookup(gpa, key.TREE_CONTROL, k) catch |e| return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        else => error.Io,
    }) orelse return null;
    defer gpa.free(bytes);
    return decode(bytes) catch return error.Corrupt;
}

/// Appender 0's newest valid page (the manager's identity, its state).
fn readAppender0(gpa: Allocator, be: *KvBackend) OpenError!?page.Page {
    const offs = page.appender0PageOffsets(be.sb.journal.start);
    var bufs: [4][page.PAGE_LEN]u8 = undefined;
    var imgs: [4]?[]const u8 = .{ null, null, null, null };
    for (offs, 0..) |off, i| {
        const img = be.dev.readRange(gpa, off, page.PAGE_LEN) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
        };
        defer gpa.free(img);
        @memcpy(&bufs[i], img[0..page.PAGE_LEN]);
        imgs[i] = &bufs[i];
    }
    return page.newestValid(imgs) catch return error.Corrupt;
}

pub fn decodeEndpoint(bytes: []const u8) !endpoint.EndpointWire {
    const e = try cr.EndpointRecordWire.decode(bytes);
    return .{ .family = e.family, .reserved = 0, .port = e.port, .addr = e.addr };
}

/// The manager's `endpoint:{0}` record off tree 0; absent ⇒ the manager
/// published it in its ring-0 window and the next cycle lands it: the
/// joiner re-reads the newest ledger record and re-opens the control tree
/// at its root, bounded (the joiner never replays ring 0).
pub fn awaitManagerEndpoint(gpa: Allocator, be: *KvBackend) OpenError!endpoint.EndpointWire {
    const deadline = clock.monotonicNs() + ENDPOINT_POLL_MAX_MS * std.time.ns_per_ms;
    while (true) {
        if (try readControl(gpa, be, cr.endpointKey(0).slice(), endpoint.EndpointWire, decodeEndpoint)) |ep| return ep;
        if (clock.monotonicNs() >= deadline) {
            log.err("meta volume {s}: no manager endpoint published on tree 0 within {d} ms — the D0 winner has not armed its wire (or `--join <endpoint>` names it)", .{ be.path, ENDPOINT_POLL_MAX_MS });
            return error.ManagerUnreachable;
        }
        stats.joined_endpoint_polls.inc();
        log.debug("meta volume {s}: no endpoint:0 record at checkpoint seq {d} (tail {d}) — polling the projection", .{ be.path, be.mounted.seq, be.mounted.journal_tail_seq });
        be.io.sleep(.fromMilliseconds(ENDPOINT_POLL_STEP_MS), .awake) catch {};
        // A joiner re-opens the control projection (nothing of its own
        // exists yet); a reader adopts the newest checkpoint whole.
        if (be.posture == .reader) {
            _ = be.adoptNewestCheckpoint() catch |e| return mapReopen(e);
        } else {
            be.reopenControlProjection() catch |e| return mapReopen(e);
        }
    }
}

fn mapReopen(e: backend.OpenError) OpenError {
    return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        error.Io => error.Io,
        else => |other| errors.narrow(OpenError, other, "joined.mapReopen"),
    };
}

// ---------------------------------------------------------------------------
// Tests: the pure parts.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "a joiner's mount slot derives from the mount point with bit 31 set: never a manager's lane index, distinct across mount points, stable" {
    const a = joinerMountSlot("/mnt-b");
    const b = joinerMountSlot("/mnt-c");
    try testing.expect(a & JOINER_MOUNT_SLOT_BIT != 0 and b & JOINER_MOUNT_SLOT_BIT != 0);
    try testing.expect(a != b);
    try testing.expectEqual(a, joinerMountSlot("/mnt-b"));
    try testing.expect(a >= 65_536 and b >= 65_536);
}

test "the endpoint poll bound is three checkpoint cadences" {
    try testing.expectEqual(@as(u64, 3_000), ENDPOINT_POLL_MAX_MS);
    try testing.expect(ENDPOINT_POLL_STEP_MS < @import("../checkpoint.zig").CHECKPOINT_MAX_AGE_MS);
}

fn endpointRecordFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [128]u8 = undefined;
    const len = smith.slice(&buf);
    if (decodeEndpoint(buf[0..len])) |e| try std.testing.expectEqual(@as(u8, 0), e.reserved) else |_| {}
}

test "fuzz: the manager endpoint record decoder is total on arbitrary bytes" {
    try fuzzkit.fuzz({}, endpointRecordFuzz, .{});
}
