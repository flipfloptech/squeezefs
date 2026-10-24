//! The death ledger (charter §5.5 "deaths propagate through the death
//! ledger in tree 0 of volume 0"; `docs/porting/sym_wire_membership_
//! recovery.md` §2.5): `dead_member:{id}` and `recovered:{id, vol}` are a
//! SHARED-DISK projection every manager reads off tree 0 of volume 0 —
//! never a verb to the recoverer. The WRITER is the home shard's eviction
//! (through the shard's `DeathSink`; a reader's id holds no region and is
//! ignored); the wire form `RecordDeath` is a PEER manager's, SCREENED
//! before anything is written (own identity, a member the installed owner
//! lists live, a key this process or a live member holds — rejected; key
//! 0 or the registered key — accepted; an unknown census — kept with the
//! word and COUNTED, quirk Q-R2). A write that fails is PARKED in RAM
//! (dedup by identity) and the next poll drains it. The join RETIRES the
//! identity's standing record before any page goes Live under it.
//!
//! The POLL: one projection per `tick = checkpoint_landing_ceiling` =
//! 1,100 ms — the §5.5.2 bound `dead_member_propagation_ms ≤ poll +
//! CHECKPOINT_MAX_AGE` (≤ 2 s): probe volume 0 (unreachable for > T_owner
//! ⇒ the manager RELEASES its role rather than act on a stale ledger) →
//! drain the parked deaths → per volume release the recovered regions THEN
//! recover the dead appenders (one observable step per state) → the
//! RETIREMENT sweep at `2 × T_owner` (one TTL for every manager to act, one
//! for the successor's grace) for an identity no page names Live/Recovering
//! and no lease names → roll forward open intents iff anything was
//! recovered. The record codecs and the idempotent writes are the data
//! plane's (`sym_planes/alloc_lease.zig`: ONE authority for the ledger's
//! bytes); this module is the writer, the screen, the poll and the wire.

const std = @import("std");
const Allocator = std.mem.Allocator;
const counters = @import("../../../core/counters.zig");
const clock = @import("../../../core/clock.zig");
const log = @import("../../../core/log.zig");
const errors = @import("../../../core/errors.zig");
const cr = @import("../control_records.zig");
const alloc_lease = @import("../../../data/sym_planes/alloc_lease.zig");
const records = @import("../../../data/sym_planes/records.zig");
const verbs = @import("../../../wire/verbs.zig");
const service = @import("../../../wire/service.zig");
const screen = @import("../../../wire/screen.zig");
const shard = @import("shard.zig");

pub const WireIdentity = verbs.WireIdentity;
pub const WriterIdentity = records.WriterIdentity;
pub const DeadMember = cr.DeadMember;

pub const stats = struct {
    /// Per region acted on (closure `acted ≡ recorded × regions held`).
    pub var dead_members_acted: counters.ShardedCounter = .{};
    /// `fetch_max` of `now − ts` at each act.
    pub var dead_member_propagation_ms: counters.Gauge = .{};
    /// A `RecordDeath` key word no census could validate — kept (the tail
    /// scan is the fence) and counted: growth under a live census is a
    /// wiring defect (quirk Q-R2).
    pub var dead_member_keys_unvalidated: counters.ShardedCounter = .{};
    /// Deaths parked in RAM awaiting the next poll's drain.
    pub var dead_members_deferred: counters.Gauge = .{};
    /// `RecordDeath` words the screen refused (the hostile/buggy class).
    pub var record_death_rejected: counters.ShardedCounter = .{};
    pub var recovery_ledger_polls: counters.ShardedCounter = .{};
    /// Volume 0 unreachable from this manager (the vol-0 rule's input).
    pub var manager_vol0_unreachable: counters.Gauge = .{};
    /// A manager that released its role under the vol-0 rule.
    pub var manager_role_releases: counters.ShardedCounter = .{};
    pub var recovered_regions_released: counters.ShardedCounter = .{};
    /// Death records retired because their member is live again
    /// (`retireForJoin`: a new incarnation under the dead pair).
    pub var dead_members_retired_rejoined: counters.ShardedCounter = .{};
    /// Open intents of recovered slots the poll handed to the cross-owner
    /// cadence (adopted into its register; its next pass completes them).
    pub var recovery_intents_adopted: counters.ShardedCounter = .{};
};

/// The ledger poll tick = the checkpoint landing ceiling (1,100 ms):
/// propagation ≤ `poll + CHECKPOINT_MAX_AGE` ≤ 2 s.
pub const LEDGER_POLL_TICK_MS: u64 = shard.CHECKPOINT_LANDING_CEILING_MS;

fn writerOf(w: WireIdentity) WriterIdentity {
    return .{ .node_token = w.node_token, .mount_slot = w.mount_slot, .writer_id = w.writer_id };
}

pub fn wireOf(w: WriterIdentity) WireIdentity {
    return .{ .node_token = w.node_token, .writer_id = w.writer_id, .mount_slot = w.mount_slot, .reserved = 0 };
}

