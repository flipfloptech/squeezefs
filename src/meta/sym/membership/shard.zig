//! Membership sharded by home volume (charter §5.5 "Membership is sharded
//! by home volume"; `docs/porting/sym_wire_membership_recovery.md` §2.1–
//! §2.4; KD-SYM-15): volume `v`'s manager is the S6 owner for the mounts
//! HOMED on `v`; the renewal venue is the home volume manager's listener;
//! a member learns its successor through the home volume's manager claim
//! (the rendezvous rides `writer_claim`). Membership is a RAM lease table
//! on the owner — per-beat durable commits serialized at 455/s against the
//! 1,500/s 15 k members need — with ONE durable row per member written on
//! CHANGE only: a `WriterClaimV1`-shaped record (`kind = 1`, decision 0003
//! §E) under `member:{node_token ‖ mount_slot}` on the home volume's tree
//! 0, so the dead-pid proof and `clients` apply to members.
//!
//! The owner evicts at `T_owner` (45 s, the ONE staleness law; `>=`: the
//! lease expires AT the deadline). A successor arms only at a term above
//! the recorded one and opens a GRACE window of two halves on one
//! deadline: the FRESH-REFUSAL half closes early once every EXPECTED
//! member re-asserted, the RE-ASSERTION half admits a prior epoch until
//! the deadline whatever the first did (a `-o ro` reader is in no claim
//! set, so never in `expected`). At the deadline the non-reclaimers are
//! DEAD and shipped to the death sink — after grace, never at `T_owner`.
//! Liveness is THREE-valued: `live` inside the deadline, `dead` past it /
//! departed / absent with the window closed, `unknown` absent while the
//! window is open (two-valued liveness read 1,318 of 1,318 acked files as
//! lost after every failover).
//!
//! The member anchors every deadline on its own SEND instant; its `T_self`
//! action is the PARK for a symmetric appender (`park_reclaim`), the poison
//! for remote custody. Every registry is a field of the owner / member
//! (0002 row 259); the instruments alone are module globals.

const std = @import("std");
const fuzzkit = @import("../../../core/fuzzkit.zig");
const Allocator = std.mem.Allocator;
const Io = std.Io;
const counters = @import("../../../core/counters.zig");
const clock = @import("../../../core/clock.zig");
const log = @import("../../../core/log.zig");
const errors = @import("../../../core/errors.zig");
const checksum = @import("../../../core/checksum.zig");
const knobs = @import("../../../core/knobs.zig");
const claim = @import("../../guard/claim.zig");
const lease_clock = @import("../../guard/lease_clock.zig");
const cr = @import("../control_records.zig");
const verbs = @import("../../../wire/verbs.zig");
const service = @import("../../../wire/service.zig");
const screen = @import("../../../wire/screen.zig");
const codec = @import("../../../wire/codec.zig");
const endpoint = @import("../../../wire/endpoint.zig");
const park = @import("park_reclaim.zig");

pub const WireIdentity = verbs.WireIdentity;
pub const EndpointWire = endpoint.EndpointWire;
pub const Role = verbs.Role;

// ---------------------------------------------------------------------------
// Instruments (spec §7.2 "Membership").
// ---------------------------------------------------------------------------

pub const stats = struct {
    /// `off | owner | member` as `Mode`'s backing int.
    pub var membership_mode: counters.Gauge = .{};
    pub var membership_term: counters.Gauge = .{};
    pub var membership_members: counters.Gauge = .{};
    pub var membership_readers: counters.Gauge = .{};
    pub var membership_writers: counters.Gauge = .{};
    pub var membership_lease_ttl_ms: counters.Gauge = .{};
    pub var membership_self_deadline_ms: counters.Gauge = .{};
    pub var membership_grace_remaining_ms: counters.Gauge = .{};
    /// The shards THIS owner serves (one per manager lease held).
    pub var membership_shards: counters.Gauge = .{};
    pub var membership_epoch: counters.Gauge = .{};
    pub var membership_self_fenced: counters.Gauge = .{};
    pub var membership_renew_cadence_ms: counters.Gauge = .{};
    pub var membership_joins: counters.ShardedCounter = .{};
    pub var membership_renewals: counters.ShardedCounter = .{};
    pub var membership_renew_refusals: counters.ShardedCounter = .{};
    pub var membership_evictions: counters.ShardedCounter = .{};
    pub var membership_self_fences: counters.ShardedCounter = .{};
    pub var membership_census_serves: counters.ShardedCounter = .{};
    pub var membership_grace_refusals: counters.ShardedCounter = .{};
    /// Predecessor rows a successor's grace does not expect: a same-host
    /// member whose pid is proven dead can never re-assert (its next mount
    /// is a new incarnation, recovered through the death ledger).
    pub var membership_grace_dead_rows: counters.ShardedCounter = .{};
    pub var membership_grace_reclaims: counters.ShardedCounter = .{};
    pub var membership_reclaim_refusals: counters.ShardedCounter = .{};
    /// Grows per CHANGE only — growth per beat is the regression.
    pub var membership_registration_commits: counters.ShardedCounter = .{};
    pub var membership_mount_slot_collisions: counters.ShardedCounter = .{};
    /// Exact-sum `carry_wait / rtt / total` per renewal.
    pub var membership_renew_phase_ns: [RenewPhase.count]counters.ShardedLatencyHistogram = @splat(.{});
    /// The owner's serve wall per membership verb.
    pub var membership_renew_serve_ns: counters.ShardedLatencyHistogram = .{};
};

pub const RenewPhase = enum(u8) {
    carry_wait,
    rtt,
    total,

    pub const count = @typeInfo(RenewPhase).@"enum".field_names.len;
};

/// The posture word (quirk Q-J1: a typed enum, never a string compare).
pub const Mode = enum(u8) { off = 0, owner = 1, member = 2 };

// ---------------------------------------------------------------------------
// Identity: the 16 B member id.
// ---------------------------------------------------------------------------

pub const MemberId = [16]u8;

/// A WRITER's id: the 12 B `(node_token BE ‖ mount_slot BE)` peer id with
/// a zero tail. A reader's is 16 random bytes (readers hold no region).
pub fn writerMemberId(identity: WireIdentity) MemberId {
    var id: MemberId = @splat(0);
    @memcpy(id[0..12], &identity.peerId());
    return id;
}

/// The writer identity a member id names (null for a reader id).
pub fn memberIdentity(id: MemberId) ?WireIdentity {
    if (!std.mem.allEqual(u8, id[12..], 0)) return null;
    return WireIdentity.fromPeerId(id[0..12]);
}

pub fn stableOf(identity: WireIdentity) cr.StableIdentity {
    return .{ .node_token = identity.node_token, .mount_slot = identity.mount_slot };
}

// ---------------------------------------------------------------------------
// The member row: `WriterClaimV1.kind = 1` on the HOME volume's tree 0.
// ---------------------------------------------------------------------------

pub const MemberRow = struct {
    writer_id: u64,
    ts_ns: i64,
    pid: u32,
    start_ticks: u64,
    boot_id: [16]u8,
    hostname_hash: u64,
    /// The membership epoch (the row's `heartbeat_seq` word).
    epoch: u64,
    /// The owner's term the row was written under.
    term: u64,

    pub fn encode(self: MemberRow) [claim.CLAIM_LEN]u8 {
        var w: claim.WriterClaimV1 = undefined;
        w.magic = claim.MAGIC;
        w.version = claim.VERSION;
        w.kind = claim.KIND_MEMBER;
        w.reserved0 = @splat(0);
        std.mem.writeInt(u64, &w.writer_id_le, self.writer_id, .little);
        std.mem.writeInt(i64, &w.ts_ns_le, self.ts_ns, .little);
        std.mem.writeInt(u32, &w.pid_le, self.pid, .little);
        w.reserved1 = @splat(0);
        std.mem.writeInt(u64, &w.start_ticks_le, self.start_ticks, .little);
        w.boot_id = self.boot_id;
        std.mem.writeInt(u64, &w.hostname_hash_le, self.hostname_hash, .little);
        std.mem.writeInt(u64, &w.heartbeat_seq_le, self.epoch, .little);
        std.mem.writeInt(u64, &w.term_le, self.term, .little);
        w.csum_le = @splat(0);
        var bytes = std.mem.toBytes(w);
        checksum.stamp(&bytes, @offsetOf(claim.WriterClaimV1, "csum_le"));
        return bytes;
    }

    /// Total: the writer-claim shape at `kind = 1`; a `kind = 0` record
    /// under a member key is corruption, never a member.
    pub fn decode(bytes: []const u8) error{Corrupt}!MemberRow {
        if (bytes.len != claim.CLAIM_LEN) return error.Corrupt;
        if (!checksum.verify(bytes, @offsetOf(claim.WriterClaimV1, "csum_le"))) return error.Corrupt;
        const w = std.mem.bytesToValue(claim.WriterClaimV1, bytes[0..claim.CLAIM_LEN]);
        if (!std.mem.eql(u8, &w.magic, &claim.MAGIC) or w.version != claim.VERSION or w.kind != claim.KIND_MEMBER) return error.Corrupt;
        if (!std.mem.allEqual(u8, &w.reserved0, 0) or !std.mem.allEqual(u8, &w.reserved1, 0)) return error.Corrupt;
        return .{
            .writer_id = std.mem.readInt(u64, &w.writer_id_le, .little),
            .ts_ns = std.mem.readInt(i64, &w.ts_ns_le, .little),
            .pid = std.mem.readInt(u32, &w.pid_le, .little),
            .start_ticks = std.mem.readInt(u64, &w.start_ticks_le, .little),
            .boot_id = w.boot_id,
            .hostname_hash = std.mem.readInt(u64, &w.hostname_hash_le, .little),
            .epoch = std.mem.readInt(u64, &w.heartbeat_seq_le, .little),
            .term = std.mem.readInt(u64, &w.term_le, .little),
        };
    }

    /// The same-host pid proof of a member: null for another host's row
    /// (unknowable here), else whether the pid is alive. A row carries no
    /// incarnation word when the join head had none (`start_ticks` 0):
    /// the proof is then the pid's existence, never a ticks mismatch.
    pub fn pidAlive(self: MemberRow, local: claim.Identity, probe: claim.PidProbe) ?bool {
        const same_host = local.bootIdReadable() and std.mem.eql(u8, &self.boot_id, &local.boot_id);
        if (!same_host) return null;
        if (self.pid == 0) return false;
        if (self.start_ticks == 0) return probe.startTicksOf(probe.ctx, self.pid) != null;
        return !probe.provablyDead(self.pid, self.start_ticks);
    }

    /// Does a successor's grace wait for this predecessor row to re-assert?
    /// Not when the row is a same-host member proven dead by its pid (the
    /// clients classifier's `dead`): nothing can re-assert its lease, and
    /// an expectation nobody meets holds the window's fresh half shut for
    /// every stranger to its deadline. A row that does not decode is
    /// expected (the conservative answer).
    pub fn expectsReassertion(row_bytes: []const u8, local: claim.Identity, probe: claim.PidProbe) bool {
        const row = decode(row_bytes) catch return true;
        const alive = row.pidAlive(local, probe) orelse return true;
        return alive or row.pid == local.pid;
    }

    /// The `clients` row of a member (the writer-claim classifier's law,
    /// quirk Q-R3: one classifier): a same-host member proven dead by its
    /// pid is `dead`; else `live` iff the owner lists it live, `stale`
    /// otherwise (offline, another host's member reads `stale`).
    pub fn registration(self: MemberRow, local: claim.Identity, now_ns: i64, probe: claim.PidProbe, live_in_ram: bool) claim.Registration {
        const c: claim.Claim = .{ .writer_id = self.writer_id, .ts_ns = self.ts_ns, .pid = self.pid, .start_ticks = self.start_ticks, .boot_id = self.boot_id, .hostname_hash = self.hostname_hash, .heartbeat_seq = self.epoch, .term = self.term };
        var r = claim.Registration.fromClaim(c, local, now_ns, probe);
        r.kind = .member;
        const dead = if (self.pidAlive(local, probe)) |alive| !alive and self.pid != local.pid else false;
        r.state = if (dead) .dead else if (live_in_ram) .live else .stale;
        return r;
    }
};

/// The durable home of the rows: tree 0 of the HOME volume, ONE put per
/// change (a join's new epoch), ONE delete per departure.
pub const RowStore = struct {
    ctx: ?*anyopaque,
    put: *const fn (ctx: ?*anyopaque, k: []const u8, v: []const u8) error{Io}!void,
    delete: *const fn (ctx: ?*anyopaque, k: []const u8) error{Io}!void,
};

// ---------------------------------------------------------------------------
// The lease clocks (every number derived; `T_owner` the one constant).
// ---------------------------------------------------------------------------

