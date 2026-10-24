//! PARK-AND-RECLAIM (charter §5.5 "A manager's death is its home shard's
//! outage"; `docs/porting/sym_wire_membership_recovery.md` §2.6): a
//! symmetric appender's `T_self` action is the PARK, never the poison — a
//! manager's death takes its home shard's renewal venue away for
//! `manager_failover_bound_ms` (≈ 46.5 s) while every member's `T_self`
//! (≈ 40–44 s) expires FIRST; the shipped self-fence poisoned 195 mounts
//! per routine manager death.
//!
//! The word: `RUNNING → PARKED → RUNNING` (a reclaim) or `PARKED → EXPIRED`
//! (sticky — the terminal poison, `appender_park_expiries` must-stay-0 on a
//! healthy failover). While parked: a DISTINCT pre-admission gate before
//! ring admission (register-recheck-await, never the D1.b escalation);
//! in-flight entries LAND with their acks HELD in journal order; reads,
//! token service, DMA to granted blocks and the checkpoint task continue;
//! the page stays `Live`, ring/leases/`g` stay in RAM. Every beat the parked
//! loop reclaims against the successor the home volume's rendezvous names
//! (`join{prior_epoch}`); `T_park_max = failover_bound + grace`.
//!
//! RETIRED (sticky, from RUNNING or PARKED) is the incarnation's end when
//! the home shard evicted a live member and it re-joins IN PLACE (charter
//! §5.5 "A live member evicted …"; decision 0003 §BS): the pre-admission
//! gate answers `Retired` — the conveyor's `Deferred`, nothing written, the
//! op re-issued on the successor incarnation — and a failed re-join moves
//! it on to EXPIRED (the poison).
//!
//! The invariants are `park_core_model.zig`'s: the Dekker pair (`tryEnter`
//! vs `park`, SeqCst on both sides), the exclusive exit (one CAS off
//! PARKED — release, expiry and retirement), the re-arm and the
//! retirement's drain. Every registry is a field of the gate (0002 row
//! 259).

const std = @import("std");
const Io = std.Io;
const counters = @import("../../../core/counters.zig");
const clock = @import("../../../core/clock.zig");
const log = @import("../../../core/log.zig");
const notify = @import("../../../sync/notify.zig");
const lease_clock = @import("../../guard/lease_clock.zig");

pub const model = @import("park_core_model.zig");

pub const stats = struct {
    /// The posture word: 0 running, 1 parked, 2 expired, 3 retired.
    pub var appender_parked: counters.Gauge = .{};
    pub var appender_parks: counters.ShardedCounter = .{};
    /// Exact-sum `wait_successor / reclaim_rtt / total` per reclaim.
    pub var appender_park_ns: [Phase.count]counters.ShardedLatencyHistogram = @splat(.{});
    /// A park that outlived `T_park_max`: the terminal poison (must-stay-0
    /// on a healthy failover).
    pub var appender_park_expiries: counters.Tripwire = .init("appender_park_expiries");
    pub var appender_park_acks_held: counters.Gauge = .{};
    pub var slot_lease_reclaims: counters.ShardedCounter = .{};
    /// A leave or shutdown refused over a standing park (set-wide).
    pub var appender_park_leave_refusals: counters.ShardedCounter = .{};
    /// `T_self` reached by a REMOTE-custody lease: the poison, by class.
    pub var remote_custody_poisons: counters.ShardedCounter = .{};
};

pub const Phase = enum(u8) {
    wait_successor,
    reclaim_rtt,
    total,

    pub const count = @typeInfo(Phase).@"enum".field_names.len;
};

pub const Word = enum(u32) { running = 0, parked = 1, expired = 2, retired = 3 };

/// `T_park_max = manager_failover_bound_ms + grace_ms`: the longest a live
/// successor can take to arm (the TTL the D0 ladder waits out + its ladder
/// + the ring replay) and then admit reclaims (its grace window).
pub fn tParkMaxMs(failover_bound_ms: u64, grace_ms: u64) u64 {
    return failover_bound_ms +| grace_ms;
}