/// Volume 0's tree 0 as the ledger sees it: the data plane's control
/// store plus the range the poll walks.
pub const Store = struct {
    control: alloc_lease.ControlStore,
    range_ctx: *anyopaque,
    /// Every `(key, value)` inside `[start, end]` (caller frees each).
    range: *const fn (ctx: *anyopaque, gpa: Allocator, start: []const u8, end: []const u8, out: *std.ArrayList(KeyValue)) error{ Io, OutOfMemory }!void,
    /// Volume 0 reachable (a device probe; false = the vol-0 rule's clock
    /// runs).
    probe: *const fn (ctx: *anyopaque) bool,
};

pub const KeyValue = struct { key: []u8, value: []u8 };

pub fn freeKvs(gpa: Allocator, kvs: *std.ArrayList(KeyValue)) void {
    for (kvs.items) |kv| {
        gpa.free(kv.key);
        gpa.free(kv.value);
    }
    kvs.deinit(gpa);
}

pub const Death = struct { identity: WriterIdentity, record: DeadMember };

const Pending = struct { identity: WriterIdentity, record: DeadMember };

pub const Ledger = struct {
    gpa: Allocator,
    store: Store,
    set_width: u16,
    /// This mount's own identity (the screen's first rule).
    own: WireIdentity,
    pending: std.ArrayList(Pending) = .empty,
    /// The vol-0 rule's clock: the first unreachable probe's instant.
    vol0_unreachable_since_ms: ?u64 = null,
    /// `T_owner` in force (the venue legs shorten it through the seam).
    t_owner_ms: u64,

    pub fn init(gpa: Allocator, store: Store, set_width: u16, own: WireIdentity, t_owner_ms: u64) Ledger {
        return .{ .gpa = gpa, .store = store, .set_width = set_width, .own = own, .t_owner_ms = t_owner_ms };
    }

    pub fn deinit(self: *Ledger) void {
        stats.dead_members_deferred.sub(self.pending.items.len);
        self.pending.deinit(self.gpa);
    }

    /// The writer: idempotent against the standing record; a failed write
    /// is parked (dedup by identity) for the next poll's drain.
    pub fn recordDeath(self: *Ledger, identity: WriterIdentity, d: DeadMember) void {
        alloc_lease.recordDeath(self.store.control, self.gpa, identity, d) catch {
            self.park(identity, d);
            return;
        };
    }

    fn park(self: *Ledger, identity: WriterIdentity, d: DeadMember) void {
        for (self.pending.items) |p| if (p.identity.sameMount(identity)) return;
        self.pending.append(self.gpa, .{ .identity = identity, .record = d }) catch {
            log.err("death ledger: a death record for node 0x{x} mount slot {d} could not be written NOR parked — the identity's regions wait for the next eviction", .{ identity.node_token, identity.mount_slot });
            return;
        };
        stats.dead_members_deferred.add(1);
    }

    /// `drain_pending_deaths`: every parked record retried; what still
    /// fails stays parked.
    pub fn drainPending(self: *Ledger) u32 {
        var landed: u32 = 0;
        var i: usize = 0;
        while (i < self.pending.items.len) {
            const p = self.pending.items[i];
            if (alloc_lease.recordDeath(self.store.control, self.gpa, p.identity, p.record)) |_| {
                _ = self.pending.orderedRemove(i);
                stats.dead_members_deferred.sub(1);
                landed += 1;
            } else |_| i += 1;
        }
        return landed;
    }

    /// The home shard's eviction as the ledger's writer: a reader's id
    /// holds no region (ignored); the record carries the member's epoch
    /// and registrant key, `early = false`, the recorder's wall clock.
    pub fn deathSink(self: *Ledger) shard.DeathSink {
        return .{ .ctx = self, .noteDeath = &noteDeath };
    }

    fn noteDeath(ctx: ?*anyopaque, dead: shard.DeadMember) void {
        const self: *Ledger = @ptrCast(@alignCast(ctx.?));
        const identity = dead.identity orelse return;
        self.recordDeath(writerOf(identity), .{ .epoch = dead.epoch, .ts_realtime_ns = @intCast(clock.realtimeCoarseNs()), .pr_key = dead.pr_key, .early = false });
    }

    /// The join / the arm RETIRES the identity's standing record (and
    /// every `recovered:` row) BEFORE any page goes Live under it: the
    /// join is the identity's newer incarnation.
    pub fn retireForJoin(self: *Ledger, identity: WriterIdentity) error{Io}!void {
        alloc_lease.retireDeathRecords(self.store.control, self.gpa, identity, self.set_width) catch |e| return switch (e) {
            error.WindowFull, error.Io, error.OutOfMemory => error.Io,
        };
        stats.dead_members_retired_rejoined.inc();
    }

    pub fn readDeath(self: *Ledger, identity: WriterIdentity) error{Io}!?DeadMember {
        return alloc_lease.readDeath(self.store.control, self.gpa, identity) catch error.Io;
    }

    pub fn isRecovered(self: *Ledger, identity: WriterIdentity, vol: u16) error{Io}!bool {
        return alloc_lease.isRecovered(self.store.control, self.gpa, identity, vol) catch error.Io;
    }

    /// `recovered:{id, vol}` (idempotent; the allocation-lease re-grant's
    /// gate).
    pub fn recordRecovered(self: *Ledger, identity: WriterIdentity, vol: u16, by_term: u64, now_realtime_ns: u64) error{Io}!bool {
        if (try self.isRecovered(identity, vol)) return false;
        alloc_lease.recordRecovered(self.store.control, self.gpa, identity, vol, by_term, now_realtime_ns) catch return error.Io;
        return true;
    }

    /// Every standing death record (the poll's input).
    pub fn listDeaths(self: *Ledger, gpa: Allocator, out: *std.ArrayList(Death)) error{ Io, OutOfMemory }!void {
        const r = cr.rangeOf(.dead_member);
        var kvs: std.ArrayList(KeyValue) = .empty;
        defer freeKvs(gpa, &kvs);
        try self.store.range(self.store.range_ctx, gpa, r.start.slice(), r.end.slice(), &kvs);
        for (kvs.items) |kv| {
            const id = cr.identityOfKey(kv.key) orelse continue;
            const d = cr.DeadMemberWire.decode(kv.value) catch {
                log.err("death ledger: dead_member record for node 0x{x} mount slot {d} does not decode — refusing to act on it (fsck C13-class corruption of tree 0)", .{ id.node_token, id.mount_slot });
                return error.Io;
            };
            try out.append(gpa, .{ .identity = .{ .node_token = id.node_token, .mount_slot = id.mount_slot, .writer_id = 0 }, .record = d });
        }
    }

    /// The vol-0 rule (`manager_should_release_role`): a manager that
    /// cannot reach volume 0 for longer than `T_owner` releases its role
    /// rather than act on a stale ledger. Answers true ONCE per outage.
    pub fn noteVol0Probe(self: *Ledger, now_ms: u64) bool {
        if (self.store.probe(self.store.range_ctx)) {
            self.vol0_unreachable_since_ms = null;
            stats.manager_vol0_unreachable.set(0);
            return false;
        }
        stats.manager_vol0_unreachable.set(1);
        const since = self.vol0_unreachable_since_ms orelse blk: {
            self.vol0_unreachable_since_ms = now_ms;
            break :blk now_ms;
        };
        if (now_ms -| since <= self.t_owner_ms) return false;
        stats.manager_role_releases.inc();
        self.vol0_unreachable_since_ms = null;
        return true;
    }

    /// The `RecordDeath` screen: every verdict BEFORE anything is written.
    /// `census` supplies the owner's view (null = this node owns no shard
    /// the member could be in: the word is unvalidated).
    pub fn screenDeath(self: *Ledger, req: verbs.RecordDeathReq, census: ?*const shard.Owner, now_ms: u64) screen.Verdict {
        // The one key the screen asks about, held or not: no allocation, so
        // the screen never fails open.
        const held = [1]u64{req.pr_key};
        var n_held: usize = 0;
        var listed_live = false;
        var registered: ?u64 = null;
        if (census) |o| {
            const id = shard.writerMemberId(req.member);
            listed_live = o.memberIsLive(id, now_ms);
            registered = o.registeredKey(id);
            if (o.holdsKey(req.pr_key)) n_held = 1;
        }
        const b: screen.DeathBounds = .{ .own = self.own, .listed_live = listed_live, .registered_key = registered, .held_keys = held[0..n_held] };
        const v = screen.recordDeath(req, &b);
        if (v.isOk() and req.pr_key != 0 and registered == null) stats.dead_member_keys_unvalidated.inc();
        return v;
    }
};