pub const LeaseClocks = struct {
    t_owner_ms: u64,
    skew_max_ms: u64,
    d_purge_ms: u64,
    t_self_ms: u64,
    renew_ms: u64,
    grace_ms: u64,

    /// `skew_max = max(T_owner × 500 ppm, RTT)`, `D_purge = max(2 ×
    /// cadence, RTT)`, `T_self = T_owner − 2·skew − D_purge` (REFUSED when
    /// not positive), `renew = clamp(T_self/3, 1 ms, 10 s)`, `grace =
    /// T_owner`. The owner derives with RTT 0 (no member to measure yet).
    pub fn derive(t_owner_ms: u64, rtt_ms: u64, revalidation_cadence_ms: u64) error{UnsafeClocks}!LeaseClocks {
        const skew = lease_clock.deriveSkewMaxMs(t_owner_ms, rtt_ms);
        const purge = lease_clock.derivePurgeMs(revalidation_cadence_ms, rtt_ms);
        const t_self = lease_clock.tSelfMs(t_owner_ms, skew, purge) orelse return error.UnsafeClocks;
        return .{ .t_owner_ms = t_owner_ms, .skew_max_ms = skew, .d_purge_ms = purge, .t_self_ms = t_self, .renew_ms = lease_clock.renewIntervalMs(t_self), .grace_ms = park.graceMs(t_owner_ms) };
    }

    /// The grant's clock words.
    pub fn grantHead(self: LeaseClocks, epoch: u64, term: u64, now_ms: u64, ack_prior_epoch: u64) verbs.GrantHead {
        return .{ .epoch = epoch, .term = term, .granted_at_owner_ms = now_ms, .ack_prior_epoch = ack_prior_epoch, .t_owner_ms = @intCast(self.t_owner_ms), .skew_max_ms = @intCast(self.skew_max_ms), .d_purge_ms = @intCast(self.d_purge_ms), .renew_ms = @intCast(self.renew_ms), .checkpoint_ceiling_ms = CHECKPOINT_LANDING_CEILING_MS, .n_ack = 0, .n_release = 0, .n_offered = 0, .reserved = @splat(0), .config_generation = 0 };
    }
};

/// The grant's `checkpoint_ceiling_ms`: `CHECKPOINT_MAX_AGE + 2 × tick` =
/// 1,100 ms at the shipped 50 ms flush (the death ledger's poll tick).
pub const CHECKPOINT_LANDING_CEILING_MS: u32 = 1100;

/// `T_owner` in force: the ONE constant, or the test seam's shortening
/// (`SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS`, the two-daemon venue legs).
pub fn tOwnerMs(seam_ms: ?u64) u64 {
    return seam_ms orelse lease_clock.T_OWNER_MS;
}

/// The seam read ONCE at the shard's arm (never a process static).
pub fn tOwnerMsFromEnv(env: *const std.process.Environ.Map) u64 {
    return tOwnerMs(knobs.optIntKnob("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", u64, env));
}

// ---------------------------------------------------------------------------
// The owner's RAM authority.
// ---------------------------------------------------------------------------

/// The longest mount path a census row keeps (the OQ-5 refusal names it;
/// a longer path is kept to this prefix — the wire cap is 4 KiB).
pub const MOUNT_KEEP: usize = 128;

pub const MemberState = struct {
    id: MemberId,
    role: Role,
    endpoint: EndpointWire,
    pid: u32,
    boot: [16]u8,
    epoch: u64,
    join_seq: u64,
    joined_ms: u64,
    renewed_ms: u64,
    deadline_ms: u64,
    acked_free_epoch: u64,
    mount: [MOUNT_KEEP]u8 = @splat(0),
    mount_len: u8 = 0,
    pr_key: u64,
    /// The row's identity words (writers; readers carry none).
    row: ?MemberRow,

    pub fn mountPath(self: *const MemberState) []const u8 {
        return self.mount[0..self.mount_len];
    }
};

/// What an eviction / the grace deadline ships to the death ledger.
pub const DeadMember = struct {
    id: MemberId,
    /// The writer identity (a reader holds no region: the sink ignores it).
    identity: ?WireIdentity,
    epoch: u64,
    pr_key: u64,
};

pub const DeathSink = struct {
    ctx: ?*anyopaque,
    noteDeath: *const fn (ctx: ?*anyopaque, dead: DeadMember) void,
};

pub const Liveness = enum { live, dead, unknown };

pub const JoinRequest = struct {
    id: MemberId,
    role: Role,
    endpoint: EndpointWire,
    pid: u32,
    boot: [16]u8,
    prior_epoch: ?u64,
    pr_key: u64,
    mount: []const u8,
    /// The writer's row words (null for a reader).
    row: ?MemberRow,
};

pub const Granted = struct {
    epoch: u64,
    /// The reclaim's acknowledged prior epoch (0 = a fresh join).
    ack_prior_epoch: u64,
};

pub const JoinVerdict = union(enum) {
    granted: Granted,
    /// The fresh-refusal half of grace is open: retry after.
    grace_refused: struct { retry_after_ms: u64 },
    /// A reclaim of an epoch this owner does not hold with the
    /// re-assertion half closed: self-fence, then re-join fresh.
    unknown_lease,
    /// OQ-5: the same `(node, mount_slot)` is LIVE at another mount point.
    mount_slot_collision: struct { other: [MOUNT_KEEP]u8, other_len: u8 },
    /// An empty id or a durable write that failed.
    rejected,
    io,
};

pub const RenewVerdict = union(enum) {
    granted: Granted,
    unknown_lease,
};

const Grace = struct {
    until_ms: u64,
    /// The writers the predecessor's roster named (the claim set).
    expected: std.ArrayList(MemberId) = .empty,
    /// Those that re-asserted so far.
    reasserted: std.ArrayList(MemberId) = .empty,
    /// The fresh-refusal half closed early.
    fresh_closed: bool = false,
    /// The deadline was reaped (the non-reclaimers are dead).
    reaped: bool = false,

    fn deinit(self: *Grace, gpa: Allocator) void {
        self.expected.deinit(gpa);
        self.reasserted.deinit(gpa);
    }

    fn expects(self: *const Grace, id: MemberId) bool {
        for (self.expected.items) |e| if (std.mem.eql(u8, &e, &id)) return true;
        return false;
    }

    fn hasReasserted(self: *const Grace, id: MemberId) bool {
        for (self.reasserted.items) |e| if (std.mem.eql(u8, &e, &id)) return true;
        return false;
    }
};

const DepartedKey = struct { key: u64, until_ms: u64 };

/// A pending OFFER the holder pushes to a wire requester: rides the
/// requester's next renewal `Grant` (the RecallForOffer push over a
/// dial-only wire — the renewal carriage is the belt).
const PendingOffer = struct { to: MemberId, slot: u16, holder: u32 };
const PendingRelease = struct { to: MemberId, slot: u16 };