/// The poison splits by class: a lease that is NOT reclaimable (S9 remote
/// custody) poisons at `T_self`; the symmetric appender's parks.
pub const FenceClass = enum { remote_custody, symmetric_appender };

/// The `data_custody::poison` sink the terminal arms call.
pub const PoisonSink = struct {
    ctx: ?*anyopaque,
    poison: *const fn (ctx: ?*anyopaque, reason: []const u8) void,
};

/// `T_self` judged by a COMMITTER at the pre-admission gate (charter §5.5
/// "Two lease clocks": the deadline is the member's clock, never its
/// beat's liveness). The beat judges it too, but the beat makes wire
/// calls — the census, the re-point's dial, the endpoint's re-publication,
/// a set-config learn — each bounded only by the dialer's reply bound, so
/// a stalled beat let a LIVE member commit past `T_owner` while its home
/// shard evicted it and recovered its region (the zombie writer). Bound by
/// the member (`shard.Member.selfFenceSeam`): parks the gate when due.
pub const SelfFence = struct {
    ctx: *anyopaque,
    fenceIfDue: *const fn (ctx: *anyopaque, now_ms: u64, now_mono_ns: u64) void,
};

pub const Pass = struct { gate: *ParkGate };

pub const AckVerdict = enum { ok, eio };