// ---------------------------------------------------------------------------
// The poll: one projection per tick.
// ---------------------------------------------------------------------------

pub const RecoverOutcome = enum { acted, skipped, failed };

/// One managed volume's hooks (the recovery driver's, `recovery.zig`).
pub const VolumeHook = struct {
    ctx: *anyopaque,
    vol: u16,
    /// Regions `Recovered` whose identity no `alloc_lease:` names any more
    /// go `Free` (the poll's FIRST act); the count released.
    releaseRecovered: *const fn (ctx: *anyopaque) u32,
    /// The §5.9 procedure for one dead identity on this volume.
    recoverDead: *const fn (ctx: *anyopaque, death: Death) RecoverOutcome,
    /// A `Live` / `Recovering` page under the identity still stands.
    regionPending: *const fn (ctx: *anyopaque, identity: WriterIdentity) bool,
};

/// The hand-off of recovered slots' open intents to the cross-owner
/// cadence (its register adopts them; null until the plane is armed). The
/// poll runs on the membership cadence and never waits on a guard, so it
/// adopts and the cadence rolls forward (decision 0003 §AE.2).
pub const IntentAdoption = struct {
    ctx: ?*anyopaque,
    adopt: *const fn (ctx: ?*anyopaque) u32,
};

pub const Report = struct {
    deaths: u32 = 0,
    acted: u32 = 0,
    released: u32 = 0,
    retired: u32 = 0,
    drained: u32 = 0,
    intents_adopted: u32 = 0,
    role_released: bool = false,
};