pub const Owner = struct {
    gpa: Allocator,
    clocks: LeaseClocks,
    term: u64,
    /// The home volume this shard serves (the gauge's row).
    home_volume: u16,
    members: std.ArrayList(MemberState) = .empty,
    next_epoch: u64 = 1,
    join_seq: u64 = 0,
    grace: ?Grace = null,
    /// A departure's registrant key kept `2 × T_owner` (the age a death
    /// record can stand) for the `RecordDeath` screen.
    departed_keys: std.ArrayList(DepartedKey) = .empty,
    death_sink: ?DeathSink = null,
    rows: ?RowStore = null,
    offers: std.ArrayList(PendingOffer) = .empty,
    releases: std.ArrayList(PendingRelease) = .empty,

    /// `arm(term, prior_term)` REFUSES `term ≤ prior_term`: the successor
    /// bumps the term durably BEFORE arming. `expected` = the predecessor's
    /// writer roster; a non-empty one opens the grace window.
    pub fn arm(gpa: Allocator, clocks: LeaseClocks, home_volume: u16, term: u64, prior_term: u64, expected: []const MemberId, now_ms: u64) error{ StaleTerm, OutOfMemory }!Owner {
        if (term <= prior_term) return error.StaleTerm;
        var o: Owner = .{ .gpa = gpa, .clocks = clocks, .term = term, .home_volume = home_volume };
        if (expected.len != 0) {
            var g: Grace = .{ .until_ms = now_ms + clocks.grace_ms };
            errdefer g.deinit(gpa);
            try g.expected.appendSlice(gpa, expected);
            o.grace = g;
        }
        stats.membership_mode.set(@backingInt(Mode.owner));
        stats.membership_term.set(term);
        stats.membership_lease_ttl_ms.set(clocks.t_owner_ms);
        stats.membership_shards.add(1);
        return o;
    }

    pub fn deinit(self: *Owner) void {
        self.members.deinit(self.gpa);
        if (self.grace) |*g| g.deinit(self.gpa);
        self.departed_keys.deinit(self.gpa);
        self.offers.deinit(self.gpa);
        self.releases.deinit(self.gpa);
        stats.membership_shards.sub(1);
    }

    pub fn bindDeathSink(self: *Owner, sink: DeathSink) void {
        self.death_sink = sink;
    }

    pub fn bindRows(self: *Owner, rows: RowStore) void {
        self.rows = rows;
    }

    fn indexOf(self: *const Owner, id: MemberId) ?usize {
        for (self.members.items, 0..) |m, i| if (std.mem.eql(u8, &m.id, &id)) return i;
        return null;
    }

    pub fn find(self: *const Owner, id: MemberId) ?*const MemberState {
        const i = self.indexOf(id) orelse return null;
        return &self.members.items[i];
    }

    fn publishCounts(self: *const Owner) void {
        var writers: u64 = 0;
        var readers: u64 = 0;
        for (self.members.items) |m| if (m.role == .writer) {
            writers += 1;
        } else {
            readers += 1;
        };
        stats.membership_members.set(writers + readers);
        stats.membership_writers.set(writers);
        stats.membership_readers.set(readers);
    }

    fn writeRow(self: *Owner, m: *const MemberState) error{Io}!void {
        const rows = self.rows orelse return;
        const row = m.row orelse return;
        const identity = memberIdentity(m.id) orelse return;
        const v = row.encode();
        try rows.put(rows.ctx, cr.memberKey(stableOf(identity)).slice(), &v);
        stats.membership_registration_commits.inc();
    }

    fn deleteRow(self: *Owner, m: *const MemberState) void {
        const rows = self.rows orelse return;
        if (m.row == null) return;
        const identity = memberIdentity(m.id) orelse return;
        rows.delete(rows.ctx, cr.memberKey(stableOf(identity)).slice()) catch |e| log.warn("membership shard {d}: a departed member's row could not be deleted ({s}) — the dead-pid proof retires it at the next census", .{ self.home_volume, @errorName(e) });
        stats.membership_registration_commits.inc();
    }

    /// The grace window's halves, judged at `now_ms`.
    fn graceOpenForFresh(self: *Owner, now_ms: u64) ?u64 {
        const g = &(self.grace orelse return null);
        if (g.fresh_closed or now_ms >= g.until_ms) return null;
        return g.until_ms - now_ms;
    }

    /// The re-assertion window stands: a member the roster does not list
    /// may still be a predecessor's member on its way back (silence is
    /// unknown, never a death — the census page carries the word).
    pub fn graceOpenForReassert(self: *const Owner, now_ms: u64) bool {
        const g = self.grace orelse return false;
        return now_ms < g.until_ms;
    }

    /// An EXPECTED member's FIRST join while the re-assertion half stands
    /// is its re-assertion (no epoch needed). Once it re-asserted, or once
    /// the window closed, a join without an epoch is a FRESH join — the
    /// member left, was evicted (a re-join in place, decision 0003 §BO) or
    /// remounted: its predecessor's roster row expects nothing of it any
    /// more (a reclaim it could never hold answered `UnknownLease` for
    /// every later join of that identity under this owner).
    fn graceAwaitsReassertion(self: *const Owner, id: MemberId, now_ms: u64) bool {
        const g = self.grace orelse return false;
        return now_ms < g.until_ms and g.expects(id) and !g.hasReasserted(id);
    }

    fn noteReasserted(self: *Owner, id: MemberId) Allocator.Error!void {
        const g = &(self.grace orelse return);
        if (!g.expects(id) or g.hasReasserted(id)) return;
        try g.reasserted.append(self.gpa, id);
        // The fresh-refusal half closes EARLY once every expected member
        // re-asserted; the re-assertion half stays open to the deadline.
        if (g.reasserted.items.len == g.expected.items.len) g.fresh_closed = true;
    }

    /// `join` (M:1492-1651): `reclaim = prior_epoch.is_some() ∨ expected`;
    /// grace ∧ !reclaim ⇒ refused with the remaining window; a reclaim of
    /// an epoch this owner does not hold with the re-assertion half closed
    /// ⇒ `unknown_lease`; the OQ-5 collision; else a NEW epoch replaces
    /// any prior state and the row is written ONCE.
    pub fn join(self: *Owner, req: JoinRequest, now_ms: u64) JoinVerdict {
        if (std.mem.allEqual(u8, &req.id, 0)) return .rejected;
        const existing = self.indexOf(req.id);
        const reclaim = req.prior_epoch != null or self.graceAwaitsReassertion(req.id, now_ms);
        if (!reclaim) {
            if (self.graceOpenForFresh(now_ms)) |remaining| {
                stats.membership_grace_refusals.inc();
                return .{ .grace_refused = .{ .retry_after_ms = remaining } };
            }
        } else {
            // A reclaim whose epoch this owner does not hold: admissible
            // only while the re-assertion half is open (the predecessor's
            // epochs died with it; an EXPECTED member's first join is a
            // re-assertion even without an epoch). HELD: the epoch this
            // owner granted the member, or ANY epoch ≤ it from the SAME
            // incarnation (pid and boot) — a member that missed a grant's
            // reply (the lost-reply window of its own reclaim, or a park
            // past a renewal this owner served) names its last epoch; the
            // verb is idempotent against the state held, never a poison of
            // a live member. Another incarnation under the id (a remount:
            // another pid) is no reclaimer of this epoch.
            const held = if (existing) |i| blk: {
                const m = &self.members.items[i];
                const named = req.prior_epoch orelse 0;
                if (m.epoch == named) break :blk true;
                break :blk req.prior_epoch != null and named < m.epoch and m.pid == req.pid and std.mem.eql(u8, &m.boot, &req.boot);
            } else false;
            if (!held and !self.graceOpenForReassert(now_ms)) {
                stats.membership_reclaim_refusals.inc();
                return .unknown_lease;
            }
        }
        // OQ-5: the same id LIVE at another mount point.
        if (existing) |i| {
            const m = &self.members.items[i];
            if (now_ms < m.deadline_ms and req.mount.len != 0 and !std.mem.eql(u8, m.mountPath(), req.mount[0..@min(req.mount.len, MOUNT_KEEP)]) and req.prior_epoch == null) {
                stats.membership_mount_slot_collisions.inc();
                return .{ .mount_slot_collision = .{ .other = m.mount, .other_len = m.mount_len } };
            }
        }
        const epoch = self.next_epoch;
        self.next_epoch += 1;
        self.join_seq += 1;
        var m: MemberState = .{
            .id = req.id,
            .role = req.role,
            .endpoint = req.endpoint,
            .pid = req.pid,
            .boot = req.boot,
            .epoch = epoch,
            .join_seq = self.join_seq,
            .joined_ms = now_ms,
            .renewed_ms = now_ms,
            .deadline_ms = now_ms + self.clocks.t_owner_ms,
            .acked_free_epoch = 0,
            .pr_key = req.pr_key,
            .row = req.row,
        };
        const keep: u8 = @intCast(@min(req.mount.len, MOUNT_KEEP));
        @memcpy(m.mount[0..keep], req.mount[0..keep]);
        m.mount_len = keep;
        if (m.row) |*r| {
            r.epoch = epoch;
            r.term = self.term;
        }
        const ack_prior: u64 = if (reclaim) (req.prior_epoch orelse 0) else 0;
        if (existing) |i| {
            self.members.items[i] = m;
        } else {
            // Room for every member's departure: `rememberDepartedKey`
            // never allocates.
            self.departed_keys.ensureUnusedCapacity(self.gpa, self.members.items.len + 1) catch return .io;
            self.members.append(self.gpa, m) catch return .io;
        }
        self.writeRow(&m) catch return .io;
        if (reclaim) {
            stats.membership_grace_reclaims.inc();
            self.noteReasserted(req.id) catch return .io;
        }
        stats.membership_joins.inc();
        self.publishCounts();
        return .{ .granted = .{ .epoch = epoch, .ack_prior_epoch = ack_prior } };
    }

    /// `renew`: ONE probe + two stores — the plane's hot op, commits
    /// nothing. An unknown `(id, epoch)` is `unknown_lease`.
    pub fn renew(self: *Owner, id: MemberId, epoch: u64, acked_free_epoch: u64, now_ms: u64) RenewVerdict {
        const i = self.indexOf(id) orelse {
            stats.membership_renew_refusals.inc();
            return .unknown_lease;
        };
        const m = &self.members.items[i];
        if (m.epoch != epoch or now_ms >= m.deadline_ms) {
            stats.membership_renew_refusals.inc();
            return .unknown_lease;
        }
        m.renewed_ms = now_ms;
        m.deadline_ms = now_ms + self.clocks.t_owner_ms;
        m.acked_free_epoch = acked_free_epoch;
        stats.membership_renewals.inc();
        return .{ .granted = .{ .epoch = epoch, .ack_prior_epoch = 0 } };
    }

    /// The clean departure: out of the census, its key remembered, the
    /// row deleted; never a death.
    pub fn leave(self: *Owner, id: MemberId, now_ms: u64) bool {
        const i = self.indexOf(id) orelse return false;
        const m = self.members.items[i];
        self.rememberDepartedKey(m.pr_key, now_ms);
        self.deleteRow(&m);
        _ = self.members.swapRemove(i);
        self.publishCounts();
        return true;
    }

    fn rememberDepartedKey(self: *Owner, key: u64, now_ms: u64) void {
        if (key == 0) return;
        self.departed_keys.appendAssumeCapacity(.{ .key = key, .until_ms = now_ms + DEPARTED_KEY_TTLS * self.clocks.t_owner_ms });
    }

    fn evict(self: *Owner, i: usize, now_ms: u64, epoch_word: u64) void {
        const m = self.members.items[i];
        self.rememberDepartedKey(m.pr_key, now_ms);
        self.deleteRow(&m);
        _ = self.members.swapRemove(i);
        stats.membership_evictions.inc();
        if (self.death_sink) |s| s.noteDeath(s.ctx, .{ .id = m.id, .identity = memberIdentity(m.id), .epoch = epoch_word, .pr_key = m.pr_key });
    }

    /// `expire_due` on the owner's cadence: every member with `now ≥
    /// deadline` is evicted and shipped to the sink — a member the owner
    /// evicts here passed `T_owner` under a LIVE owner, so the successor's
    /// grace never lists it: never a reclaimer.
    pub fn expireDue(self: *Owner, now_ms: u64) u32 {
        var n: u32 = 0;
        var i: usize = 0;
        while (i < self.members.items.len) {
            if (now_ms >= self.members.items[i].deadline_ms) {
                self.evict(i, now_ms, self.members.items[i].epoch);
                n += 1;
            } else i += 1;
        }
        self.pruneDepartedKeys(now_ms);
        if (n != 0) self.publishCounts();
        return n;
    }

    fn pruneDepartedKeys(self: *Owner, now_ms: u64) void {
        var i: usize = 0;
        while (i < self.departed_keys.items.len) {
            if (now_ms >= self.departed_keys.items[i].until_ms) {
                _ = self.departed_keys.swapRemove(i);
            } else i += 1;
        }
    }

    /// `reap_grace_deadline`: at the deadline the EXPECTED members that
    /// never re-asserted are DEAD — shipped NOW with `epoch 0` and the
    /// roster's key (after grace, never at `T_owner`). Once.
    pub fn reapGraceDeadline(self: *Owner, now_ms: u64, roster_key: *const fn (id: MemberId) u64) u32 {
        const g = &(self.grace orelse return 0);
        if (g.reaped or now_ms < g.until_ms) return 0;
        g.reaped = true;
        var n: u32 = 0;
        for (g.expected.items) |id| {
            if (g.hasReasserted(id)) continue;
            if (self.death_sink) |s| s.noteDeath(s.ctx, .{ .id = id, .identity = memberIdentity(id), .epoch = 0, .pr_key = roster_key(id) });
            n += 1;
        }
        return n;
    }

    pub fn graceRemainingMs(self: *const Owner, now_ms: u64) u64 {
        const g = self.grace orelse return 0;
        return g.until_ms -| now_ms;
    }

    /// Three-valued liveness (D:564): the F2 predicate refuses `dead`
    /// alone.
    pub fn liveness(self: *const Owner, id: MemberId, now_ms: u64) Liveness {
        if (self.find(id)) |m| return if (now_ms < m.deadline_ms) .live else .dead;
        return if (self.graceOpenForReassert(now_ms)) .unknown else .dead;
    }

    /// The `RecordDeath` screen's word: "not listed" = "not proven live".
    pub fn memberIsLive(self: *const Owner, id: MemberId, now_ms: u64) bool {
        return self.liveness(id, now_ms) == .live;
    }

    /// The key the census registered for a member (null = unknown).
    pub fn registeredKey(self: *const Owner, id: MemberId) ?u64 {
        const m = self.find(id) orelse return null;
        return m.pr_key;
    }

    /// Is `key` held — a live member's or a departure's inside its TTL?
    pub fn holdsKey(self: *const Owner, key: u64) bool {
        for (self.members.items) |m| if (m.pr_key != 0 and m.pr_key == key) return true;
        for (self.departed_keys.items) |d| if (d.key == key) return true;
        return false;
    }

    pub fn endpointOf(self: *const Owner, id: MemberId) ?EndpointWire {
        const m = self.find(id) orelse return null;
        return if (m.endpoint.isNone()) null else m.endpoint;
    }

    /// The census page from `cursor` (a join_seq), ≤ `limit` rows, in
    /// join order; `next` = the cursor of the page after, null at the end.
    pub fn census(self: *const Owner, cursor: u64, limit: u32, now_ms: u64, b: *codec.Builder) codec.BuildError!verbs.CensusPageHead {
        stats.membership_census_serves.inc();
        var sorted: [verbs.CENSUS_PAGE_MAX + 1]usize = undefined;
        var n: usize = 0;
        for (self.members.items, 0..) |m, i| {
            if (m.join_seq < cursor) continue;
            if (n == sorted.len) break;
            sorted[n] = i;
            n += 1;
        }
        std.mem.sort(usize, sorted[0..n], self, struct {
            fn lt(o: *const Owner, a: usize, c: usize) bool {
                return o.members.items[a].join_seq < o.members.items[c].join_seq;
            }
        }.lt);
        const take: usize = @min(n, @min(limit, verbs.CENSUS_PAGE_MAX));
        for (sorted[0..take]) |i| {
            const m = &self.members.items[i];
            const state: verbs.MemberState = if (now_ms < m.deadline_ms) .live else .departed;
            try verbs.CensusPage.encodeRow(b, .{ .head = .{ .id = m.id, .boot = m.boot, .endpoint = m.endpoint, .role = @backingInt(m.role), .state = @backingInt(state), .mount_len = m.mount_len, .pid = m.pid, .reserved = 0, .epoch = m.epoch, .age_ms = now_ms -| m.joined_ms, .expires_in_ms = m.deadline_ms -| now_ms, .acked_free_epoch = m.acked_free_epoch }, .mount = m.mountPath() });
        }
        const has_next = take < n;
        return .{ .n = @intCast(take), .has_next = @intFromBool(has_next), .grace_open = @intFromBool(self.graceOpenForReassert(now_ms)), .reserved = @splat(0), .next = if (has_next) self.members.items[sorted[take]].join_seq else 0 };
    }

    // ---- the push carriage ----

    /// A holder's OFFER to a wire requester: carried on the requester's
    /// next renewal `Grant` (`offered_slots`).
    pub fn pushOffer(self: *Owner, to: MemberId, slot: u16, holder: u32) Allocator.Error!void {
        for (self.offers.items) |o| if (o.slot == slot and std.mem.eql(u8, &o.to, &to)) return;
        try self.offers.append(self.gpa, .{ .to = to, .slot = slot, .holder = holder });
    }

    /// A release notice: the requester's standing offer lapsed / the slot
    /// moved; carried the same way (`slot_release_notices`).
    pub fn pushReleaseNotice(self: *Owner, to: MemberId, slot: u16) Allocator.Error!void {
        try self.releases.append(self.gpa, .{ .to = to, .slot = slot });
    }

    /// Drain the carriage for `id` into the grant's tails (the builder's
    /// scratch); the pending rows are consumed.
    pub fn takeCarriage(self: *Owner, id: MemberId, offered: *std.ArrayList(verbs.OfferedSlotWire), notices: *std.ArrayList(u16)) Allocator.Error!void {
        var i: usize = 0;
        while (i < self.offers.items.len) {
            const o = self.offers.items[i];
            if (std.mem.eql(u8, &o.to, &id)) {
                try offered.append(self.gpa, .{ .slot = o.slot, .reserved = 0, .holder = o.holder });
                _ = self.offers.orderedRemove(i);
            } else i += 1;
        }
        i = 0;
        while (i < self.releases.items.len) {
            const r = self.releases.items[i];
            if (std.mem.eql(u8, &r.to, &id)) {
                try notices.append(self.gpa, r.slot);
                _ = self.releases.orderedRemove(i);
            } else i += 1;
        }
    }

    // ---- the `clients` rows ----

    pub fn registrationSource(self: *Owner, local: *const claim.Identity, probe: claim.PidProbe, now: *const fn (ctx: ?*anyopaque) u64, now_ctx: ?*anyopaque) OwnerRegistrations {
        return .{ .owner = self, .local = local, .probe = probe, .now = now, .now_ctx = now_ctx };
    }
};

/// A departure's key stands as long as its death record can: `2 ×
/// T_owner` (one TTL for every manager to act, one for the grace).
pub const DEPARTED_KEY_TTLS: u64 = 2;

/// The `clients` seam: every member of this shard as a registration row
/// (writers through their row's dead-pid proof; readers as live rows).
pub const OwnerRegistrations = struct {
    owner: *Owner,
    local: *const claim.Identity,
    probe: claim.PidProbe,
    /// The owner's monotonic clock in ms (the lease deadlines' frame).
    now: *const fn (ctx: ?*anyopaque) u64,
    now_ctx: ?*anyopaque,

    pub fn source(self: *OwnerRegistrations) claim.RegistrationSource {
        return .{ .ctx = self, .append = &append };
    }

    fn append(ctx: ?*anyopaque, gpa: Allocator, out: *std.ArrayList(claim.Registration)) Allocator.Error!void {
        const self: *OwnerRegistrations = @ptrCast(@alignCast(ctx.?));
        const now_ns = clock.realtimeCoarseS() * std.time.ns_per_s;
        const now_ms = self.now(self.now_ctx);
        for (self.owner.members.items) |m| {
            const live = now_ms < m.deadline_ms;
            if (m.row) |r| {
                try out.append(gpa, r.registration(self.local.*, @intCast(now_ns), self.probe, live));
            } else {
                try out.append(gpa, .{ .kind = .member, .id = std.mem.readInt(u64, m.id[0..8], .big), .pid = m.pid, .boot_id = m.boot, .hostname_hash = 0, .heartbeat_ts_s = @intCast(@divFloor(now_ns, std.time.ns_per_s)), .age_s = (now_ms -| m.renewed_ms) / 1000, .state = if (live) .live else .stale });
            }
        }
    }
};