pub const ParkGate = struct {
    io: Io,
    state: std.atomic.Value(u32) = .init(@backingInt(Word.running)),
    inflight: std.atomic.Value(u32) = .init(0),
    parked_at_mono_ns: std.atomic.Value(u64) = .init(0),
    t_park_max_ms: u64,
    home_volume: u16,
    admission_wake: notify.Notify = .init,
    ack_wake: notify.Notify = .init,
    /// The `LEAVE_REFUSED` latch: a dismount over a standing park is the
    /// crash shape on every volume.
    leave_refused: std.atomic.Value(bool) = .init(false),
    acks_held: std.atomic.Value(u32) = .init(0),
    /// The member's `T_self` judgement for the committers (`bindSelfFence`
    /// once the first grant is learnt; null = the beat's alone).
    self_fence: ?SelfFence = null,

    /// `arm_symmetric_appender(home_volume, t_park_max)`.
    pub fn arm(io: Io, home_volume: u16, t_park_max_ms: u64) ParkGate {
        return .{ .io = io, .home_volume = home_volume, .t_park_max_ms = t_park_max_ms };
    }

    /// Bound by the joined half once the member learnt its first grant
    /// (before it there is no deadline to judge).
    pub fn bindSelfFence(self: *ParkGate, f: SelfFence) void {
        self.self_fence = f;
    }

    pub fn word(self: *const ParkGate) Word {
        return @fromBackingInt(self.state.load(.seq_cst));
    }

    pub fn isParked(self: *const ParkGate) bool {
        return self.word() == .parked;
    }

    pub fn isExpired(self: *const ParkGate) bool {
        return self.word() == .expired;
    }

    pub fn isRetired(self: *const ParkGate) bool {
        return self.word() == .retired;
    }

    /// A parked lessee is still the lock master: grants and recalls go on
    /// until the poison.
    pub fn admitsTokenService(self: *const ParkGate) bool {
        return !self.isExpired();
    }

    /// The barrier/`fsync` bounded-error timers are SUSPENDED while parked.
    pub fn boundedTimersSuspended(self: *const ParkGate) bool {
        return self.isParked();
    }

    /// The committer's half of the Dekker pair: `inflight += 1` (SeqCst)
    /// then the state load (SeqCst — the pair's fence); PARKED or EXPIRED
    /// withdraws. SeqCst pairs with `park`'s SeqCst store + load.
    pub fn tryEnter(self: *ParkGate) ?Pass {
        _ = self.inflight.fetchAdd(1, .seq_cst);
        if (self.word() == .running) return .{ .gate = self };
        _ = self.inflight.fetchSub(1, .seq_cst);
        return null;
    }

    /// The entry landed (or failed): its ack is no longer in flight.
    pub fn leave(self: *ParkGate, pass: Pass) void {
        std.debug.assert(pass.gate == self);
        _ = self.inflight.fetchSub(1, .seq_cst);
    }

    /// The pre-admission gate consulted BEFORE ring admission: while PARKED
    /// a committer registers, re-checks and awaits `admission_wake`; it
    /// never feeds the journal-failure escalation. EXPIRED fails it. A
    /// RUNNING gate judges `T_self` first (`SelfFence`): a committer past
    /// the member's deadline parks the gate itself and waits like the rest.
    /// RETIRED answers `Retired`: this incarnation admits nothing more.
    pub fn preAdmission(self: *ParkGate) error{ Expired, Retired }!void {
        while (true) {
            switch (self.word()) {
                .running => {
                    if (self.self_fence) |f| {
                        const now = clock.monotonicNs();
                        f.fenceIfDue(f.ctx, now / std.time.ns_per_ms, now);
                        if (self.word() != .running) continue;
                    }
                    return;
                },
                .expired => return error.Expired,
                .retired => return error.Retired,
                .parked => {},
            }
            var n = self.admission_wake.notified();
            if (n.enable(self.io)) {
                n.waitUncancelable(self.io);
                continue;
            }
            if (self.word() != .parked) {
                n.cancel(self.io);
                continue;
            }
            n.waitUncancelable(self.io);
        }
    }

    /// The parker's half: store PARKED (SeqCst), then load `inflight`
    /// (SeqCst) — what it saw in flight IS the acks the hold keeps. Answers
    /// the in-flight count when THIS call parked; null when the gate was
    /// already parked or expired (a second park is idempotent — the CAS is
    /// the one latch between the beat and the committers judging `T_self`).
    pub fn park(self: *ParkGate, now_mono_ns: u64) ?u32 {
        if (self.state.cmpxchgStrong(@backingInt(Word.running), @backingInt(Word.parked), .seq_cst, .seq_cst) != null) return null;
        self.parked_at_mono_ns.store(now_mono_ns, .release);
        const inflight = self.inflight.load(.seq_cst);
        stats.appender_parks.inc();
        stats.appender_parked.set(@backingInt(Word.parked));
        log.warn("appender park: T_self reached with the home volume {d}'s manager unreachable — parked with {d} commit(s) in flight (acks held), reclaiming against the successor for up to {d} ms", .{ self.home_volume, inflight, self.t_park_max_ms });
        return inflight;
    }

    /// `hold_acks`: an entry that landed under a park holds its ack on the
    /// durability lane until the gate releases (`ok`) or expires (`eio`).
    /// A running gate answers at once.
    pub fn holdAck(self: *ParkGate) AckVerdict {
        _ = self.acks_held.fetchAdd(1, .acq_rel);
        stats.appender_park_acks_held.add(1);
        defer {
            _ = self.acks_held.fetchSub(1, .acq_rel);
            stats.appender_park_acks_held.sub(1);
        }
        while (true) {
            switch (self.word()) {
                .running => return .ok,
                .expired, .retired => return .eio,
                .parked => {},
            }
            var n = self.ack_wake.notified();
            if (n.enable(self.io)) {
                n.waitUncancelable(self.io);
                continue;
            }
            if (self.word() != .parked) {
                n.cancel(self.io);
                continue;
            }
            n.waitUncancelable(self.io);
        }
    }

    fn wakeAll(self: *ParkGate) void {
        self.admission_wake.notifyWaiters(self.io);
        self.ack_wake.notifyWaiters(self.io);
    }

    /// The reclaim landed: ONE CAS `PARKED → RUNNING` (exclusive with
    /// `expire*`), every parker woken, the phases recorded exact-sum.
    pub fn release(self: *ParkGate, wait_successor_ns: u64, reclaim_rtt_ns: u64) bool {
        if (self.state.cmpxchgStrong(@backingInt(Word.parked), @backingInt(Word.running), .seq_cst, .seq_cst) != null) return false;
        stats.appender_parked.set(@backingInt(Word.running));
        stats.slot_lease_reclaims.inc();
        stats.appender_park_ns[@backingInt(Phase.wait_successor)].record(wait_successor_ns);
        stats.appender_park_ns[@backingInt(Phase.reclaim_rtt)].record(reclaim_rtt_ns);
        stats.appender_park_ns[@backingInt(Phase.total)].record(wait_successor_ns + reclaim_rtt_ns);
        self.wakeAll();
        return true;
    }

    /// Past `T_park_max`: ONE CAS `PARKED → EXPIRED`, the poison, every
    /// parker woken (their acks fail `EIO`), the terminal tripwire.
    pub fn expireIfDue(self: *ParkGate, now_mono_ns: u64, sink: PoisonSink) bool {
        if (!self.isParked()) return false;
        const age_ms = (now_mono_ns -| self.parked_at_mono_ns.load(.acquire)) / std.time.ns_per_ms;
        if (age_ms < self.t_park_max_ms) return false;
        return self.expireNow(sink, "the park outlived T_park_max: no successor admitted the reclaim");
    }

    /// The owner's own verdict says the lease is gone (`UnknownLease`): a
    /// park would stand for nothing. A RETIRED gate whose re-join in place
    /// was refused (something un-landed, or the join failed) expires too.
    pub fn expireNow(self: *ParkGate, sink: PoisonSink, reason: []const u8) bool {
        if (self.state.cmpxchgStrong(@backingInt(Word.parked), @backingInt(Word.expired), .seq_cst, .seq_cst) != null and
            self.state.cmpxchgStrong(@backingInt(Word.retired), @backingInt(Word.expired), .seq_cst, .seq_cst) != null) return false;
        stats.appender_parked.set(@backingInt(Word.expired));
        stats.appender_park_expiries.fire(reason);
        sink.poison(sink.ctx, reason);
        self.wakeAll();
        return true;
    }

    /// The home shard evicted this live member: ONE CAS off RUNNING or
    /// PARKED to RETIRED (exclusive with `release` and `expire*`), every
    /// parker woken — a parked pass answers its batch `Retired` (the
    /// conveyor's `Deferred`: nothing written) and later passes refuse at
    /// once. True iff THIS call retired the word. The caller then drains
    /// the incarnation (`park_core_model.RetireModel`: a pass that read
    /// RUNNING is inside the growth gate until its window is tracked).
    pub fn retire(self: *ParkGate) bool {
        while (true) {
            const w = self.word();
            if (w != .running and w != .parked) return false;
            if (self.state.cmpxchgStrong(@backingInt(w), @backingInt(Word.retired), .seq_cst, .seq_cst) == null) break;
        }
        stats.appender_parked.set(@backingInt(Word.retired));
        self.wakeAll();
        return true;
    }

    /// A leave or shutdown over a standing park — or past the poison — is
    /// REFUSED set-wide (the latch stays: a parked or FENCED appender's
    /// dismount is the crash shape; a fenced one's region is already the
    /// successor's, recovered from the ring and the page as it left them,
    /// so a release written from here would land on a recovered region).
    pub fn leaveAllowed(self: *ParkGate) bool {
        if (self.word() == .running) return true;
        self.leave_refused.store(true, .release);
        stats.appender_park_leave_refusals.inc();
        return false;
    }

    pub fn parkedAgeNs(self: *const ParkGate, now_mono_ns: u64) u64 {
        return now_mono_ns -| self.parked_at_mono_ns.load(.acquire);
    }
};