pub const Projection = struct {
    ledger: *Ledger,
    volumes: []const VolumeHook,
    /// The data volumes' tags (`namedAsHolder`'s walk).
    vol_tags: []const u64,
    adoption: ?IntentAdoption = null,

    /// `recover_dead_appenders_set` (REC:3486-3569).
    pub fn run(self: *Projection, now_ms: u64, now_realtime_ns: u64) Report {
        var rep: Report = .{};
        stats.recovery_ledger_polls.inc();
        if (self.ledger.noteVol0Probe(now_ms)) {
            rep.role_released = true;
            return rep;
        }
        if (self.ledger.vol0_unreachable_since_ms != null) return rep;
        rep.drained = self.ledger.drainPending();
        var deaths: std.ArrayList(Death) = .empty;
        defer deaths.deinit(self.ledger.gpa);
        self.ledger.listDeaths(self.ledger.gpa, &deaths) catch |e| {
            log.warn("death ledger: the poll could not read the ledger ({s}) — this projection acts on nothing", .{@errorName(e)});
            return rep;
        };
        rep.deaths = @intCast(deaths.items.len);
        for (self.volumes) |v| {
            rep.released += v.releaseRecovered(v.ctx);
            for (deaths.items) |d| switch (v.recoverDead(v.ctx, d)) {
                .acted => {
                    rep.acted += 1;
                    stats.dead_members_acted.inc();
                    const lag_ms: u64 = (now_realtime_ns -| d.record.ts_realtime_ns) / std.time.ns_per_ms;
                    if (lag_ms > stats.dead_member_propagation_ms.load()) stats.dead_member_propagation_ms.set(lag_ms);
                },
                .skipped, .failed => {},
            };
        }
        stats.recovered_regions_released.add(rep.released);
        rep.retired = self.retire(deaths.items, now_realtime_ns);
        if (rep.acted != 0) if (self.adoption) |a| {
            rep.intents_adopted = a.adopt(a.ctx);
            stats.recovery_intents_adopted.add(rep.intents_adopted);
        };
        return rep;
    }

    /// The retirement sweep: a record past `2 × T_owner` whose identity has
    /// no Live/Recovering page on ANY volume and that no lease names.
    fn retire(self: *Projection, deaths: []const Death, now_realtime_ns: u64) u32 {
        var n: u32 = 0;
        for (deaths) |d| {
            var pending = false;
            for (self.volumes) |v| pending = pending or v.regionPending(v.ctx, d.identity);
            const named = alloc_lease.namedAsHolder(self.ledger.store.control, self.ledger.gpa, d.identity, self.vol_tags) catch true;
            if (!retirable(d.record.ts_realtime_ns, now_realtime_ns, self.ledger.t_owner_ms, pending, named)) continue;
            alloc_lease.retireDeathRecords(self.ledger.store.control, self.ledger.gpa, d.identity, self.ledger.set_width) catch continue;
            n += 1;
        }
        return n;
    }
};

/// The sweep predicate at the `T_owner` in force (the data plane's is the
/// constant's form; the venue legs shorten it).
pub fn retirable(recorded_realtime_ns: u64, now_realtime_ns: u64, t_owner_ms: u64, region_pending: bool, named_as_holder: bool) bool {
    if (region_pending or named_as_holder) return false;
    return now_realtime_ns -| recorded_realtime_ns >= alloc_lease.DEATH_RECORD_RETIRE_TTLS * t_owner_ms * std.time.ns_per_ms;
}

// ---------------------------------------------------------------------------
// The wire: `RecordDeath` / `RecordRecovered` at volume 0's manager, and
// the peer manager's clients.
// ---------------------------------------------------------------------------

pub const LedgerHandlers = struct {
    ledger: *Ledger,
    /// The shard this node owns (the screen's census), if any.
    census: ?*const shard.Owner,
    now: *const fn (ctx: ?*anyopaque) u64,
    now_ctx: ?*anyopaque = null,
    is_vol0_manager: *const fn (ctx: ?*anyopaque) bool,

    pub fn register(self: *LedgerHandlers, svc: *service.Service) void {
        svc.register(.record_death, .{ .ctx = self, .serve = &serveDeath });
        svc.register(.record_recovered, .{ .ctx = self, .serve = &serveRecovered });
    }

    fn serveDeath(ctx: *anyopaque, req: *const service.Request, _: []u8) service.Answer {
        const self: *LedgerHandlers = @ptrCast(@alignCast(ctx));
        if (!self.is_vol0_manager(self.now_ctx)) return .{ .status = .{ .status = .not_manager } };
        const r = req.payload.record_death;
        switch (self.ledger.screenDeath(r, self.census, self.now(self.now_ctx))) {
            .ok => {},
            .rejected => |why| {
                stats.record_death_rejected.inc();
                return service.Answer.rejected(@tagName(why));
            },
        }
        const identity = writerOf(r.member);
        const standing = self.ledger.readDeath(identity) catch return service.Answer.refused(error.InputOutput, "the ledger could not be read");
        if (standing != null) return .{ .ok = .{ .record_death = verbs.Ack.replay } };
        self.ledger.recordDeath(identity, .{ .epoch = r.epoch, .ts_realtime_ns = @intCast(clock.realtimeCoarseNs()), .pr_key = r.pr_key, .early = false });
        return .{ .ok = .{ .record_death = verbs.Ack.fresh } };
    }

    fn serveRecovered(ctx: *anyopaque, req: *const service.Request, _: []u8) service.Answer {
        const self: *LedgerHandlers = @ptrCast(@alignCast(ctx));
        if (!self.is_vol0_manager(self.now_ctx)) return .{ .status = .{ .status = .not_manager } };
        const r = req.payload.record_recovered;
        switch (screen.recordRecovered(r, self.ledger.set_width)) {
            .ok => {},
            .rejected => |why| return service.Answer.rejected(@tagName(why)),
        }
        const fresh = self.ledger.recordRecovered(writerOf(r.member), r.vol, 0, @intCast(clock.realtimeCoarseNs())) catch return service.Answer.refused(error.InputOutput, "the ledger could not be written");
        return .{ .ok = .{ .record_recovered = if (fresh) verbs.Ack.fresh else verbs.Ack.replay } };
    }
};