// ---------------------------------------------------------------------------
// The owner's wire face: the four membership verbs on the ONE listener.
// ---------------------------------------------------------------------------

/// The renewal's window declarations' target (charter §5.5 "every live
/// writer declares its held windows on each renewal"): the allocation
/// holder of the declared volume adopts them (`plane_arm.HolderPlane.
/// declareWindows`). Null = no holder plane in this process.
pub const WindowSink = struct {
    ctx: *anyopaque,
    declare: *const fn (ctx: *anyopaque, identity: WireIdentity, vol_tag: u64, ranges: []align(1) const verbs.GrantRunWire) void,
};

/// The owner's `set_config` generation the grant carries (the online
/// `volume add-data` / `remove-data` push): the wire plane binds the
/// process's word; null = 0 on the wire (not carried).
pub const GenerationSource = struct {
    ctx: ?*anyopaque,
    read: *const fn (ctx: ?*anyopaque) u64,
};

pub const OwnerService = struct {
    owner: *Owner,
    /// The data volumes the renewal's window declarations are screened by.
    data_volumes: []const screen.DataVolumeBounds = &.{},
    /// Where a screened, granted renewal's declarations go.
    windows: ?WindowSink = null,
    /// The owner's monotonic clock in ms (tests inject a manual one).
    now: *const fn (ctx: ?*anyopaque) u64,
    now_ctx: ?*anyopaque = null,
    generation: ?GenerationSource = null,

    pub fn register(self: *OwnerService, svc: *service.Service) void {
        svc.register(.join, .{ .ctx = self, .serve = &serveJoin });
        svc.register(.renew, .{ .ctx = self, .serve = &serveRenew });
        svc.register(.leave, .{ .ctx = self, .serve = &serveLeave });
        svc.register(.census, .{ .ctx = self, .serve = &serveCensus });
    }

    fn grant(self: *OwnerService, g: Granted, now_ms: u64, id: MemberId, scratch: []u8) service.Answer {
        var offered: std.ArrayList(verbs.OfferedSlotWire) = .empty;
        defer offered.deinit(self.owner.gpa);
        var notices: std.ArrayList(u16) = .empty;
        defer notices.deinit(self.owner.gpa);
        self.owner.takeCarriage(id, &offered, &notices) catch return service.Answer.refused(error.OutOfMemory, "carriage");
        const need = offered.items.len * @sizeOf(verbs.OfferedSlotWire) + notices.items.len * 2;
        if (scratch.len < need) return service.Answer.refused(error.Unexpected, "reply scratch too small for the carriage");
        const off: []align(1) verbs.OfferedSlotWire = @ptrCast(scratch[0 .. offered.items.len * @sizeOf(verbs.OfferedSlotWire)]);
        for (offered.items, 0..) |o, i| off[i] = o;
        const nt: []align(1) u16 = @ptrCast(scratch[offered.items.len * @sizeOf(verbs.OfferedSlotWire) ..][0 .. notices.items.len * 2]);
        for (notices.items, 0..) |s, i| nt[i] = s;
        var head = self.owner.clocks.grantHead(g.epoch, self.owner.term, now_ms, g.ack_prior_epoch);
        head.n_offered = @intCast(off.len);
        head.n_release = @intCast(nt.len);
        if (self.generation) |src| head.config_generation = src.read(src.ctx);
        return .{ .ok = .{ .join = .{ .head = head, .slot_leases_ack = &.{}, .slot_release_notices = nt, .offered_slots = off } } };
    }

    fn serveJoin(ctx: *anyopaque, req: *const service.Request, scratch: []u8) service.Answer {
        const self: *OwnerService = @ptrCast(@alignCast(ctx));
        const t0 = clock.monotonicNs();
        defer stats.membership_renew_serve_ns.record(clock.monotonicNs() - t0);
        const r = req.payload.join;
        if (!r.head.check()) return service.Answer.rejected("join head");
        switch (screen.join(r)) {
            .ok => {},
            .rejected => |why| return service.Answer.rejected(@tagName(why)),
        }
        const role: Role = @fromBackingInt(r.head.role);
        // A writer speaks for itself alone: its id derives the peer.
        var row: ?MemberRow = null;
        if (role == .writer) {
            const identity = memberIdentity(r.head.id) orelse return service.Answer.rejected("writer id");
            switch (screen.identityPeer(identity, req.peer, false)) {
                .ok => {},
                .rejected => |why| return service.Answer.rejected(@tagName(why)),
            }
            row = .{ .writer_id = if (req.peer) |p| p.writer_id else 0, .ts_ns = @intCast(clock.realtimeCoarseS() * std.time.ns_per_s), .pid = r.head.pid, .start_ticks = 0, .boot_id = r.head.boot, .hostname_hash = identity.node_token, .epoch = 0, .term = 0 };
        }
        const now_ms = self.now(self.now_ctx);
        const v = self.owner.join(.{
            .id = r.head.id,
            .role = role,
            .endpoint = if (r.head.flags & verbs.JOIN_FLAG_HAS_ENDPOINT != 0) r.head.endpoint else .none,
            .pid = r.head.pid,
            .boot = r.head.boot,
            .prior_epoch = if (r.head.flags & verbs.JOIN_FLAG_HAS_PRIOR_EPOCH != 0) r.head.prior_epoch else null,
            .pr_key = r.head.pr_key,
            .mount = r.bytes,
            .row = row,
        }, now_ms);
        return switch (v) {
            .granted => |g| self.grant(g, now_ms, r.head.id, scratch),
            .grace_refused => |gr| .{ .status = .{ .status = .deferred, .err = error.MembershipPending, .reason = "the successor's grace window admits only re-assertions", .retry_after_ms = @intCast(@min(gr.retry_after_ms, std.math.maxInt(u32))) } },
            .unknown_lease => .{ .status = .{ .status = .unknown_lease } },
            .mount_slot_collision => |c| blk: {
                // The other mount point travels as the reason (the scratch
                // outlives the reply's encoding).
                @memcpy(scratch[0..c.other_len], c.other[0..c.other_len]);
                break :blk .{ .status = .{ .status = .busy, .err = error.Busy, .reason = scratch[0..c.other_len] } };
            },
            .rejected => service.Answer.rejected("empty member id"),
            .io => service.Answer.refused(error.InputOutput, "the member row could not be written"),
        };
    }

    fn serveRenew(ctx: *anyopaque, req: *const service.Request, scratch: []u8) service.Answer {
        const self: *OwnerService = @ptrCast(@alignCast(ctx));
        const t0 = clock.monotonicNs();
        defer stats.membership_renew_serve_ns.record(clock.monotonicNs() - t0);
        const r = req.payload.renew;
        switch (screen.renew(r, self.data_volumes)) {
            .ok => {},
            .rejected => |why| return service.Answer.rejected(@tagName(why)),
        }
        const now_ms = self.now(self.now_ctx);
        return switch (self.owner.renew(r.head.id, r.head.epoch, r.head.acked_free_epoch, now_ms)) {
            .granted => |g| blk: {
                // The declarations of a LIVE member (the renewal granted
                // is the liveness proof): one per data volume, screened
                // above, to the holder.
                if (self.windows) |ws| if (memberIdentity(r.head.id)) |identity| {
                    var it = r.windows();
                    while (it.next()) |w| ws.declare(ws.ctx, identity, w.vol_tag, w.ranges);
                };
                const a = self.grant(g, now_ms, r.head.id, scratch);
                break :blk switch (a) {
                    .ok => |rep| .{ .ok = .{ .renew = rep.join } },
                    .status => a,
                };
            },
            .unknown_lease => .{ .status = .{ .status = .unknown_lease } },
        };
    }

    fn serveLeave(ctx: *anyopaque, req: *const service.Request, _: []u8) service.Answer {
        const self: *OwnerService = @ptrCast(@alignCast(ctx));
        const left = self.owner.leave(req.payload.leave.id, self.now(self.now_ctx));
        return .{ .ok = .{ .leave = if (left) verbs.Ack.fresh else verbs.Ack.replay } };
    }

    fn serveCensus(ctx: *anyopaque, req: *const service.Request, scratch: []u8) service.Answer {
        const self: *OwnerService = @ptrCast(@alignCast(ctx));
        const r = req.payload.census;
        var b: codec.Builder = .init(scratch);
        const head = self.owner.census(r.cursor, r.limit, self.now(self.now_ctx), &b) catch return service.Answer.refused(error.ValueTooLarge, "census page overflowed the scratch");
        return .{ .ok = .{ .census = .{ .head = head, .rows_bytes = b.written() } } };
    }
};

// ---------------------------------------------------------------------------
// The member side: the renewal tick.
// ---------------------------------------------------------------------------

/// The member's wire to its home shard: a loopback session or a dialed
/// client (`join_ladder.WireLink`).
pub const Link = struct {
    ctx: *anyopaque,
    call: *const fn (ctx: *anyopaque, req: verbs.Request) errors.Error!verbs.Reply,
};

/// The carriage's consumer: offers and release notices for this member's
/// slot plane (the push's receiving end).
pub const CarriageSink = struct {
    ctx: ?*anyopaque,
    offered: *const fn (ctx: ?*anyopaque, slot: u16, holder: u32) void,
    released: *const fn (ctx: ?*anyopaque, slot: u16) void,
};

/// `evicted`: the owner answered `UnknownLease` to a member armed to re-join
/// in place (decision 0003 §BO) — nothing touched the gate or the words;
/// the caller retires the incarnation and re-joins, or poisons.
pub const TickVerdict = enum { idle, renewed, rejoined, parked, fenced, evicted };

/// The carriage a member acts on per grant: the rotor cap (`2M` ≤ 128).
pub const CARRIAGE_MAX: usize = 128;