/// `fence_at_t_self`: the ONE place the class splits — remote custody
/// poisons, the symmetric appender parks. True iff THIS call acted (the
/// park landed here; a poison always does).
pub fn fenceAtTSelf(class: FenceClass, gate: ?*ParkGate, sink: PoisonSink, now_mono_ns: u64) bool {
    switch (class) {
        .remote_custody => {
            stats.remote_custody_poisons.inc();
            sink.poison(sink.ctx, "T_self reached on a remote-custody lease");
            return true;
        },
        .symmetric_appender => return gate.?.park(now_mono_ns) != null,
    }
}

/// What the home shard answered a parked member's `join{prior_epoch}`.
pub const ReclaimAnswer = enum {
    /// The successor admitted the reclaim in its grace window.
    granted,
    /// `UnknownLease`: the owner's own verdict — the lease is gone.
    unknown_lease,
    /// A refusal or a transport failure: the park stands.
    not_now,
};

pub const ReclaimVerdict = enum { rejoined, fenced, parked };

/// `reclaim_under_park` (M:3766-3808): the parked loop's beat. Expiry is
/// judged FIRST; then the reclaim's answer moves the word.
pub fn reclaimUnderPark(gate: *ParkGate, answer: ReclaimAnswer, now_mono_ns: u64, reclaim_rtt_ns: u64, sink: PoisonSink) ReclaimVerdict {
    if (gate.expireIfDue(now_mono_ns, sink)) return .fenced;
    if (gate.isExpired()) return .fenced;
    switch (answer) {
        .granted => {
            const waited = gate.parkedAgeNs(now_mono_ns) -| reclaim_rtt_ns;
            return if (gate.release(waited, reclaim_rtt_ns)) .rejoined else .fenced;
        },
        .unknown_lease => {
            _ = gate.expireNow(sink, "the successor answered UnknownLease: the prior lease is not custody");
            return .fenced;
        },
        .not_now => return .parked,
    }
}