/// A peer manager's `RecordDeath` over its link to volume 0's manager:
/// `already` when the record stood.
pub fn recordDeathOver(link: shard.Link, member: WireIdentity, epoch: u64, pr_key: u64) errors.Error!bool {
    const rep = try link.call(link.ctx, .{ .record_death = .{ .volume = 0, .reserved = 0, .reserved2 = 0, .member = member, .epoch = epoch, .pr_key = pr_key } });
    return rep.record_death.already == 1;
}

pub fn recordRecoveredOver(link: shard.Link, member: WireIdentity, vol: u16) errors.Error!bool {
    const rep = try link.call(link.ctx, .{ .record_recovered = .{ .volume = 0, .vol = vol, .reserved = 0, .member = member } });
    return rep.record_recovered.already == 1;
}

// ---------------------------------------------------------------------------
// Tests: a RAM volume 0 behind the store.
// ---------------------------------------------------------------------------

const testing = std.testing;

/// The data plane's RAM tree 0 with the range walk and the probe.
pub const RamLedgerStore = struct {
    ram: alloc_lease.RamControlStore,
    reachable: bool = true,

    pub fn init(gpa: Allocator) RamLedgerStore {
        return .{ .ram = alloc_lease.RamControlStore.init(gpa) };
    }

    pub fn deinit(self: *RamLedgerStore) void {
        self.ram.deinit();
    }

    fn range(ctx: *anyopaque, gpa: Allocator, start: []const u8, end: []const u8, out: *std.ArrayList(KeyValue)) error{ Io, OutOfMemory }!void {
        const self: *RamLedgerStore = @ptrCast(@alignCast(ctx));
        var it = self.ram.map.iterator();
        while (it.next()) |kv| {
            const k = kv.key_ptr.*;
            if (std.mem.order(u8, k, start) == .lt or std.mem.order(u8, k, end) == .gt) continue;
            try out.append(gpa, .{ .key = try gpa.dupe(u8, k), .value = try gpa.dupe(u8, kv.value_ptr.*) });
        }
    }

    fn probe(ctx: *anyopaque) bool {
        const self: *RamLedgerStore = @ptrCast(@alignCast(ctx));
        return self.reachable;
    }

    pub fn store(self: *RamLedgerStore) Store {
        return .{ .control = self.ram.store(), .range_ctx = self, .range = &range, .probe = &probe };
    }
};

fn wid(n: u32) WriterIdentity {
    return .{ .node_token = 0xA000 + n, .mount_slot = n, .writer_id = 0xB000 + n };
}

const FakeVolume = struct {
    vol: u16,
    /// Identities with a Live/Recovering page here.
    pending: std.ArrayList(WriterIdentity) = .empty,
    acted: u32 = 0,
    released_once: bool = false,
    outcome: RecoverOutcome = .acted,

    fn release(ctx: *anyopaque) u32 {
        const self: *FakeVolume = @ptrCast(@alignCast(ctx));
        if (self.released_once) return 0;
        self.released_once = true;
        return 1;
    }
    fn recover(ctx: *anyopaque, d: Death) RecoverOutcome {
        const self: *FakeVolume = @ptrCast(@alignCast(ctx));
        for (self.pending.items, 0..) |p, i| if (p.sameMount(d.identity)) {
            if (self.outcome == .acted) {
                _ = self.pending.swapRemove(i);
                self.acted += 1;
            }
            return self.outcome;
        };
        return .skipped;
    }
    fn isPending(ctx: *anyopaque, id: WriterIdentity) bool {
        const self: *FakeVolume = @ptrCast(@alignCast(ctx));
        for (self.pending.items) |p| if (p.sameMount(id)) return true;
        return false;
    }
    fn hook(self: *FakeVolume) VolumeHook {
        return .{ .ctx = self, .vol = self.vol, .releaseRecovered = &release, .recoverDead = &recover, .regionPending = &isPending };
    }
};

var adoptions: u32 = 0;
fn adoptFn(_: ?*anyopaque) u32 {
    adoptions += 1;
    return 3;
}