pub const Member = struct {
    gpa: Allocator,
    id: MemberId,
    role: Role,
    link: Link,
    words: lease_clock.MemberLeaseWords = .{},
    epoch: u64 = 0,
    label: u64 = 0,
    clocks: ?LeaseClocks = null,
    pid: u32,
    boot: [16]u8,
    endpoint: EndpointWire,
    pr_key: u64,
    mount: []const u8,
    /// The symmetric appender's gate (null = a remote-custody lease: the
    /// poison class).
    gate: ?*park.ParkGate = null,
    poison: park.PoisonSink,
    carriage: ?CarriageSink = null,
    /// The venue a parked member observed through the home volume's claim.
    successor_observed: bool = false,
    /// The windows the renewal declares (the block-grant plane's).
    windows_bytes: []const u8 = "",
    n_windows: u16 = 0,
    /// The owner's `set_config` generation the last grant carried (0 =
    /// never carried): the beat compares it with the process's own word
    /// and pulls the record at a bump (`wire_plane.learnSetConfig`).
    config_generation: u64 = 0,
    /// THE RE-JOIN IN PLACE (charter §5.5; decision 0003 §BO): the owner's
    /// `UnknownLease` — at a reclaim under the park or a renewal — is the
    /// verdict `evicted`, never the poison here: the caller judges what
    /// the incarnation holds un-landed and re-joins or poisons. Set before
    /// the beat starts (read on the beat alone).
    rejoin_armed: bool = false,

    fn joinReq(self: *const Member, prior_epoch: ?u64) verbs.Request {
        var flags: u8 = 0;
        if (prior_epoch != null) flags |= verbs.JOIN_FLAG_HAS_PRIOR_EPOCH;
        if (!self.endpoint.isNone()) flags |= verbs.JOIN_FLAG_HAS_ENDPOINT;
        const mount_len: u16 = @intCast(@min(self.mount.len, verbs.MOUNT_PATH_MAX));
        return .{ .join = .{ .head = .{ .role = @backingInt(self.role), .flags = flags, .mount_len = mount_len, .pid = self.pid, .id = self.id, .boot = self.boot, .endpoint = self.endpoint, .reserved = @splat(0), .prior_epoch = prior_epoch orelse 0, .pr_key = self.pr_key }, .bytes = self.mount[0..mount_len] } };
    }

    fn learn(self: *Member, g: verbs.Grant, sent_ms: u64) error{UnsafeClocks}!void {
        const h = g.head;
        self.clocks = .{ .t_owner_ms = h.t_owner_ms, .skew_max_ms = h.skew_max_ms, .d_purge_ms = h.d_purge_ms, .t_self_ms = lease_clock.grantTSelfMs(h.t_owner_ms, h.skew_max_ms, h.d_purge_ms), .renew_ms = h.renew_ms, .grace_ms = park.graceMs(h.t_owner_ms) };
        if (self.clocks.?.t_self_ms == 0) return error.UnsafeClocks;
        self.epoch = h.epoch;
        self.label += 1;
        self.config_generation = h.config_generation;
        _ = self.words.renewed(self.label, sent_ms, self.clocks.?.t_self_ms, h.renew_ms);
        stats.membership_epoch.set(h.epoch);
        stats.membership_renew_cadence_ms.set(h.renew_ms);
        stats.membership_self_deadline_ms.set(sent_ms +| self.clocks.?.t_self_ms);
        stats.membership_mode.set(@backingInt(Mode.member));
        if (self.carriage) |c| {
            // The grant's tails live in the link's reply buffer, which the
            // sink's own calls reuse: copied out before the first callback.
            var offers: [CARRIAGE_MAX]verbs.OfferedSlotWire = undefined;
            const n_off = @min(g.offered_slots.len, offers.len);
            for (g.offered_slots[0..n_off], 0..) |o, i| offers[i] = o;
            var notices: [CARRIAGE_MAX]u16 = undefined;
            const n_rel = @min(g.slot_release_notices.len, notices.len);
            for (g.slot_release_notices[0..n_rel], 0..) |s, i| notices[i] = s;
            for (offers[0..n_off]) |o| c.offered(c.ctx, o.slot, o.holder);
            for (notices[0..n_rel]) |s| c.released(c.ctx, s);
        }
    }

    pub const JoinOutcome = enum { granted, grace_refused, unknown_lease, collision, failed };

    /// The join (or a reclaim with `prior_epoch`), anchored on the SEND
    /// instant.
    pub fn join(self: *Member, now_ms: u64, prior_epoch: ?u64) JoinOutcome {
        const rep = self.link.call(self.link.ctx, self.joinReq(prior_epoch)) catch |e| return switch (e) {
            error.MembershipPending, error.Deferred => .grace_refused,
            error.UnknownLease => .unknown_lease,
            error.Busy => .collision,
            else => .failed,
        };
        self.learn(rep.join, now_ms) catch return .failed;
        return .granted;
    }

    pub const RenewOutcome = enum { granted, unknown_lease, failed };

    fn renew(self: *Member, now_ms: u64) RenewOutcome {
        const t0 = clock.monotonicNs();
        const rep = self.link.call(self.link.ctx, .{ .renew = .{ .head = .{ .id = self.id, .epoch = self.epoch, .acked_free_epoch = self.words.acked_free_epoch.load(.acquire), .n_windows = self.n_windows, .reserved = @splat(0) }, .windows_bytes = self.windows_bytes } }) catch |e| return switch (e) {
            error.UnknownLease => .unknown_lease,
            else => .failed,
        };
        const rtt = clock.monotonicNs() - t0;
        stats.membership_renew_phase_ns[@backingInt(RenewPhase.carry_wait)].record(0);
        stats.membership_renew_phase_ns[@backingInt(RenewPhase.rtt)].record(rtt);
        stats.membership_renew_phase_ns[@backingInt(RenewPhase.total)].record(rtt);
        self.learn(rep.renew, now_ms) catch return .failed;
        return .granted;
    }

    fn fenceClass(self: *const Member) park.FenceClass {
        return if (self.gate != null) .symmetric_appender else .remote_custody;
    }

    fn selfFence(self: *Member, now_mono_ns: u64) TickVerdict {
        if (self.gate) |g| {
            // The gate's CAS is the one latch between the beat and the
            // committers judging `T_self` at the pre-admission gate: the
            // count follows the park that landed. The words stay unfenced
            // (the park re-arms for the reclaim's grant).
            if (park.fenceAtTSelf(.symmetric_appender, g, self.poison, now_mono_ns)) {
                stats.membership_self_fences.inc();
                stats.membership_self_fenced.set(1);
            }
            return .parked;
        }
        if (!self.words.fence()) return .fenced;
        stats.membership_self_fences.inc();
        stats.membership_self_fenced.set(1);
        _ = park.fenceAtTSelf(.remote_custody, null, self.poison, now_mono_ns);
        return .fenced;
    }

    /// The pre-admission gate's `T_self` judgement (`park.SelfFence`): a
    /// committer past the member's deadline parks the gate itself — the
    /// beat may be stalled in a wire call; the deadline is the member's
    /// clock. Nothing to judge before the first grant (no deadline learnt).
    pub fn selfFenceSeam(self: *Member) park.SelfFence {
        return .{ .ctx = self, .fenceIfDue = &selfFenceIfDue };
    }

    fn selfFenceIfDue(ctx: *anyopaque, now_ms: u64, now_mono_ns: u64) void {
        const self: *Member = @ptrCast(@alignCast(ctx));
        if (self.words.learnedLabel().label == 0) return;
        if (self.words.fenced.load(.acquire)) return;
        if (self.words.selfFenceDue(now_ms)) _ = self.selfFence(now_mono_ns);
    }

    /// `member_renewal_tick_decided` (M:3587-3626): a parked member runs
    /// the reclaim beat (expiry judged first); else a due renewal; an
    /// `UnknownLease` before `T_self` fences terminally and never parks;
    /// `T_self` passed is the class's action.
    pub fn tick(self: *Member, now_ms: u64, now_mono_ns: u64) TickVerdict {
        if (self.gate) |g| {
            if (g.isExpired()) return .fenced;
            if (g.isParked()) {
                const t0 = clock.monotonicNs();
                const answer: park.ReclaimAnswer = switch (self.join(now_ms, self.epoch)) {
                    .granted => .granted,
                    .unknown_lease => .unknown_lease,
                    else => .not_now,
                };
                // Expiry judged first (`reclaimUnderPark`'s order): a park
                // past `T_park_max` is the lattice's terminal verdict.
                if (answer == .unknown_lease and self.rejoin_armed and !g.expireIfDue(now_mono_ns, self.poison) and !g.isExpired()) return .evicted;
                return switch (park.reclaimUnderPark(g, answer, now_mono_ns, clock.monotonicNs() - t0, self.poison)) {
                    .rejoined => .rejoined,
                    .fenced => blk: {
                        _ = self.words.fence();
                        stats.membership_self_fenced.set(1);
                        break :blk .fenced;
                    },
                    .parked => .parked,
                };
            }
        }
        if (self.words.fenced.load(.acquire)) return .fenced;
        if (self.words.selfFenceDue(now_ms)) return self.selfFence(now_mono_ns);
        if (now_ms < self.words.renew_at_ms.load(.acquire)) return .idle;
        switch (self.renew(now_ms)) {
            .granted => {
                self.words.renew_at_ms.store(now_ms +| self.clocks.?.renew_ms, .release);
                return .renewed;
            },
            .unknown_lease => {
                if (self.rejoin_armed) return .evicted;
                // The owner's own verdict: the lease is gone — terminal
                // whatever the class (a park would stand for nothing).
                _ = self.words.fence();
                stats.membership_self_fences.inc();
                stats.membership_self_fenced.set(1);
                self.poison.poison(self.poison.ctx, "the home shard answered UnknownLease before T_self");
                return .fenced;
            },
            .failed => {
                // Paced at the beat: the deadline decides.
                self.words.renew_at_ms.store(now_ms +| (if (self.clocks) |c| c.renew_ms else lease_clock.SHIPPED_BEAT_MS), .release);
                return .idle;
            },
        }
    }

    /// The clean leave; refused set-wide over a standing park.
    pub fn leave(self: *Member) error{ParkedLeaveRefused}!void {
        if (self.gate) |g| if (!g.leaveAllowed()) return error.ParkedLeaveRefused;
        _ = self.link.call(self.link.ctx, .{ .leave = .{ .id = self.id } }) catch {};
        stats.membership_mode.set(@backingInt(Mode.off));
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

fn testIdentity(n: u32) WireIdentity {
    return .{ .node_token = 0xA000 + n, .writer_id = 0xB000 + n, .mount_slot = n, .reserved = 0 };
}

fn rowOf(n: u32) MemberRow {
    return .{ .writer_id = 0xB000 + n, .ts_ns = 1_000_000_000, .pid = 100 + n, .start_ticks = 7, .boot_id = "boot-boot-boot-0".*, .hostname_hash = 0x55, .epoch = 0, .term = 0 };
}

fn joinReqOf(n: u32, prior: ?u64, mount: []const u8) JoinRequest {
    const id = testIdentity(n);
    return .{ .id = writerMemberId(id), .role = .writer, .endpoint = .none, .pid = 100 + n, .boot = "boot-boot-boot-0".*, .prior_epoch = prior, .pr_key = 0x1000 + n, .mount = mount, .row = rowOf(n) };
}

const Deaths = struct {
    list: std.ArrayList(DeadMember) = .empty,
    fn note(ctx: ?*anyopaque, d: DeadMember) void {
        const self: *Deaths = @ptrCast(@alignCast(ctx.?));
        self.list.append(testing.allocator, d) catch unreachable;
    }
    fn sink(self: *Deaths) DeathSink {
        return .{ .ctx = self, .noteDeath = &note };
    }
};

const Rows = struct {
    puts: u32 = 0,
    deletes: u32 = 0,
    last: [claim.CLAIM_LEN]u8 = undefined,
    fn put(ctx: ?*anyopaque, k: []const u8, v: []const u8) error{Io}!void {
        const self: *Rows = @ptrCast(@alignCast(ctx.?));
        std.debug.assert(cr.tagOfKey(k) == .member);
        self.puts += 1;
        @memcpy(&self.last, v);
    }
    fn delete(ctx: ?*anyopaque, k: []const u8) error{Io}!void {
        const self: *Rows = @ptrCast(@alignCast(ctx.?));
        std.debug.assert(cr.tagOfKey(k) == .member);
        self.deletes += 1;
    }
    fn store(self: *Rows) RowStore {
        return .{ .ctx = self, .put = &put, .delete = &delete };
    }
};

fn rosterKey(_: MemberId) u64 {
    return 0x9999;
}

test "the member row is the writer claim's shape at kind 1: round-trip, kind 0 refused, checksum total; a member id names its writer and a reader id names none; the clocks derive from T_owner alone" {
    const r = rowOf(3);
    const bytes = r.encode();
    try testing.expectEqual(claim.CLAIM_LEN, bytes.len);
    try testing.expectEqual(claim.KIND_MEMBER, bytes[5]);
    const d = try MemberRow.decode(&bytes);
    try testing.expectEqual(r.writer_id, d.writer_id);
    try testing.expectEqual(r.pid, d.pid);
    try testing.expectError(error.Corrupt, claim.Claim.decode(&bytes));
    const wc = (claim.Identity{ .boot_id = "boot-boot-boot-0".*, .pid = 1, .start_ticks = 1, .hostname_hash = 1 }).claim(0, 0).encode();
    try testing.expectError(error.Corrupt, MemberRow.decode(&wc));
    var torn = bytes;
    torn[20] ^= 1;
    try testing.expectError(error.Corrupt, MemberRow.decode(&torn));
    try testing.expectError(error.Corrupt, MemberRow.decode(bytes[0..80]));
    const id = writerMemberId(testIdentity(5));
    try testing.expect(memberIdentity(id).?.sameMember(testIdentity(5)));
    var reader: MemberId = @splat(0x33);
    try testing.expect(memberIdentity(reader) == null);
    reader[15] = 0;
    try testing.expect(memberIdentity(reader) == null);
    const c = try LeaseClocks.derive(45_000, 0, 100);
    try testing.expectEqual(@as(u64, 22), c.skew_max_ms);
    try testing.expectEqual(@as(u64, 200), c.d_purge_ms);
    try testing.expectEqual(@as(u64, 45_000 - 44 - 200), c.t_self_ms);
    try testing.expectEqual(@as(u64, 10_000), c.renew_ms);
    try testing.expectEqual(@as(u64, 45_000), c.grace_ms);
    try testing.expectError(error.UnsafeClocks, LeaseClocks.derive(1000, 400, 300));
    try testing.expectEqual(@as(u64, 45_000), tOwnerMs(null));
    try testing.expectEqual(@as(u64, 3000), tOwnerMs(3000));
}

test "the owner: arm refuses a stale term; a join writes ONE row and grants an epoch; renewals write nothing; eviction at >= T_owner ships the death with the member's epoch and key; the key is remembered 2 × T_owner; a leave is never a death" {
    const gpa = testing.allocator;
    const clocks = try LeaseClocks.derive(45_000, 0, 100);
    try testing.expectError(error.StaleTerm, Owner.arm(gpa, clocks, 0, 3, 3, &.{}, 0));
    var o = try Owner.arm(gpa, clocks, 0, 4, 3, &.{}, 1000);
    defer o.deinit();
    var deaths: Deaths = .{};
    defer deaths.list.deinit(gpa);
    var rows: Rows = .{};
    o.bindDeathSink(deaths.sink());
    o.bindRows(rows.store());
    const commits0 = stats.membership_registration_commits.load();
    const g = o.join(joinReqOf(1, null, "/mnt/a"), 1000).granted;
    try testing.expectEqual(@as(u64, 1), g.epoch);
    try testing.expectEqual(@as(u64, 0), g.ack_prior_epoch);
    try testing.expectEqual(@as(u32, 1), rows.puts);
    try testing.expectEqual((try MemberRow.decode(&rows.last)).epoch, 1);
    try testing.expectEqual(@as(u64, 4), (try MemberRow.decode(&rows.last)).term);
    const id1 = writerMemberId(testIdentity(1));
    // Three beats: no commits.
    var t: u64 = 1000;
    var beats: u32 = 0;
    while (beats < 3) : (beats += 1) {
        t += 10_000;
        try testing.expect(o.renew(id1, 1, 0, t) == .granted);
    }
    try testing.expectEqual(commits0 + 1, stats.membership_registration_commits.load());
    try testing.expect(o.renew(id1, 2, 0, t) == .unknown_lease);
    try testing.expectEqual(Liveness.live, o.liveness(id1, t));
    try testing.expectEqual(@as(?u64, 0x1001), o.registeredKey(id1));
    // Eviction at exactly the deadline (>=), never before.
    const ev0 = stats.membership_evictions.load();
    try testing.expectEqual(@as(u32, 0), o.expireDue(t + 44_999));
    try testing.expectEqual(@as(u32, 1), o.expireDue(t + 45_000));
    try testing.expectEqual(ev0 + 1, stats.membership_evictions.load());
    try testing.expectEqual(@as(usize, 1), deaths.list.items.len);
    try testing.expectEqual(@as(u64, 1), deaths.list.items[0].epoch);
    try testing.expectEqual(@as(u64, 0x1001), deaths.list.items[0].pr_key);
    try testing.expect(deaths.list.items[0].identity.?.sameMember(testIdentity(1)));
    try testing.expectEqual(@as(u32, 1), rows.deletes);
    try testing.expectEqual(Liveness.dead, o.liveness(id1, t + 45_000));
    try testing.expect(o.renew(id1, 1, 0, t + 45_001) == .unknown_lease);
    try testing.expect(o.holdsKey(0x1001));
    _ = o.expireDue(t + 45_000 + 2 * 45_000);
    try testing.expect(!o.holdsKey(0x1001));
    // A leave: departed, no death.
    _ = o.join(joinReqOf(2, null, "/mnt/b"), t);
    try testing.expect(o.leave(writerMemberId(testIdentity(2)), t));
    try testing.expect(!o.leave(writerMemberId(testIdentity(2)), t));
    try testing.expectEqual(@as(usize, 1), deaths.list.items.len);
    try testing.expectEqual(@as(u64, 0), stats.membership_members.load());
}

test "the reclaim is idempotent against the state held (a lost grant reply): with the grace closed, a reclaim naming the epoch granted re-points; one naming a STALE epoch from the SAME incarnation (pid, boot) re-points too — never UnknownLease, never a poison of a live member; the same id from ANOTHER incarnation with a stale epoch is UnknownLease; an epoch above the held one is UnknownLease; a dead-and-evicted member's reclaim is UnknownLease" {
    const gpa = testing.allocator;
    const clocks = try LeaseClocks.derive(45_000, 0, 100);
    var o = try Owner.arm(gpa, clocks, 0, 1, 0, &.{}, 0);
    defer o.deinit();
    const id = writerMemberId(testIdentity(1));
    const e1 = o.join(joinReqOf(1, null, "/mnt/a"), 0).granted.epoch;
    // The member reclaims naming e1 (a park past a renewal this owner
    // served): the re-point grants e2 and acks e1 — but the reply is lost.
    const g2 = o.join(joinReqOf(1, e1, "/mnt/a"), 100).granted;
    try testing.expect(g2.epoch > e1);
    try testing.expectEqual(e1, g2.ack_prior_epoch);
    try testing.expect(o.renew(id, e1, 0, 200) == .unknown_lease);
    // The member, still at e1, reclaims again: the same incarnation, a
    // stale epoch — re-pointed, never UnknownLease.
    const refusals0 = stats.membership_reclaim_refusals.load();
    const g3 = o.join(joinReqOf(1, e1, "/mnt/a"), 300).granted;
    try testing.expect(g3.epoch > g2.epoch);
    try testing.expectEqual(e1, g3.ack_prior_epoch);
    try testing.expect(o.renew(id, g3.epoch, 0, 400) == .granted);
    try testing.expectEqual(refusals0, stats.membership_reclaim_refusals.load());
    try testing.expectEqual(Liveness.live, o.liveness(id, 400));
    // An epoch this owner never granted (above the held one): unknown.
    try testing.expect(o.join(joinReqOf(1, g3.epoch + 7, "/mnt/a"), 500) == .unknown_lease);
    // Another incarnation under the same id (another pid) naming a stale
    // epoch: no reclaimer of this epoch.
    var other = joinReqOf(1, e1, "/mnt/a");
    other.pid = 999;
    try testing.expect(o.join(other, 600) == .unknown_lease);
    try testing.expectEqual(refusals0 + 2, stats.membership_reclaim_refusals.load());
    try testing.expectEqual(Liveness.live, o.liveness(id, 600));
    // Evicted at T_owner: the reclaim of a dead member is unknown.
    try testing.expectEqual(@as(u32, 1), o.expireDue(400 + 45_000));
    try testing.expect(o.join(joinReqOf(1, g3.epoch, "/mnt/a"), 400 + 45_001) == .unknown_lease);
}

test "grace, two halves on one deadline: a stranger is refused while the fresh half is open; an EXPECTED member's first join is a re-assertion; the fresh half closes early once every expected member re-asserted while the re-assertion half admits a prior epoch to the deadline; liveness reads unknown inside the window; the deadline ships the non-reclaimers with epoch 0 and the roster's key, once" {
    const gpa = testing.allocator;
    const clocks = try LeaseClocks.derive(45_000, 0, 100);
    const a = writerMemberId(testIdentity(1));
    const b = writerMemberId(testIdentity(2));
    var o = try Owner.arm(gpa, clocks, 0, 2, 1, &.{ a, b }, 10_000);
    defer o.deinit();
    var deaths: Deaths = .{};
    defer deaths.list.deinit(gpa);
    o.bindDeathSink(deaths.sink());
    try testing.expectEqual(@as(u64, 45_000), o.graceRemainingMs(10_000));
    // A stranger: refused naming the remaining window.
    const gr = o.join(joinReqOf(7, null, "/m7"), 11_000).grace_refused;
    try testing.expectEqual(@as(u64, 44_000), gr.retry_after_ms);
    try testing.expectEqual(Liveness.unknown, o.liveness(writerMemberId(testIdentity(7)), 11_000));
    // A's first join without an epoch is a re-assertion (expected).
    const reclaims0 = stats.membership_grace_reclaims.load();
    try testing.expect(o.join(joinReqOf(1, null, "/m1"), 12_000) == .granted);
    try testing.expectEqual(reclaims0 + 1, stats.membership_grace_reclaims.load());
    try testing.expect(o.join(joinReqOf(7, null, "/m7"), 13_000) == .grace_refused);
    // B re-asserts with its prior epoch: the fresh half closes early.
    const gb = o.join(joinReqOf(2, 9, "/m2"), 14_000).granted;
    try testing.expectEqual(@as(u64, 9), gb.ack_prior_epoch);
    try testing.expect(o.join(joinReqOf(7, null, "/m7"), 15_000) == .granted);
    // The re-assertion half still admits a prior epoch of an unknown member until the deadline.
    try testing.expect(o.join(joinReqOf(8, 3, "/m8"), 54_999) == .granted);
    try testing.expect(o.join(joinReqOf(9, 3, "/m9"), 55_000) == .unknown_lease);
    // Nobody died: both expected re-asserted.
    try testing.expectEqual(@as(u32, 0), o.reapGraceDeadline(55_000, &rosterKey));
    try testing.expectEqual(@as(usize, 0), deaths.list.items.len);
    // A second owner whose expected member never returns: dead at the deadline, once.
    var o2 = try Owner.arm(gpa, clocks, 1, 2, 1, &.{a}, 0);
    defer o2.deinit();
    o2.bindDeathSink(deaths.sink());
    try testing.expectEqual(@as(u32, 0), o2.reapGraceDeadline(44_999, &rosterKey));
    try testing.expectEqual(@as(u32, 1), o2.reapGraceDeadline(45_000, &rosterKey));
    try testing.expectEqual(@as(u32, 0), o2.reapGraceDeadline(46_000, &rosterKey));
    try testing.expectEqual(@as(u64, 0), deaths.list.items[0].epoch);
    try testing.expectEqual(@as(u64, 0x9999), deaths.list.items[0].pr_key);
    try testing.expectEqual(Liveness.dead, o2.liveness(a, 45_000));
}

test "a predecessor-roster member that re-asserted, then left or was evicted, joins FRESH under the same owner (a re-join in place, decision 0003 §BO): its join without an epoch is a re-assertion only while the window stands and it has not re-asserted; past the deadline, too, it is a fresh join, never UnknownLease" {
    const gpa = testing.allocator;
    const clocks = try LeaseClocks.derive(3_000, 0, 100);
    const a = writerMemberId(testIdentity(1));
    var o = try Owner.arm(gpa, clocks, 0, 2, 1, &.{a}, 0);
    defer o.deinit();
    // The re-assertion (expected, no epoch) inside the window.
    const g1 = o.join(joinReqOf(1, null, "/m1"), 100).granted;
    // Evicted at T_owner (silent), then its re-join in place: a fresh join.
    try testing.expectEqual(@as(u32, 1), o.expireDue(100 + clocks.t_owner_ms));
    try testing.expect(o.find(a) == null);
    const g2 = o.join(joinReqOf(1, null, "/m1"), 200 + clocks.t_owner_ms).granted;
    try testing.expect(g2.epoch > g1.epoch);
    try testing.expectEqual(@as(u64, 0), g2.ack_prior_epoch);
    // A clean leave inside the window, then a fresh join: granted too.
    _ = o.leave(a, 300 + clocks.t_owner_ms);
    try testing.expect(o.join(joinReqOf(1, null, "/m1"), 400 + clocks.t_owner_ms) == .granted);
    // An expected member that never re-asserted joins past the deadline: fresh.
    const b = writerMemberId(testIdentity(2));
    var o2 = try Owner.arm(gpa, clocks, 1, 2, 1, &.{b}, 0);
    defer o2.deinit();
    try testing.expect(o2.join(joinReqOf(2, null, "/m2"), clocks.grace_ms + 1) == .granted);
}

test "OQ-5: the same (node, mount_slot) LIVE at another mount point refuses naming the other; the same mount point replaces the prior state; the census pages in join order with a cursor; the push carriage rides the next grant once" {
    const gpa = testing.allocator;
    const clocks = try LeaseClocks.derive(45_000, 0, 100);
    var o = try Owner.arm(gpa, clocks, 0, 1, 0, &.{}, 0);
    defer o.deinit();
    _ = o.join(joinReqOf(1, null, "/mnt/one"), 0).granted;
    const c = o.join(joinReqOf(1, null, "/mnt/two"), 1000).mount_slot_collision;
    try testing.expectEqualStrings("/mnt/one", c.other[0..c.other_len]);
    try testing.expect(o.join(joinReqOf(1, null, "/mnt/one"), 1000) == .granted);
    var n: u32 = 2;
    while (n <= 5) : (n += 1) _ = o.join(joinReqOf(n, null, "/m"), 1000).granted;
    var scratch: [8192]u8 = undefined;
    var b: codec.Builder = .init(&scratch);
    const h = try o.census(0, 2, 2000, &b);
    try testing.expectEqual(@as(u16, 2), h.n);
    try testing.expectEqual(@as(u8, 1), h.has_next);
    const page: verbs.CensusPage = .{ .head = h, .rows_bytes = b.written() };
    var it = page.rows();
    // Identity 1's re-join at its own mount point replaced its state (a
    // later join_seq than the first), so it heads the page.
    const first = it.next().?;
    try testing.expectEqualSlices(u8, &writerMemberId(testIdentity(1)), &first.head.id);
    try testing.expectEqualStrings("/mnt/one", first.mount);
    var b2: codec.Builder = .init(&scratch);
    const h2 = try o.census(h.next, 100, 2000, &b2);
    try testing.expectEqual(@as(u16, 3), h2.n);
    try testing.expectEqual(@as(u8, 0), h2.has_next);
    // The carriage.
    const id3 = writerMemberId(testIdentity(3));
    try o.pushOffer(id3, 40, 1);
    try o.pushOffer(id3, 40, 1);
    try o.pushReleaseNotice(id3, 41);
    var offered: std.ArrayList(verbs.OfferedSlotWire) = .empty;
    defer offered.deinit(gpa);
    var notices: std.ArrayList(u16) = .empty;
    defer notices.deinit(gpa);
    try o.takeCarriage(id3, &offered, &notices);
    try testing.expectEqual(@as(usize, 1), offered.items.len);
    try testing.expectEqual(@as(u16, 40), offered.items[0].slot);
    try testing.expectEqual(@as(usize, 1), notices.items.len);
    offered.clearRetainingCapacity();
    try o.takeCarriage(id3, &offered, &notices);
    try testing.expectEqual(@as(usize, 0), offered.items.len);
}

/// An in-process link straight onto the owner's service (no hub): the
/// member's tick exercised against the owner's RAM authority.
const DirectLink = struct {
    svc: service.Service = .init,
    owner_svc: *OwnerService,
    reply: [8192]u8 = undefined,
    scratch: [8192]u8 = undefined,
    frame_buf: [8192]u8 = undefined,
    peer: ?WireIdentity,
    /// Seam: the venue is dead (every call fails as a transport error).
    dead: bool = false,

    fn call(ctx: *anyopaque, req: verbs.Request) errors.Error!verbs.Reply {
        const self: *DirectLink = @ptrCast(@alignCast(ctx));
        if (self.dead) return error.HolderUnreachable;
        var b: codec.Builder = .init(&self.frame_buf);
        verbs.encodeRequest(req, &b) catch return error.ValueTooLarge;
        const verb: verbs.Verb = @as(verbs.Verb, req);
        const header: @import("../../../wire/frame.zig").Header = .{ .flags = 1, .len = 0, .verb = @backingInt(verb), .status = 0, .correlation = 1, .seq = 1, .schema = verb.schema() };
        const served = self.svc.serve(header, b.written(), self.peer, 0, &self.reply, &self.scratch);
        return service.interpretReply(verb, served.status, served.payload);
    }

    fn link(self: *DirectLink) Link {
        return .{ .ctx = self, .call = &call };
    }
};

var manual_now_ms: u64 = 0;
fn manualNow(_: ?*anyopaque) u64 {
    return manual_now_ms;
}

const Poison = struct {
    count: u32 = 0,
    fn poison(ctx: ?*anyopaque, _: []const u8) void {
        const self: *Poison = @ptrCast(@alignCast(ctx.?));
        self.count += 1;
    }
    fn sink(self: *Poison) park.PoisonSink {
        return .{ .ctx = self, .poison = &poison };
    }
};

const Carriage = struct {
    offered: u32 = 0,
    slot: u16 = 0,
    fn off(ctx: ?*anyopaque, slot: u16, _: u32) void {
        const self: *Carriage = @ptrCast(@alignCast(ctx.?));
        self.offered += 1;
        self.slot = slot;
    }
    fn rel(_: ?*anyopaque, _: u16) void {}
    fn sink(self: *Carriage) CarriageSink {
        return .{ .ctx = self, .offered = &off, .released = &rel };
    }
};

var manual_generation: u64 = 0;
fn manualGeneration(_: ?*anyopaque) u64 {
    return manual_generation;
}

test "the member over the owner's wire face: join learns the clocks anchored on the send instant; the tick renews at the cadence and never between beats; the carriage's offer lands at the member and the owner's set-config generation rides every grant; a dead venue past T_self PARKS a symmetric appender (never the poison) and the reclaim beat rejoins once the successor admits it; a remote-custody lease poisons" {
    const gpa = testing.allocator;
    const clocks = try LeaseClocks.derive(45_000, 0, 100);
    var o = try Owner.arm(gpa, clocks, 0, 1, 0, &.{}, 0);
    defer o.deinit();
    manual_now_ms = 0;
    manual_generation = 3;
    var osvc: OwnerService = .{ .owner = &o, .now = &manualNow, .generation = .{ .ctx = null, .read = &manualGeneration } };
    const me = testIdentity(1);
    var dl: DirectLink = .{ .owner_svc = &osvc, .peer = me };
    osvc.register(&dl.svc);
    var gate = park.ParkGate.arm(testing.io, 0, park.tParkMaxMs(46_500, clocks.grace_ms));
    var p: Poison = .{};
    var carriage: Carriage = .{};
    var m: Member = .{ .gpa = gpa, .id = writerMemberId(me), .role = .writer, .link = dl.link(), .pid = 7, .boot = "boot-boot-boot-0".*, .endpoint = .none, .pr_key = 0x77, .mount = "/mnt/m", .gate = &gate, .poison = p.sink(), .carriage = carriage.sink() };
    try testing.expectEqual(Member.JoinOutcome.granted, m.join(100, null));
    try testing.expectEqual(@as(u64, 1), m.epoch);
    try testing.expectEqual(@as(u64, 3), m.config_generation);
    try testing.expectEqual(@as(u64, 100 + clocks.t_self_ms), m.words.learnedLabel().deadline_ms);
    try testing.expectEqual(@as(u64, 10_100), m.words.renew_at_ms.load(.acquire));
    // Between beats: idle; at the beat: renewed; the offer rides it, and
    // so does the generation the owner's word moved to (an online add).
    try testing.expectEqual(TickVerdict.idle, m.tick(5_000, 0));
    try o.pushOffer(m.id, 12, 0);
    manual_now_ms = 10_100;
    manual_generation = 4;
    const renewals0 = stats.membership_renewals.load();
    try testing.expectEqual(TickVerdict.renewed, m.tick(10_100, 0));
    try testing.expectEqual(renewals0 + 1, stats.membership_renewals.load());
    try testing.expectEqual(@as(u32, 1), carriage.offered);
    try testing.expectEqual(@as(u16, 12), carriage.slot);
    try testing.expectEqual(@as(u64, 4), m.config_generation);
    try testing.expectEqual(@as(u64, 10_100 + clocks.t_self_ms), m.words.learnedLabel().deadline_ms);
    // The venue dies: beats fail quietly until T_self, then the PARK.
    dl.dead = true;
    try testing.expectEqual(TickVerdict.idle, m.tick(20_100, 0));
    const t_self_at = 10_100 + clocks.t_self_ms;
    try testing.expectEqual(TickVerdict.idle, m.tick(t_self_at - 1, 0));
    const parks0 = park.stats.appender_parks.load();
    try testing.expectEqual(TickVerdict.parked, m.tick(t_self_at, 1_000));
    try testing.expectEqual(parks0 + 1, park.stats.appender_parks.load());
    try testing.expectEqual(@as(u32, 0), p.count);
    try testing.expect(gate.isParked());
    // Parked beats against a dead venue keep the park.
    try testing.expectEqual(TickVerdict.parked, m.tick(t_self_at + 10_000, 2_000));
    // The successor arms at term 2 with this member expected; the reclaim lands.
    var succ = try Owner.arm(gpa, clocks, 0, 2, 1, &.{m.id}, t_self_at + 20_000);
    defer succ.deinit();
    manual_now_ms = t_self_at + 20_000;
    var ssvc: OwnerService = .{ .owner = &succ, .now = &manualNow };
    var sdl: DirectLink = .{ .owner_svc = &ssvc, .peer = me };
    ssvc.register(&sdl.svc);
    m.link = sdl.link();
    const reclaims0 = park.stats.slot_lease_reclaims.load();
    try testing.expectEqual(TickVerdict.rejoined, m.tick(t_self_at + 20_000, 3_000));
    try testing.expectEqual(reclaims0 + 1, park.stats.slot_lease_reclaims.load());
    try testing.expect(!gate.isParked());
    try testing.expectEqual(Liveness.live, succ.liveness(m.id, t_self_at + 20_000));
    try testing.expect(succ.grace.?.fresh_closed);
    try m.leave();
    // A remote-custody lease at T_self: the poison, terminal.
    var r: Member = .{ .gpa = gpa, .id = @splat(0x44), .role = .reader, .link = sdl.link(), .pid = 8, .boot = "boot-boot-boot-0".*, .endpoint = .none, .pr_key = 0, .mount = "/mnt/r", .poison = p.sink() };
    try testing.expectEqual(Member.JoinOutcome.granted, r.join(0, null));
    try testing.expectEqual(TickVerdict.fenced, r.tick(clocks.t_self_ms, 0));
    try testing.expectEqual(@as(u32, 1), p.count);
    try testing.expectEqual(TickVerdict.fenced, r.tick(clocks.t_self_ms + 1, 0));
    try testing.expectEqual(@as(u32, 1), p.count);
}

test "UnknownLease before T_self fences terminally and never parks; a leave over a standing park is refused" {
    const gpa = testing.allocator;
    const clocks = try LeaseClocks.derive(45_000, 0, 100);
    var o = try Owner.arm(gpa, clocks, 0, 1, 0, &.{}, 0);
    defer o.deinit();
    manual_now_ms = 0;
    var osvc: OwnerService = .{ .owner = &o, .now = &manualNow };
    const me = testIdentity(2);
    var dl: DirectLink = .{ .owner_svc = &osvc, .peer = me };
    osvc.register(&dl.svc);
    var gate = park.ParkGate.arm(testing.io, 0, 1 << 40);
    var p: Poison = .{};
    var m: Member = .{ .gpa = gpa, .id = writerMemberId(me), .role = .writer, .link = dl.link(), .pid = 7, .boot = "boot-boot-boot-0".*, .endpoint = .none, .pr_key = 0, .mount = "/mnt/m", .gate = &gate, .poison = p.sink() };
    try testing.expectEqual(Member.JoinOutcome.granted, m.join(0, null));
    // The owner forgets the lease (a leave from elsewhere): the beat answers UnknownLease.
    _ = o.leave(m.id, 0);
    manual_now_ms = 10_000;
    try testing.expectEqual(TickVerdict.fenced, m.tick(10_000, 0));
    try testing.expect(!gate.isParked());
    try testing.expectEqual(@as(u32, 1), p.count);
    // A parked member's leave is refused set-wide.
    var m2: Member = .{ .gpa = gpa, .id = writerMemberId(testIdentity(3)), .role = .writer, .link = dl.link(), .pid = 7, .boot = "boot-boot-boot-0".*, .endpoint = .none, .pr_key = 0, .mount = "/mnt/n", .gate = &gate, .poison = p.sink() };
    _ = gate.park(1);
    try testing.expectError(error.ParkedLeaveRefused, m2.leave());
}

test "THE RE-JOIN IN PLACE's verdict (decision 0003 §BO): a member armed to re-join answers the owner's UnknownLease `evicted` — at a renewal before T_self and at the reclaim under the park — touching neither the gate nor the words nor the poison; an expired park stays the terminal fence; unarmed, the same answers fence and poison as before" {
    const gpa = testing.allocator;
    const clocks = try LeaseClocks.derive(45_000, 0, 100);
    var o = try Owner.arm(gpa, clocks, 0, 1, 0, &.{}, 0);
    defer o.deinit();
    manual_now_ms = 0;
    var osvc: OwnerService = .{ .owner = &o, .now = &manualNow };
    var dl: DirectLink = .{ .owner_svc = &osvc, .peer = testIdentity(5) };
    osvc.register(&dl.svc);
    var p: Poison = .{};
    // At a renewal (the gate running).
    var gate = park.ParkGate.arm(testing.io, 0, 1 << 40);
    var m: Member = .{ .gpa = gpa, .id = writerMemberId(testIdentity(5)), .role = .writer, .link = dl.link(), .pid = 7, .boot = "boot-boot-boot-0".*, .endpoint = .none, .pr_key = 0, .mount = "/mnt/m", .gate = &gate, .poison = p.sink(), .rejoin_armed = true };
    try testing.expectEqual(Member.JoinOutcome.granted, m.join(0, null));
    _ = o.leave(m.id, 0);
    manual_now_ms = 10_000;
    try testing.expectEqual(TickVerdict.evicted, m.tick(10_000, 0));
    try testing.expect(gate.word() == .running and !m.words.fenced.load(.acquire));
    try testing.expectEqual(@as(u32, 0), p.count);
    // At the reclaim under the park.
    var gate2 = park.ParkGate.arm(testing.io, 0, 1 << 40);
    var dl2: DirectLink = .{ .owner_svc = &osvc, .peer = testIdentity(6) };
    osvc.register(&dl2.svc);
    var m2: Member = .{ .gpa = gpa, .id = writerMemberId(testIdentity(6)), .role = .writer, .link = dl2.link(), .pid = 7, .boot = "boot-boot-boot-0".*, .endpoint = .none, .pr_key = 0, .mount = "/mnt/n", .gate = &gate2, .poison = p.sink(), .rejoin_armed = true };
    manual_now_ms = 10_000;
    try testing.expectEqual(Member.JoinOutcome.granted, m2.join(10_000, null));
    _ = o.leave(m2.id, 10_000);
    _ = gate2.park(1);
    try testing.expectEqual(TickVerdict.evicted, m2.tick(10_001, 2));
    try testing.expect(gate2.isParked() and !m2.words.fenced.load(.acquire));
    try testing.expectEqual(@as(u32, 0), p.count);
    // A park past T_park_max is the lattice's terminal verdict, armed or not.
    var gate3 = park.ParkGate.arm(testing.io, 0, 100);
    var dl3: DirectLink = .{ .owner_svc = &osvc, .peer = testIdentity(7) };
    osvc.register(&dl3.svc);
    var m3: Member = .{ .gpa = gpa, .id = writerMemberId(testIdentity(7)), .role = .writer, .link = dl3.link(), .pid = 7, .boot = "boot-boot-boot-0".*, .endpoint = .none, .pr_key = 0, .mount = "/mnt/o", .gate = &gate3, .poison = p.sink(), .rejoin_armed = true };
    try testing.expectEqual(Member.JoinOutcome.granted, m3.join(10_000, null));
    _ = o.leave(m3.id, 10_000);
    _ = gate3.park(1);
    park.stats.appender_park_expiries.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(TickVerdict.fenced, m3.tick(10_002, 1 + 200 * std.time.ns_per_ms));
    try log.testing.expectAllFired();
    try testing.expect(gate3.isExpired());
    try testing.expectEqual(@as(u32, 1), p.count);
    park.stats.appender_parked.set(@backingInt(park.Word.running));
}

test "T_self is the member's clock, never its beat's liveness: with the beat stalled, a committer at the pre-admission gate past the deadline PARKS the gate itself (one park, one self-fence, acks held), the committer waits, the beat's next tick runs the reclaim beat and the re-point releases it; before the first grant nothing fences; a renewal in time fences nothing" {
    const gpa = testing.allocator;
    const clocks = try LeaseClocks.derive(45_000, 0, 100);
    var o = try Owner.arm(gpa, clocks, 0, 1, 0, &.{}, 0);
    defer o.deinit();
    manual_now_ms = 0;
    var osvc: OwnerService = .{ .owner = &o, .now = &manualNow };
    const me = testIdentity(4);
    var dl: DirectLink = .{ .owner_svc = &osvc, .peer = me };
    osvc.register(&dl.svc);
    var gate = park.ParkGate.arm(testing.io, 0, 1 << 40);
    var p: Poison = .{};
    var m: Member = .{ .gpa = gpa, .id = writerMemberId(me), .role = .writer, .link = dl.link(), .pid = 7, .boot = "boot-boot-boot-0".*, .endpoint = .none, .pr_key = 0, .mount = "/mnt/m", .gate = &gate, .poison = p.sink() };
    gate.bindSelfFence(m.selfFenceSeam());
    // Before any grant there is no deadline: the gate admits.
    try gate.preAdmission();
    try testing.expect(!gate.isParked());
    // The join anchors the deadline at the send instant. The committers'
    // judgement uses the REAL monotonic clock, so the member's clock is
    // anchored there too: the deadline lies T_self past now.
    const t0 = clock.monotonicNs() / std.time.ns_per_ms;
    try testing.expectEqual(Member.JoinOutcome.granted, m.join(t0, null));
    try gate.preAdmission();
    try testing.expect(!gate.isParked());
    // The beat stalls (no tick). A committer arrives past the deadline:
    // the words say due at `now ≥ deadline` — moved by a renewal anchored
    // in the past, the owner's clock answering it.
    manual_now_ms = 100;
    m.words.renew_at_ms.store(0, .release);
    try testing.expectEqual(TickVerdict.renewed, m.tick(t0 -| clocks.t_self_ms -| 1, 0));
    try testing.expect(m.words.selfFenceDue(t0));
    const fences0 = stats.membership_self_fences.load();
    const parks0 = park.stats.appender_parks.load();
    var c: park.TestCommitter = .{ .gate = &gate };
    const th = try std.Thread.spawn(.{}, park.TestCommitter.run, .{&c});
    var spins: u32 = 0;
    while (gate.admission_wake.waiters(testing.io) == 0 and spins < 10_000_000) : (spins += 1) std.Thread.yield() catch {};
    try testing.expect(gate.isParked());
    try testing.expectEqual(parks0 + 1, park.stats.appender_parks.load());
    try testing.expectEqual(fences0 + 1, stats.membership_self_fences.load());
    try testing.expectEqual(@as(u32, 0), p.count);
    // The beat resumes: the gate is parked, so its tick is the reclaim
    // beat — the owner still holds the lease (a re-point), the gate
    // releases, the committer lands. One self-fence in all.
    manual_now_ms = 200;
    try testing.expectEqual(TickVerdict.rejoined, m.tick(clock.monotonicNs() / std.time.ns_per_ms, 0));
    th.join();
    try testing.expect(c.entered);
    try testing.expectEqual(park.AckVerdict.ok, c.verdict);
    try testing.expect(!gate.isParked());
    try testing.expectEqual(fences0 + 1, stats.membership_self_fences.load());
    try testing.expectEqual(@as(u32, 0), p.count);
    try gate.preAdmission();
}

test "THE DOUBLE PARK (the kill matrix's, 2026-10-24) at the membership core: the manager dies ⇒ PARK #1 at T_self ⇒ the successor admits the reclaim in grace and the deadline is the reclaim's (its send instant + T_self, never a stale one); renewals carry the member past the grace window; the beat silent past T_self but short of T_owner ⇒ PARK #2, and its reclaim RE-POINTS against the epoch the successor holds (granted, no refusal, no poison: idempotent against the manager's view); the beat silent past T_owner is a REAL eviction (the successor evicts at T_owner and ships the death) ⇒ PARK #3, and the reclaim's UnknownLease is HONEST: the member fences, poisoned once" {
    const gpa = testing.allocator;
    const clocks = try LeaseClocks.derive(45_000, 0, 100);
    var o = try Owner.arm(gpa, clocks, 0, 1, 0, &.{}, 0);
    defer o.deinit();
    manual_now_ms = 0;
    var osvc: OwnerService = .{ .owner = &o, .now = &manualNow };
    const me = testIdentity(5);
    var dl: DirectLink = .{ .owner_svc = &osvc, .peer = me };
    osvc.register(&dl.svc);
    var gate = park.ParkGate.arm(testing.io, 0, park.tParkMaxMs(46_500, clocks.grace_ms));
    var p: Poison = .{};
    var m: Member = .{ .gpa = gpa, .id = writerMemberId(me), .role = .writer, .link = dl.link(), .pid = 7, .boot = "boot-boot-boot-0".*, .endpoint = .none, .pr_key = 0, .mount = "/mnt/m", .gate = &gate, .poison = p.sink() };
    try testing.expectEqual(Member.JoinOutcome.granted, m.join(0, null));
    // PARK #1: the manager dies; T_self passes with no renewal.
    dl.dead = true;
    const parks0 = park.stats.appender_parks.load();
    try testing.expectEqual(TickVerdict.parked, m.tick(clocks.t_self_ms, 1_000));
    // The successor arms at term 2 expecting the member; the reclaim lands
    // and re-arms the deadline at its own send instant.
    const t1 = clocks.t_self_ms + 5_000;
    var succ = try Owner.arm(gpa, clocks, 0, 2, 1, &.{m.id}, t1);
    defer succ.deinit();
    var deaths: Deaths = .{};
    defer deaths.list.deinit(gpa);
    succ.bindDeathSink(deaths.sink());
    var ssvc: OwnerService = .{ .owner = &succ, .now = &manualNow };
    var sdl: DirectLink = .{ .owner_svc = &ssvc, .peer = me };
    ssvc.register(&sdl.svc);
    m.link = sdl.link();
    manual_now_ms = t1;
    try testing.expectEqual(TickVerdict.rejoined, m.tick(t1, 2_000));
    try testing.expectEqual(t1 + clocks.t_self_ms, m.words.learnedLabel().deadline_ms);
    // Renewals at the cadence carry the member past the grace window: a
    // reclaim from here is admitted only on the epoch the successor holds.
    var t = t1;
    while (t <= t1 + clocks.grace_ms) {
        t += clocks.renew_ms;
        manual_now_ms = t;
        try testing.expectEqual(TickVerdict.renewed, m.tick(t, 2_000));
    }
    try testing.expect(!succ.graceOpenForReassert(t));
    const e_held = m.epoch;
    // PARK #2: the beat silent past T_self, short of T_owner (the
    // successor still holds the lease): the member parks on its own clock.
    const t2 = t + clocks.t_self_ms;
    try testing.expect(t2 < t + clocks.t_owner_ms);
    try testing.expectEqual(TickVerdict.parked, m.tick(t2, 3_000));
    try testing.expectEqual(parks0 + 2, park.stats.appender_parks.load());
    manual_now_ms = t2 + 1;
    try testing.expectEqual(@as(u32, 0), succ.expireDue(t2 + 1));
    const refusals0 = stats.membership_reclaim_refusals.load();
    try testing.expectEqual(TickVerdict.rejoined, m.tick(t2 + 1, 4_000));
    try testing.expect(m.epoch > e_held);
    try testing.expectEqual(refusals0, stats.membership_reclaim_refusals.load());
    try testing.expectEqual(t2 + 1 + clocks.t_self_ms, m.words.learnedLabel().deadline_ms);
    try testing.expectEqual(@as(u32, 0), p.count);
    try testing.expectEqual(@as(usize, 0), deaths.list.items.len);
    // Silent past T_owner: the successor evicts the silent member and ships
    // its death — a REAL eviction, its region the successor's to recover.
    const t3 = t2 + 1 + clocks.t_owner_ms;
    manual_now_ms = t3;
    try testing.expectEqual(@as(u32, 1), succ.expireDue(t3));
    try testing.expectEqual(@as(usize, 1), deaths.list.items.len);
    try testing.expectEqual(m.epoch, deaths.list.items[0].epoch);
    // PARK #3 on the member's clock, then the reclaim: UnknownLease is the
    // owner's honest verdict — the member fences, poisoned once.
    try testing.expectEqual(TickVerdict.parked, m.tick(t3, 5_000));
    park.stats.appender_park_expiries.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(TickVerdict.fenced, m.tick(t3 + 1, 6_000));
    try log.testing.expectAllFired();
    try testing.expectEqual(refusals0 + 1, stats.membership_reclaim_refusals.load());
    try testing.expect(gate.isExpired());
    try testing.expectEqual(@as(u32, 1), p.count);
}

test "the clients seam lists every member of the shard: a writer through its row's classifier (dead-pid proof applies), a reader as a live row" {
    const gpa = testing.allocator;
    const clocks = try LeaseClocks.derive(45_000, 0, 100);
    var o = try Owner.arm(gpa, clocks, 0, 1, 0, &.{}, 0);
    defer o.deinit();
    _ = o.join(joinReqOf(1, null, "/m"), 0).granted;
    var rq = joinReqOf(2, null, "/r");
    rq.id = @splat(0x22);
    rq.role = .reader;
    rq.row = null;
    _ = o.join(rq, 0).granted;
    const local: claim.Identity = .{ .boot_id = "boot-boot-boot-0".*, .pid = 1, .start_ticks = 1, .hostname_hash = 1 };
    const probe: claim.PidProbe = .{ .ctx = null, .startTicksOf = struct {
        fn f(_: ?*anyopaque, _: u32) ?u64 {
            return null;
        }
    }.f };
    manual_now_ms = 5_000;
    var regs = o.registrationSource(&local, probe, &manualNow, null);
    var rows: std.ArrayList(claim.Registration) = .empty;
    defer rows.deinit(gpa);
    const src = regs.source();
    try src.append(src.ctx, gpa, &rows);
    try testing.expectEqual(@as(usize, 2), rows.items.len);
    for (rows.items) |r| try testing.expectEqual(claim.RegistrationKind.member, r.kind);
    // The writer's pid is provably dead on this host: the row reads dead.
    try testing.expectEqual(claim.RegistrationState.dead, rows.items[0].state);
    try testing.expectEqual(claim.RegistrationState.live, rows.items[1].state);
}

test "a successor's grace expects no same-host predecessor whose pid is proven dead (the `-o ro` row's remount refused once: a joiner's row outlived its clean leave, the manager's successor expected it, and the fresh half held every stranger to its deadline): a dead row is not expected, a live one, another host's and an undecodable one are; with only dead rows no grace opens and a reader's fresh join is granted" {
    const gpa = testing.allocator;
    const local: claim.Identity = .{ .boot_id = "boot-boot-boot-0".*, .pid = 1, .start_ticks = 1, .hostname_hash = 1 };
    // pid 102's process is alive (start ticks 7); every other pid is gone.
    const probe: claim.PidProbe = .{ .ctx = null, .startTicksOf = struct {
        fn f(_: ?*anyopaque, pid: u32) ?u64 {
            return if (pid == 102) 7 else null;
        }
    }.f };
    const dead = rowOf(1).encode();
    const live = rowOf(2).encode();
    var foreign_row = rowOf(3);
    foreign_row.boot_id = "boot-other-host!".*;
    const foreign = foreign_row.encode();
    try testing.expect(!MemberRow.expectsReassertion(&dead, local, probe));
    try testing.expect(MemberRow.expectsReassertion(&live, local, probe));
    try testing.expect(MemberRow.expectsReassertion(&foreign, local, probe));
    try testing.expect(MemberRow.expectsReassertion("torn", local, probe));
    // The roster the successor arms from: the dead row dropped.
    var roster: std.ArrayList(MemberId) = .empty;
    defer roster.deinit(gpa);
    for ([_]struct { n: u32, bytes: []const u8 }{.{ .n = 1, .bytes = &dead }}) |r| {
        if (MemberRow.expectsReassertion(r.bytes, local, probe)) try roster.append(gpa, writerMemberId(testIdentity(r.n)));
    }
    try testing.expectEqual(@as(usize, 0), roster.items.len);
    const clocks = try LeaseClocks.derive(45_000, 0, 100);
    var reader = joinReqOf(9, null, "/ro");
    reader.id = @splat(0x99);
    reader.role = .reader;
    reader.row = null;
    var succ = try Owner.arm(gpa, clocks, 0, 2, 1, roster.items, 10_000);
    defer succ.deinit();
    _ = succ.join(reader, 10_001).granted;
    // The roster that expected the dead row: the same reader is refused.
    var stale = try Owner.arm(gpa, clocks, 0, 2, 1, &.{writerMemberId(testIdentity(1))}, 10_000);
    defer stale.deinit();
    try testing.expect(stale.join(reader, 10_001) == .grace_refused);
}

fn memberRowFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [claim.CLAIM_LEN + 8]u8 = undefined;
    const len = smith.slice(&buf);
    _ = MemberRow.decode(buf[0..len]) catch {};
    if (len >= claim.CLAIM_LEN) {
        const img = buf[0..claim.CLAIM_LEN];
        checksum.stamp(img, @offsetOf(claim.WriterClaimV1, "csum_le"));
        if (MemberRow.decode(img)) |row| try std.testing.expectEqualSlices(u8, img, &row.encode()) else |_| {}
    }
}

test "fuzz: the member row decoder is total on arbitrary and restamped claims; a decoded row re-encodes byte-identically" {
    try fuzzkit.fuzz({}, memberRowFuzz, .{});
}