/// `grace = T_owner`: a full lease period to re-assert.
pub fn graceMs(t_owner_ms: u64) u64 {
    return t_owner_ms;
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test {
    _ = model;
}

const Poison = struct {
    count: u32 = 0,
    fn poison(ctx: ?*anyopaque, _: []const u8) void {
        const self: *Poison = @ptrCast(@alignCast(ctx.?));
        self.count += 1;
    }
    fn sink(self: *Poison) PoisonSink {
        return .{ .ctx = self, .poison = &poison };
    }
};

test "ties: T_park_max = failover_bound + grace (≈ 91.5 s at the design's row); grace = T_owner" {
    try testing.expectEqual(@as(u64, 46_500 + 45_000), tParkMaxMs(46_500, graceMs(lease_clock.T_OWNER_MS)));
    try testing.expectEqual(lease_clock.T_OWNER_MS, graceMs(lease_clock.T_OWNER_MS));
}

test "the seven steps: a running gate admits; the park holds the in-flight count; a committer withdraws while parked; the reclaim releases exactly once with the phases recorded; a leave over a park is refused; the class split poisons remote custody only" {
    var g = ParkGate.arm(testing.io, 3, 1000);
    var p: Poison = .{};
    const pass = g.tryEnter().?;
    try testing.expectEqual(@as(u32, 1), g.inflight.load(.seq_cst));
    const parks0 = stats.appender_parks.load();
    // Step 1: T_self on the symmetric class parks (the remote class poisons).
    try testing.expect(fenceAtTSelf(.remote_custody, &g, p.sink(), 10));
    try testing.expectEqual(@as(u32, 1), p.count);
    try testing.expect(!g.isParked());
    try testing.expect(fenceAtTSelf(.symmetric_appender, &g, p.sink(), 10));
    try testing.expect(g.isParked());
    // A second park is idempotent: not this call's.
    try testing.expect(!fenceAtTSelf(.symmetric_appender, &g, p.sink(), 11));
    try testing.expectEqual(@as(?u32, null), g.park(12));
    try testing.expectEqual(parks0 + 1, stats.appender_parks.load());
    try testing.expectEqual(@as(u64, 1), stats.appender_parked.load());
    // Step 2: a new committer withdraws; the in-flight one lands (its ack held).
    try testing.expect(g.tryEnter() == null);
    try testing.expectEqual(@as(u32, 1), g.inflight.load(.seq_cst));
    try testing.expect(g.admitsTokenService() and g.boundedTimersSuspended());
    g.leave(pass);
    // Step 6: the leave is refused set-wide and latched.
    try testing.expect(!g.leaveAllowed());
    try testing.expect(g.leave_refused.load(.acquire));
    // Step 3/4: a refused beat keeps the park; the granted beat rejoins ONCE.
    try testing.expectEqual(ReclaimVerdict.parked, reclaimUnderPark(&g, .not_now, 10 + 5 * std.time.ns_per_ms, std.time.ns_per_ms, p.sink()));
    const reclaims0 = stats.slot_lease_reclaims.load();
    try testing.expectEqual(ReclaimVerdict.rejoined, reclaimUnderPark(&g, .granted, 10 + 7 * std.time.ns_per_ms, 2 * std.time.ns_per_ms, p.sink()));
    try testing.expectEqual(reclaims0 + 1, stats.slot_lease_reclaims.load());
    try testing.expect(!g.release(0, 0));
    try testing.expectEqual(@as(u64, 0), stats.appender_parked.load());
    try testing.expectEqual(AckVerdict.ok, g.holdAck());
    try g.preAdmission();
    try testing.expect(g.leaveAllowed());
    // Exact-sum: total = wait_successor + reclaim_rtt.
    const w = stats.appender_park_ns[0].snapshot();
    const r = stats.appender_park_ns[1].snapshot();
    const t = stats.appender_park_ns[2].snapshot();
    try testing.expectEqual(w.sum_ns + r.sum_ns, t.sum_ns);
    try testing.expectEqual(@as(u32, 1), p.count);
}

test "step 5 / the terminal poison: a park past T_park_max expires exactly once — the tripwire fires, the sink poisons, acks fail EIO, the pre-admission fails, the leave is refused (the crash shape), expiry and release are exclusive; UnknownLease expires now" {
    var g = ParkGate.arm(testing.io, 0, 100);
    var p: Poison = .{};
    _ = g.park(1_000_000);
    try testing.expectEqual(ReclaimVerdict.parked, reclaimUnderPark(&g, .not_now, 1_000_000 + 99 * std.time.ns_per_ms, 0, p.sink()));
    const tw0 = stats.appender_park_expiries.load();
    stats.appender_park_expiries.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(ReclaimVerdict.fenced, reclaimUnderPark(&g, .granted, 1_000_000 + 100 * std.time.ns_per_ms, 0, p.sink()));
    try log.testing.expectAllFired();
    try testing.expectEqual(tw0 + 1, stats.appender_park_expiries.load());
    try testing.expectEqual(@as(u32, 1), p.count);
    try testing.expect(g.isExpired() and !g.admitsTokenService());
    try testing.expectEqual(AckVerdict.eio, g.holdAck());
    try testing.expectError(error.Expired, g.preAdmission());
    // A fenced member's dismount is the crash shape: nothing durable.
    try testing.expect(!g.leaveAllowed());
    try testing.expect(g.leave_refused.load(.acquire));
    try testing.expect(!g.release(0, 0));
    try testing.expectEqual(ReclaimVerdict.fenced, reclaimUnderPark(&g, .granted, 2_000_000, 0, p.sink()));
    try testing.expectEqual(tw0 + 1, stats.appender_park_expiries.load());
    // UnknownLease: terminal at once.
    var g2 = ParkGate.arm(testing.io, 0, 1 << 40);
    _ = g2.park(5);
    stats.appender_park_expiries.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(ReclaimVerdict.fenced, reclaimUnderPark(&g2, .unknown_lease, 6, 0, p.sink()));
    try log.testing.expectAllFired();
    try testing.expect(g2.isExpired());
    try testing.expectEqual(@as(u32, 2), p.count);
}

test "THE RETIREMENT (a live member evicted re-joins in place): retire moves RUNNING or PARKED to RETIRED exactly once, exclusive with release and expiry; a parked committer wakes and refuses Retired, a later one at once; the leave writes nothing; a refused re-join expires the retired word into the poison" {
    var p: Poison = .{};
    // From PARKED, with a committer waiting at the pre-admission gate.
    var g = ParkGate.arm(testing.io, 0, 1 << 40);
    _ = g.park(1);
    const Waiter = struct {
        fn run(gate: *ParkGate, out: *?anyerror) void {
            gate.preAdmission() catch |e| {
                out.* = e;
                return;
            };
            out.* = null;
        }
    };
    var answered: ?anyerror = error.Unexpected;
    const th = try std.Thread.spawn(.{}, Waiter.run, .{ &g, &answered });
    var spins: u32 = 0;
    while (g.admission_wake.waiters(testing.io) == 0 and spins < 1_000_000) : (spins += 1) std.Thread.yield() catch {};
    try testing.expect(g.retire());
    th.join();
    try testing.expectEqual(@as(?anyerror, error.Retired), answered);
    try testing.expect(g.isRetired() and !g.isParked() and !g.isExpired());
    try testing.expectEqual(@as(u64, 3), stats.appender_parked.load());
    // Exclusive: a second retire, a release and a T_park_max expiry move nothing.
    try testing.expect(!g.retire());
    try testing.expect(!g.release(0, 0));
    try testing.expect(!g.expireIfDue(std.math.maxInt(u64), p.sink()));
    try testing.expectError(error.Retired, g.preAdmission());
    try testing.expect(g.tryEnter() == null);
    try testing.expectEqual(AckVerdict.eio, g.holdAck());
    // The retired incarnation's dismount is the crash shape.
    try testing.expect(!g.leaveAllowed());
    // A refused re-join: RETIRED → EXPIRED, the poison once.
    stats.appender_park_expiries.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    try testing.expect(g.expireNow(p.sink(), "the re-join in place was refused"));
    try log.testing.expectAllFired();
    try testing.expect(g.isExpired());
    try testing.expectEqual(@as(u32, 1), p.count);
    try testing.expect(!g.retire());
    // From RUNNING (UnknownLease before T_self).
    var g2 = ParkGate.arm(testing.io, 0, 1 << 40);
    try testing.expect(g2.retire());
    try testing.expectError(error.Retired, g2.preAdmission());
    stats.appender_parked.set(@backingInt(Word.running));
}

/// A committer through the gate (the pre-admission, the entry, the held
/// ack): this module's and the shard's tests.
pub const TestCommitter = struct {
    gate: *ParkGate,
    entered: bool = false,
    verdict: AckVerdict = .ok,

    pub fn run(self: *TestCommitter) void {
        self.gate.preAdmission() catch return;
        if (self.gate.tryEnter()) |pass| {
            self.entered = true;
            self.verdict = self.gate.holdAck();
            self.gate.leave(pass);
        }
    }
};

test "a committer parked at the pre-admission gate wakes on the release and lands; one parked on the ack hold wakes with ok" {
    var g = ParkGate.arm(testing.io, 0, 1 << 40);
    _ = g.park(1);
    var c: TestCommitter = .{ .gate = &g };
    const th = try std.Thread.spawn(.{}, TestCommitter.run, .{&c});
    // Let the committer reach the park (bounded spin on the waiter count).
    var spins: u32 = 0;
    while (g.admission_wake.waiters(testing.io) == 0 and spins < 1_000_000) : (spins += 1) std.Thread.yield() catch {};
    try testing.expect(g.release(1, 1));
    th.join();
    try testing.expect(c.entered);
    try testing.expectEqual(AckVerdict.ok, c.verdict);
}