test "the writer: the home shard's eviction records a death ONCE (a reader's id records nothing); a write the store refuses is parked, counted and drained by the next poll; the join retires the standing record" {
    const gpa = testing.allocator;
    var rs = RamLedgerStore.init(gpa);
    defer rs.deinit();
    var l = Ledger.init(gpa, rs.store(), 2, wireOf(wid(0)), 45_000);
    defer l.deinit();
    const sink = l.deathSink();
    const recorded0 = alloc_lease.stats.dead_members_recorded.load();
    sink.noteDeath(sink.ctx, .{ .id = @splat(0x11), .identity = null, .epoch = 1, .pr_key = 0 });
    try testing.expectEqual(@as(usize, 0), rs.ram.count());
    sink.noteDeath(sink.ctx, .{ .id = shard.writerMemberId(wireOf(wid(1))), .identity = wireOf(wid(1)), .epoch = 5, .pr_key = 0x51 });
    sink.noteDeath(sink.ctx, .{ .id = shard.writerMemberId(wireOf(wid(1))), .identity = wireOf(wid(1)), .epoch = 6, .pr_key = 0x51 });
    try testing.expectEqual(recorded0 + 1, alloc_lease.stats.dead_members_recorded.load());
    const d = (try l.readDeath(wid(1))).?;
    try testing.expectEqual(@as(u64, 5), d.epoch);
    try testing.expectEqual(@as(u64, 0x51), d.pr_key);
    try testing.expect(!d.early);
    // A refused write (every admission class) parks; the drain lands it.
    rs.ram.refuse_try_next = 0;
    var dead_store = RamLedgerStore.init(gpa);
    defer dead_store.deinit();
    var l2 = Ledger.init(gpa, dead_store.store(), 2, wireOf(wid(0)), 45_000);
    defer l2.deinit();
    const refuse = struct {
        fn put(_: *anyopaque, _: []const u8, _: []const u8, _: alloc_lease.Admission) alloc_lease.StoreError!void {
            return error.Io;
        }
        fn get(_: *anyopaque, _: Allocator, _: []const u8) alloc_lease.StoreError!?[]u8 {
            return null;
        }
        fn del(_: *anyopaque, _: []const u8, _: alloc_lease.Admission) alloc_lease.StoreError!void {
            return error.Io;
        }
        const vt: alloc_lease.ControlStore.VTable = .{ .get = &get, .put = &put, .delete = &del };
    };
    var marker: u8 = 0;
    l2.store.control = .{ .ctx = &marker, .vtable = &refuse.vt };
    const deferred0 = stats.dead_members_deferred.load();
    const tw0 = alloc_lease.stats.dead_member_write_deferrals.load();
    alloc_lease.stats.dead_member_write_deferrals.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    l2.recordDeath(wid(2), .{ .epoch = 1, .ts_realtime_ns = 1, .pr_key = 0, .early = false });
    l2.recordDeath(wid(2), .{ .epoch = 1, .ts_realtime_ns = 1, .pr_key = 0, .early = false });
    try log.testing.expectAllFired();
    try testing.expectEqual(tw0 + 2, alloc_lease.stats.dead_member_write_deferrals.load());
    try testing.expectEqual(deferred0 + 1, stats.dead_members_deferred.load());
    try testing.expectEqual(@as(u32, 0), l2.drainPending());
    l2.store.control = dead_store.ram.store();
    try testing.expectEqual(@as(u32, 1), l2.drainPending());
    try testing.expectEqual(deferred0, stats.dead_members_deferred.load());
    try testing.expect((try l2.readDeath(wid(2))) != null);
    // The join retires it.
    try l2.retireForJoin(wid(2));
    try testing.expect((try l2.readDeath(wid(2))) == null);
}

test "the screen: own identity, a live member, a key the census holds or that contradicts the registered key are rejected before any write; key 0 and the registered key are accepted; a member no census knows is accepted and counted unvalidated" {
    const gpa = testing.allocator;
    var rs = RamLedgerStore.init(gpa);
    defer rs.deinit();
    var l = Ledger.init(gpa, rs.store(), 2, wireOf(wid(0)), 45_000);
    defer l.deinit();
    const clocks = try shard.LeaseClocks.derive(45_000, 0, 100);
    var o = try shard.Owner.arm(gpa, clocks, 0, 1, 0, &.{}, 0);
    defer o.deinit();
    _ = o.join(.{ .id = shard.writerMemberId(wireOf(wid(3))), .role = .writer, .endpoint = .none, .pid = 1, .boot = @splat(0), .prior_epoch = null, .pr_key = 0x33, .mount = "/m", .row = null }, 0).granted;
    const req = struct {
        fn of(id: WireIdentity, key: u64) verbs.RecordDeathReq {
            return .{ .volume = 0, .reserved = 0, .reserved2 = 0, .member = id, .epoch = 1, .pr_key = key };
        }
    };
    try testing.expect(!l.screenDeath(req.of(wireOf(wid(0)), 0), &o, 0).isOk());
    try testing.expect(!l.screenDeath(req.of(wireOf(wid(3)), 0), &o, 0).isOk());
    try testing.expect(!l.screenDeath(req.of(wireOf(wid(4)), 0x33), &o, 0).isOk());
    try testing.expect(l.screenDeath(req.of(wireOf(wid(4)), 0), &o, 0).isOk());
    // Past T_owner the member is evicted: its key stays HELD (departed,
    // 2 × T_owner) so a word naming it for anyone is a contradiction; a
    // word naming an unknown key for the now-unregistered member is kept
    // and counted unvalidated.
    _ = o.expireDue(50_000);
    try testing.expect(!l.screenDeath(req.of(wireOf(wid(4)), 0x33), &o, 50_000).isOk());
    const unval0 = stats.dead_member_keys_unvalidated.load();
    try testing.expect(l.screenDeath(req.of(wireOf(wid(3)), 0x44), &o, 50_000).isOk());
    try testing.expect(l.screenDeath(req.of(wireOf(wid(9)), 0x99), &o, 50_000).isOk());
    try testing.expectEqual(unval0 + 2, stats.dead_member_keys_unvalidated.load());
    try testing.expect(l.screenDeath(req.of(wireOf(wid(9)), 0), null, 50_000).isOk());
    try testing.expectEqual(unval0 + 2, stats.dead_member_keys_unvalidated.load());
}

test "an evicted member's key stays held through an allocation failure: the departure keeps it and the screen still rejects a death naming it (the screen never fails open)" {
    const gpa = testing.allocator;
    var rs = RamLedgerStore.init(gpa);
    defer rs.deinit();
    var failing = std.testing.FailingAllocator.init(gpa, .{});
    var l = Ledger.init(failing.allocator(), rs.store(), 2, wireOf(wid(0)), 45_000);
    defer l.deinit();
    const clocks = try shard.LeaseClocks.derive(45_000, 0, 100);
    var o = try shard.Owner.arm(failing.allocator(), clocks, 0, 1, 0, &.{}, 0);
    defer o.deinit();
    _ = o.join(.{ .id = shard.writerMemberId(wireOf(wid(3))), .role = .writer, .endpoint = .none, .pid = 1, .boot = @splat(0), .prior_epoch = null, .pr_key = 0x33, .mount = "/m", .row = null }, 0).granted;
    failing.fail_index = failing.alloc_index;
    failing.resize_fail_index = failing.resize_index;
    try testing.expectEqual(@as(u32, 1), o.expireDue(50_000));
    const req: verbs.RecordDeathReq = .{ .volume = 0, .reserved = 0, .reserved2 = 0, .member = wireOf(wid(4)), .epoch = 1, .pr_key = 0x33 };
    try testing.expect(!l.screenDeath(req, &o, 50_000).isOk());
}

test "the poll: release-recovered first, then every death on every volume (acted per region; propagation = max lag), the retirement at 2 × T_owner only when no page stands and no lease names it, the roll-forward iff anything was recovered; the vol-0 rule releases the role after T_owner unreachable, once" {
    const gpa = testing.allocator;
    var rs = RamLedgerStore.init(gpa);
    defer rs.deinit();
    var l = Ledger.init(gpa, rs.store(), 2, wireOf(wid(0)), 45_000);
    defer l.deinit();
    const ts: u64 = 1_000 * std.time.ns_per_ms;
    l.recordDeath(wid(1), .{ .epoch = 1, .ts_realtime_ns = ts, .pr_key = 0, .early = false });
    l.recordDeath(wid(2), .{ .epoch = 2, .ts_realtime_ns = ts, .pr_key = 0, .early = false });
    var v0: FakeVolume = .{ .vol = 0 };
    defer v0.pending.deinit(gpa);
    var v1: FakeVolume = .{ .vol = 1 };
    defer v1.pending.deinit(gpa);
    try v0.pending.append(gpa, wid(1));
    try v1.pending.append(gpa, wid(1));
    try v1.pending.append(gpa, wid(2));
    const hooks = [_]VolumeHook{ v0.hook(), v1.hook() };
    adoptions = 0;
    var p: Projection = .{ .ledger = &l, .volumes = &hooks, .vol_tags = &.{}, .adoption = .{ .ctx = null, .adopt = &adoptFn } };
    const acted0 = stats.dead_members_acted.load();
    const polls0 = stats.recovery_ledger_polls.load();
    const r = p.run(2_000, ts + 1_500 * std.time.ns_per_ms);
    try testing.expectEqual(@as(u32, 2), r.deaths);
    try testing.expectEqual(@as(u32, 3), r.acted);
    try testing.expectEqual(@as(u32, 2), r.released);
    try testing.expectEqual(@as(u32, 0), r.retired);
    try testing.expectEqual(@as(u32, 3), r.intents_adopted);
    try testing.expectEqual(@as(u32, 1), adoptions);
    try testing.expectEqual(acted0 + 3, stats.dead_members_acted.load());
    try testing.expectEqual(polls0 + 1, stats.recovery_ledger_polls.load());
    try testing.expect(stats.dead_member_propagation_ms.load() >= 1_500);
    // The closure: acted ≡ recorded (2) × regions held (1 + 2).
    try testing.expectEqual(@as(u32, 1), v0.acted);
    try testing.expectEqual(@as(u32, 2), v1.acted);
    // Nothing left to act on; no roll-forward; not yet retirable.
    const r2 = p.run(3_000, ts + 2 * 45_000 * std.time.ns_per_ms - 1);
    try testing.expectEqual(@as(u32, 0), r2.acted);
    try testing.expectEqual(@as(u32, 0), r2.intents_adopted);
    try testing.expectEqual(@as(u32, 0), r2.retired);
    try testing.expectEqual(@as(u32, 1), adoptions);
    // A page still Recovering under wid(2) on v0 blocks its retirement; wid(1) retires.
    try v0.pending.append(gpa, wid(2));
    v0.outcome = .failed;
    const retired0 = alloc_lease.stats.dead_member_records_retired.load();
    const r3 = p.run(4_000, ts + 2 * 45_000 * std.time.ns_per_ms);
    try testing.expectEqual(@as(u32, 1), r3.retired);
    try testing.expectEqual(retired0 + 1, alloc_lease.stats.dead_member_records_retired.load());
    try testing.expect((try l.readDeath(wid(1))) == null);
    try testing.expect((try l.readDeath(wid(2))) != null);
    // The predicate at a shortened T_owner.
    try testing.expect(retirable(0, 2 * 3_000 * std.time.ns_per_ms, 3_000, false, false));
    try testing.expect(!retirable(0, 2 * 3_000 * std.time.ns_per_ms - 1, 3_000, false, false));
    try testing.expect(!retirable(0, 1 << 60, 3_000, true, false));
    try testing.expect(!retirable(0, 1 << 60, 3_000, false, true));
    // The vol-0 rule.
    rs.reachable = false;
    const releases0 = stats.manager_role_releases.load();
    try testing.expect(!p.run(10_000, 0).role_released);
    try testing.expectEqual(@as(u64, 1), stats.manager_vol0_unreachable.load());
    try testing.expect(!p.run(10_000 + 45_000, 0).role_released);
    try testing.expect(p.run(10_000 + 45_001, 0).role_released);
    try testing.expectEqual(releases0 + 1, stats.manager_role_releases.load());
    rs.reachable = true;
    try testing.expect(!p.run(100_000, 0).role_released);
    try testing.expectEqual(@as(u64, 0), stats.manager_vol0_unreachable.load());
}

const DirectLink = struct {
    svc: service.Service = .init,
    reply: [4096]u8 = undefined,
    scratch: [4096]u8 = undefined,
    frame_buf: [4096]u8 = undefined,
    peer: ?WireIdentity,

    fn call(ctx: *anyopaque, req: verbs.Request) errors.Error!verbs.Reply {
        const self: *DirectLink = @ptrCast(@alignCast(ctx));
        var b: @import("../../../wire/codec.zig").Builder = .init(&self.frame_buf);
        verbs.encodeRequest(req, &b) catch return error.ValueTooLarge;
        const verb: verbs.Verb = @as(verbs.Verb, req);
        const header: @import("../../../wire/frame.zig").Header = .{ .flags = 1, .len = 0, .verb = @backingInt(verb), .status = 0, .correlation = 1, .seq = 1, .schema = verb.schema() };
        const served = self.svc.serve(header, b.written(), self.peer, 0, &self.reply, &self.scratch);
        return service.interpretReply(verb, served.status, served.payload);
    }

    fn link(self: *DirectLink) shard.Link {
        return .{ .ctx = self, .call = &call };
    }
};

var is_vol0: bool = true;
fn isVol0(_: ?*anyopaque) bool {
    return is_vol0;
}
fn zeroNow(_: ?*anyopaque) u64 {
    return 0;
}

test "the wire: RecordDeath from a peer manager is screened, written once (already on the replay), rejected for a hostile word, not_manager off volume 0; RecordRecovered is idempotent and screened by the set width" {
    const gpa = testing.allocator;
    var rs = RamLedgerStore.init(gpa);
    defer rs.deinit();
    var l = Ledger.init(gpa, rs.store(), 2, wireOf(wid(0)), 45_000);
    defer l.deinit();
    is_vol0 = true;
    var h: LedgerHandlers = .{ .ledger = &l, .census = null, .now = &zeroNow, .is_vol0_manager = &isVol0 };
    var dl: DirectLink = .{ .peer = wireOf(wid(7)) };
    h.register(&dl.svc);
    try testing.expect(!try recordDeathOver(dl.link(), wireOf(wid(5)), 3, 0));
    try testing.expect(try recordDeathOver(dl.link(), wireOf(wid(5)), 3, 0));
    try testing.expectEqual(@as(u64, 3), (try l.readDeath(wid(5))).?.epoch);
    const rejected0 = stats.record_death_rejected.load();
    try testing.expectError(error.Rejected, recordDeathOver(dl.link(), wireOf(wid(0)), 1, 0));
    try testing.expectError(error.Rejected, recordDeathOver(dl.link(), .zero, 1, 0));
    try testing.expectEqual(rejected0 + 2, stats.record_death_rejected.load());
    try testing.expect(!try recordRecoveredOver(dl.link(), wireOf(wid(5)), 1));
    try testing.expect(try recordRecoveredOver(dl.link(), wireOf(wid(5)), 1));
    try testing.expect(try l.isRecovered(wid(5), 1));
    try testing.expectError(error.Rejected, recordRecoveredOver(dl.link(), wireOf(wid(5)), 2));
    is_vol0 = false;
    try testing.expectError(error.NotManager, recordDeathOver(dl.link(), wireOf(wid(6)), 1, 0));
}
