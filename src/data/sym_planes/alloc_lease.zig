//! The allocation lease (charter §5.5 "per DATA volume a floating
//! allocation lease (`alloc_lease:{vol_tag}` in tree 0 of volume 0; first-
//! come at term 1; a dead holder succeeded only after `dead_member:` AND
//! `recovered:{holder, home_vol}`)"; `docs/porting/sym_data_planes.md` §1).
//! PORTABLE: tree 0 of volume 0 is a `ControlStore` seam (the manager's
//! KV backend on Linux, a RAM table in the contracts).
//!
//! The ladder (`AllocLeaseAcquire`, served by VOLUME 0's manager): screen
//! FIRST — a wire integer is never an allocation authority — then: no
//! record ⇒ term 1; holder == caller ⇒ `already` WITH the record's pages
//! (crash row 8: a successor that died before its copy retries and still
//! learns what to copy); a live holder ⇒ `Busy` (a lease moves only by its
//! release — allocate through block grants from the holder); dead but not
//! recovered ⇒ `LeaseDeferred` (the typed retryable class: re-granted only
//! after the home manager's ring replay applied the bitmap deltas); dead
//! AND recovered ⇒ term + 1 naming the PREDECESSOR'S PAGES until the
//! successor publishes its copy. **There is no renewal**: liveness is the
//! death ledger, the holder's own re-acquire answering `already` is the
//! re-hold's probe, the membership beat keeps the HOLDER alive, and the
//! term word moves only on succession.
//!
//! Every verb is idempotent against DURABLE state and every write is ONE
//! control entry `Try`-admitted (a full ring-0 window answers `Deferred`,
//! never parks a handler); the death record alone is PARKING-admitted
//! ("never a `Try` refusal that loses the death").

const std = @import("std");
const Atomic = std.atomic.Value;
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const counters = @import("../../core/counters.zig");
const log = @import("../../core/log.zig");
const records = @import("records.zig");
const data_bitmap = @import("data_bitmap.zig");
const block_alloc = @import("../block_alloc.zig");

const cr = records.control;

pub const WriterIdentity = records.WriterIdentity;
pub const BitmapRef = records.BitmapRef;

// ---------------------------------------------------------------------------
// Constants (every one with its WHY).
// ---------------------------------------------------------------------------

/// The owner's eviction TTL (charter §5.5 "home shard evicts at `T_owner`
/// (45 s)"; 0002 row 188: `TTL > nvme_io_timeout 30 s + beat 10 s`). The
/// ledger's clocks are CLOCK_REALTIME ns (compared across hosts).
pub const T_OWNER_MS: u64 = 45_000;
pub const T_OWNER_NS: u64 = T_OWNER_MS * std.time.ns_per_ms;
/// Death records retire after `2 × T_owner`: one TTL for every manager's
/// poll to act, one for the successor's grace window (`AL:L1053-1065`).
pub const DEATH_RECORD_RETIRE_TTLS: u64 = 2;
/// The same-node takeover's attempts: one takeover + one deferred retry,
/// each with a re-read of the record (`AL:L3532`).
pub const SAME_NODE_TAKEOVER_ATTEMPTS: u32 = 4;

// ---------------------------------------------------------------------------
// The store seam (tree 0 of volume 0).
// ---------------------------------------------------------------------------

/// `try_`: ONE control entry that answers `WindowFull` on a full ring-0
/// user window (the verb's `Deferred`); `parking`: parks for the window
/// (the death record's class).
pub const Admission = enum { try_, parking };

pub const StoreError = error{ WindowFull, Io, OutOfMemory };

pub const ControlStore = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        /// A copy of the value (caller frees), or null.
        get: *const fn (*anyopaque, Allocator, key: []const u8) StoreError!?[]u8,
        put: *const fn (*anyopaque, key: []const u8, value: []const u8, Admission) StoreError!void,
        delete: *const fn (*anyopaque, key: []const u8, Admission) StoreError!void,
    };
};

// ---------------------------------------------------------------------------
// Stats (§7.2).
// ---------------------------------------------------------------------------

pub const stats = struct {
    pub var alloc_lease_acquires: counters.ShardedCounter = .{};
    pub var alloc_lease_grants: counters.ShardedCounter = .{};
    pub var alloc_lease_successions: counters.ShardedCounter = .{};
    /// `already` answers (the re-hold's probe, a replayed frame).
    pub var alloc_lease_replays: counters.ShardedCounter = .{};
    /// `Busy` against a live holder.
    pub var alloc_lease_refusals: counters.ShardedCounter = .{};
    /// `LeaseDeferred` (dead, not yet recovered) and a full ring-0 window.
    pub var alloc_lease_deferrals: counters.ShardedCounter = .{};
    pub var alloc_lease_rejected: counters.ShardedCounter = .{};
    pub var alloc_lease_bitmap_publishes: counters.ShardedCounter = .{};
    pub var alloc_lease_releases: counters.ShardedCounter = .{};
    pub var alloc_lease_same_node_takeovers: counters.ShardedCounter = .{};
    pub var dead_members_recorded: counters.ShardedCounter = .{};
    pub var dead_member_records_retired: counters.ShardedCounter = .{};
    pub var recovered_records: counters.ShardedCounter = .{};
    /// Must-stay-0: a death record write that could not land even parking.
    pub var dead_member_write_deferrals: counters.Tripwire = .init("dead_member_write_deferrals");
    /// The highest term this process holds (0 = none).
    pub var alloc_lease_term: counters.Gauge = .{};
};

// ---------------------------------------------------------------------------
// §1.2 The screen and the ladder.
// ---------------------------------------------------------------------------

/// The arm's registered facts the verbs are screened against.
pub const Screen = struct {
    ctx: ?*anyopaque,
    /// The DURABLE block count of a registered data volume, null if the
    /// tag names no data volume.
    dataVolumeBlocks: *const fn (?*anyopaque, vol_tag: u64) ?u64,
    /// The set width (meta volume ordinals below it exist).
    set_width: u16,
    /// Heap geometry per meta volume ordinal (the bitmap refs screen).
    heaps: []const data_bitmap.HeapGeometry,
};

pub const AcquireRequest = struct {
    vol_tag: u64,
    identity: WriterIdentity,
    home_vol: u16,
    appender_id: u32,
    blocks: u64,
};

/// A copy of the record's refs (no allocation: the record caps them).
pub const Refs = struct {
    buf: [records.BITMAP_REFS_MAX]BitmapRef = undefined,
    n: u8 = 0,

    pub fn slice(self: *const Refs) []const BitmapRef {
        return self.buf[0..self.n];
    }

    pub fn from(src: []const BitmapRef) Refs {
        var r: Refs = .{};
        assert(src.len <= records.BITMAP_REFS_MAX);
        for (src, 0..) |s, i| r.buf[i] = s;
        r.n = @intCast(src.len);
        return r;
    }

    pub fn eql(a: *const Refs, b: []const BitmapRef) bool {
        if (a.n != b.len) return false;
        for (a.slice(), b) |x, y| if (x.vol != y.vol or x.start != y.start or x.len != y.len) return false;
        return true;
    }
};

pub const Granted = struct {
    term: u64,
    already: bool,
    blocks: u64,
    /// The pages the record names: the predecessor's at a succession, our
    /// own at a replay, empty at term 1.
    refs: Refs,
};

pub const Rejection = enum { unknown_volume, blocks_out_of_range, home_vol_out_of_set, bad_refs, term_mismatch };

pub const AcquireVerdict = union(enum) {
    granted: Granted,
    /// A live holder: allocate through block grants from it.
    busy,
    /// Dead, not yet recovered — or a full ring-0 window: retry.
    deferred,
    rejected: Rejection,
    /// Not volume 0's manager: the caller re-resolves the venue.
    not_manager,
    io_error,
};

const Lease = struct {
    rec: cr.AllocLease,
    refs: Refs,

    fn holder(self: *const Lease) WriterIdentity {
        return .{ .node_token = self.rec.node_token, .mount_slot = self.rec.mount_slot, .writer_id = self.rec.writer_id };
    }
};

fn readLease(store: ControlStore, gpa: Allocator, vol_tag: u64) StoreError!?Lease {
    const k = cr.allocLeaseKey(vol_tag);
    const bytes = (try store.vtable.get(store.ctx, gpa, k.slice())) orelse return null;
    defer gpa.free(bytes);
    const dec = cr.decodeAllocLease(bytes) catch {
        log.err("alloc_lease:{x}: the record does not decode — refusing to act on it (fsck C13-class corruption of tree 0)", .{vol_tag});
        return error.Io;
    };
    if (dec.count() > records.BITMAP_REFS_MAX) {
        log.err("alloc_lease:{x}: {d} bitmap refs exceed the run cap {d} — refusing to act on it", .{ vol_tag, dec.count(), records.BITMAP_REFS_MAX });
        return error.Io;
    }
    var refs: Refs = .{};
    var i: usize = 0;
    while (i < dec.count()) : (i += 1) {
        refs.buf[i] = dec.bitmapAt(i);
    }
    refs.n = @intCast(dec.count());
    return .{ .rec = dec.lease, .refs = refs };
}

fn writeLease(store: ControlStore, vol_tag: u64, rec: cr.AllocLease, refs: []const BitmapRef, admission: Admission) StoreError!void {
    var buf: [records.ALLOC_LEASE_MAX_LEN]u8 = undefined;
    var r = rec;
    r.bitmap = refs;
    // A ref count the record caps and non-empty refs are the caller's law.
    const enc = cr.encodeAllocLease(r, &buf) catch return error.Io;
    try store.vtable.put(store.ctx, cr.allocLeaseKey(vol_tag).slice(), enc, admission);
}

pub fn isDead(store: ControlStore, gpa: Allocator, id: WriterIdentity) StoreError!bool {
    const k = cr.deadMemberKey(id.stable());
    const v = (try store.vtable.get(store.ctx, gpa, k.slice())) orelse return false;
    gpa.free(v);
    return true;
}

pub fn isRecovered(store: ControlStore, gpa: Allocator, id: WriterIdentity, vol: u16) StoreError!bool {
    const k = cr.recoveredKey(id.stable(), vol);
    const v = (try store.vtable.get(store.ctx, gpa, k.slice())) orelse return false;
    gpa.free(v);
    return true;
}

/// `control_ino` is unused (the shared index's home is the holder's home
/// volume's tree 0, never an ino); written 0, read by nothing.
fn leaseRecord(req: AcquireRequest, blocks: u64, term: u64) cr.AllocLease {
    return .{ .node_token = req.identity.node_token, .mount_slot = req.identity.mount_slot, .holder_appender_id = req.appender_id, .writer_id = req.identity.writer_id, .home_vol = req.home_vol, .control_ino = 0, .blocks = blocks, .term = term, .bitmap = &.{} };
}

fn mapStore(e: StoreError) AcquireVerdict {
    return switch (e) {
        error.WindowFull => blk: {
            stats.alloc_lease_deferrals.inc();
            break :blk .deferred;
        },
        error.Io, error.OutOfMemory => .io_error,
    };
}

/// `AllocLeaseAcquire` (§1.2). `is_vol0_manager` is the serving gate.
pub fn acquire(store: ControlStore, gpa: Allocator, screen: Screen, is_vol0_manager: bool, req: AcquireRequest) AcquireVerdict {
    stats.alloc_lease_acquires.inc();
    if (!is_vol0_manager) return .not_manager;
    // The screen: `vol_tag` a registered data volume; `0 < blocks ≤ known`
    // ("a smaller ask is a smaller bitmap over the same volume"); the home
    // volume inside the set.
    const known = screen.dataVolumeBlocks(screen.ctx, req.vol_tag) orelse return reject(.unknown_volume);
    if (req.blocks == 0 or req.blocks > known) return reject(.blocks_out_of_range);
    if (req.home_vol >= screen.set_width) return reject(.home_vol_out_of_set);

    const prior = readLease(store, gpa, req.vol_tag) catch |e| return mapStore(e);
    if (prior == null) {
        writeLease(store, req.vol_tag, leaseRecord(req, req.blocks, 1), &.{}, .try_) catch |e| return mapStore(e);
        stats.alloc_lease_grants.inc();
        return .{ .granted = .{ .term = 1, .already = false, .blocks = req.blocks, .refs = .{} } };
    }
    const p = prior.?;
    if (p.holder().eql(req.identity)) {
        stats.alloc_lease_replays.inc();
        return .{ .granted = .{ .term = p.rec.term, .already = true, .blocks = p.rec.blocks, .refs = p.refs } };
    }
    const dead = isDead(store, gpa, p.holder()) catch |e| return mapStore(e);
    if (!dead) {
        stats.alloc_lease_refusals.inc();
        return .busy;
    }
    const recovered = isRecovered(store, gpa, p.holder(), p.rec.home_vol) catch |e| return mapStore(e);
    if (!recovered) {
        stats.alloc_lease_deferrals.inc();
        return .deferred;
    }
    // Succession: the record names the predecessor's pages until the
    // successor publishes its copy; `blocks` is the predecessor's geometry.
    writeLease(store, req.vol_tag, leaseRecord(req, p.rec.blocks, p.rec.term + 1), p.refs.slice(), .try_) catch |e| return mapStore(e);
    stats.alloc_lease_grants.inc();
    stats.alloc_lease_successions.inc();
    return .{ .granted = .{ .term = p.rec.term + 1, .already = false, .blocks = p.rec.blocks, .refs = p.refs } };
}

fn reject(why: Rejection) AcquireVerdict {
    stats.alloc_lease_rejected.inc();
    return .{ .rejected = why };
}

// ---------------------------------------------------------------------------
// §1.3 `AllocLeaseBitmap` and `AllocLeaseRelease`.
// ---------------------------------------------------------------------------

pub const BitmapRequest = struct {
    vol_tag: u64,
    identity: WriterIdentity,
    term: u64,
    refs: []const BitmapRef,
};

pub const BitmapVerdict = union(enum) { published, already, busy, rejected: Rejection, deferred, io_error };

/// The holder at `term` publishes its pages: the refs screened against
/// the set's heaps (every ref inside a volume's heap, node-aligned, the
/// total covering the region) — else `Rejected`, nothing persisted; equal
/// refs ⇒ `already`.
pub fn publishBitmap(store: ControlStore, gpa: Allocator, screen: Screen, req: BitmapRequest) BitmapVerdict {
    const prior = (readLease(store, gpa, req.vol_tag) catch |e| return mapBitmapStore(e)) orelse return .busy;
    if (!prior.holder().eql(req.identity) or prior.rec.term != req.term) return .busy;
    if (req.refs.len > records.BITMAP_REFS_MAX or !data_bitmap.screenBitmapRefs(req.refs, prior.rec.blocks, screen.heaps)) {
        stats.alloc_lease_rejected.inc();
        return .{ .rejected = .bad_refs };
    }
    if (prior.refs.eql(req.refs)) {
        stats.alloc_lease_replays.inc();
        return .already;
    }
    writeLease(store, req.vol_tag, prior.rec, req.refs, .try_) catch |e| return mapBitmapStore(e);
    stats.alloc_lease_bitmap_publishes.inc();
    return .published;
}

fn mapBitmapStore(e: StoreError) BitmapVerdict {
    return switch (e) {
        error.WindowFull => blk: {
            stats.alloc_lease_deferrals.inc();
            break :blk .deferred;
        },
        error.Io, error.OutOfMemory => .io_error,
    };
}

pub const ReleaseVerdict = enum { released, already, deferred, io_error };

/// The clean leave: no record, or another holder/term ⇒ `already`
/// (idempotent, a replay); the holder at `term` ⇒ delete.
pub fn release(store: ControlStore, gpa: Allocator, vol_tag: u64, identity: WriterIdentity, term: u64) ReleaseVerdict {
    const prior = (readLease(store, gpa, vol_tag) catch |e| return mapReleaseStore(e)) orelse {
        stats.alloc_lease_replays.inc();
        return .already;
    };
    if (!prior.holder().eql(identity) or prior.rec.term != term) {
        stats.alloc_lease_replays.inc();
        return .already;
    }
    store.vtable.delete(store.ctx, cr.allocLeaseKey(vol_tag).slice(), .try_) catch |e| return mapReleaseStore(e);
    stats.alloc_lease_releases.inc();
    return .released;
}

fn mapReleaseStore(e: StoreError) ReleaseVerdict {
    return switch (e) {
        error.WindowFull => blk: {
            stats.alloc_lease_deferrals.inc();
            break :blk .deferred;
        },
        error.Io, error.OutOfMemory => .io_error,
    };
}

// ---------------------------------------------------------------------------
// §1.4 The death ledger.
// ---------------------------------------------------------------------------

pub const DeathRecord = cr.DeadMember;

/// `RecordDeath` at volume 0's manager: PARKING admission — never a `Try`
/// refusal that loses the death; a write that still fails is the must-
/// stay-0 tripwire (the caller parks it in RAM for the ledger poll's
/// retry). Idempotent: a standing record is kept (the FIRST recorder's
/// clock bounds the retire TTL).
pub fn recordDeath(store: ControlStore, gpa: Allocator, id: WriterIdentity, d: DeathRecord) StoreError!void {
    if (try isDead(store, gpa, id)) return;
    const v = cr.DeadMemberWire.init(d).encode();
    store.vtable.put(store.ctx, cr.deadMemberKey(id.stable()).slice(), &v, .parking) catch |e| {
        stats.dead_member_write_deferrals.fire("a death record could not be written even parking");
        return e;
    };
    stats.dead_members_recorded.inc();
}

pub fn readDeath(store: ControlStore, gpa: Allocator, id: WriterIdentity) StoreError!?cr.DeadMember {
    const k = cr.deadMemberKey(id.stable());
    const v = (try store.vtable.get(store.ctx, gpa, k.slice())) orelse return null;
    defer gpa.free(v);
    return cr.DeadMemberWire.decode(v) catch error.Io;
}

/// `recovered:{id, vol}` after the volume manager's replay applied the
/// dead holder's term's bitmap deltas (PARKING admission).
pub fn recordRecovered(store: ControlStore, gpa: Allocator, id: WriterIdentity, vol: u16, by_term: u64, ts_realtime_ns: u64) StoreError!void {
    if (try isRecovered(store, gpa, id, vol)) return;
    const v = cr.RecoveredWire.init(.{ .by_term = by_term, .ts_realtime_ns = ts_realtime_ns }).encode();
    try store.vtable.put(store.ctx, cr.recoveredKey(id.stable(), vol).slice(), &v, .parking);
    stats.recovered_records.inc();
}

/// Retirement: `dead_member:` AND every `recovered:{member, v}` — a LATER
/// death of the same identity is gated on ITS recovery, never the
/// previous one's.
pub fn retireDeathRecords(store: ControlStore, gpa: Allocator, id: WriterIdentity, set_width: u16) StoreError!void {
    if (!(try isDead(store, gpa, id))) return;
    var v: u16 = 0;
    while (v < set_width) : (v += 1) {
        if (try isRecovered(store, gpa, id, v)) try store.vtable.delete(store.ctx, cr.recoveredKey(id.stable(), v).slice(), .parking);
    }
    try store.vtable.delete(store.ctx, cr.deadMemberKey(id.stable()).slice(), .parking);
    stats.dead_member_records_retired.inc();
}

/// The poll's sweep predicate: age ≥ `2 × T_owner`, no region pending,
/// and no `alloc_lease:` names the member as holder.
pub fn deathRecordRetirable(recorded_realtime_ns: u64, now_realtime_ns: u64, region_pending: bool, named_as_holder: bool) bool {
    if (region_pending or named_as_holder) return false;
    return now_realtime_ns -| recorded_realtime_ns >= DEATH_RECORD_RETIRE_TTLS * T_OWNER_NS;
}

/// Does any `alloc_lease:` record over `vol_tags` name `id` as holder?
pub fn namedAsHolder(store: ControlStore, gpa: Allocator, id: WriterIdentity, vol_tags: []const u64) StoreError!bool {
    for (vol_tags) |t| {
        const l = (try readLease(store, gpa, t)) orelse continue;
        if (l.holder().eql(id)) return true;
    }
    return false;
}

// ---------------------------------------------------------------------------
// §1.4 The same-node takeover (`acquire_and_hold`).
// ---------------------------------------------------------------------------

/// "Page 0 at open was owned by this node AND named that mount slot": the
/// D0 flock this open holds proves the recorded holder dead.
pub const D0Proof = struct {
    ctx: ?*anyopaque,
    provesDead: *const fn (?*anyopaque, holder: WriterIdentity) bool,
};

/// The arm's recovery of a same-node predecessor: replay the window onto
/// the predecessor's pages, then `recovered:` (the caller's volume
/// manager duty, bound here so the takeover is one function).
pub const Recover = struct {
    ctx: ?*anyopaque,
    recover: *const fn (?*anyopaque, dead: WriterIdentity, home_vol: u16, term: u64) bool,
};

pub const HoldOutcome = union(enum) {
    granted: Granted,
    /// A live holder (or a same-node JOINED appender): never taken over.
    busy,
    /// A FOREIGN dead holder awaiting its home manager: returned loud.
    deferred,
    rejected: Rejection,
    attempts_exhausted,
    not_manager,
    io_error,
};

/// On `Busy`, iff the D0 proof names the recorded holder dead (a prior
/// incarnation of THIS mount point), the arm records the death itself
/// (the EARLY class), recovers, and retries — ≤ 4 attempts. A foreign dead
/// holder's `LeaseDeferred` is returned loud: the arm does not wait for a
/// foreign recovery.
pub fn acquireAndHold(store: ControlStore, gpa: Allocator, screen: Screen, is_vol0_manager: bool, req: AcquireRequest, d0: D0Proof, recover: Recover, now_realtime_ns: u64) HoldOutcome {
    var attempt: u32 = 0;
    while (attempt < SAME_NODE_TAKEOVER_ATTEMPTS) : (attempt += 1) {
        switch (acquire(store, gpa, screen, is_vol0_manager, req)) {
            .granted => |g| {
                // The member's own rejoin retires its mount's death records
                // (`retire_own_death_records`): the ledger keys a death by
                // `{node_token, mount_slot}`, so a record left standing
                // would have the next poll recover a LIVE region and gate a
                // LATER death of this identity on the previous recovery.
                retireDeathRecords(store, gpa, req.identity, screen.set_width) catch return .io_error;
                return .{ .granted = g };
            },
            .rejected => |r| return .{ .rejected = r },
            .not_manager => return .not_manager,
            .io_error => return .io_error,
            .busy => {
                const prior = (readLease(store, gpa, req.vol_tag) catch return .io_error) orelse continue;
                const holder = prior.holder();
                // Only a prior incarnation of THIS mount point is ours to
                // take over, and only when the D0 flock proves it dead; a
                // same-node holder at another slot is a JOINED appender.
                if (!holder.sameMount(req.identity) or !d0.provesDead(d0.ctx, holder)) return .busy;
                recordDeath(store, gpa, holder, .{ .epoch = 0, .ts_realtime_ns = now_realtime_ns, .pr_key = 0, .early = true }) catch return .io_error;
                if (!recover.recover(recover.ctx, holder, prior.rec.home_vol, prior.rec.term)) return .io_error;
                recordRecovered(store, gpa, holder, prior.rec.home_vol, prior.rec.term, now_realtime_ns) catch return .io_error;
                stats.alloc_lease_same_node_takeovers.inc();
            },
            .deferred => {
                const prior = (readLease(store, gpa, req.vol_tag) catch return .io_error) orelse continue;
                // A foreign dead holder's recovery is its home manager's:
                // loud, never awaited. Our own predecessor (recorded dead by
                // a peer before this open) is recovered by this arm.
                const holder = prior.holder();
                if (!holder.sameMount(req.identity)) return .deferred;
                if (!recover.recover(recover.ctx, holder, prior.rec.home_vol, prior.rec.term)) return .io_error;
                recordRecovered(store, gpa, holder, prior.rec.home_vol, prior.rec.term, now_realtime_ns) catch return .io_error;
                stats.alloc_lease_same_node_takeovers.inc();
            },
        }
    }
    return .attempts_exhausted;
}

// ---------------------------------------------------------------------------
// §1.5 The holding — the allocator's `AllocationLease` seam.
// ---------------------------------------------------------------------------

/// What the lease arm installs on an allocator: a HOLDER reads its own
/// bitmap's population and the window it minted from; a JOINER keeps the
/// holder's last word (the population rides every `BlocksGranted` reply,
/// 0002 row 241) with its own window.
pub const LeaseArm = struct {
    /// The holder's bitmap when THIS process holds the lease.
    bitmap: ?*data_bitmap.DataBitmap = null,
    /// A joiner's last word from the holder.
    joiner_blocks: Atomic(u64) = .init(0),
    joiner_population: Atomic(u64) = .init(0),
    joiner_frontier: Atomic(u64) = .init(0),
    /// The writer's unconsumed window (the grant window's `remaining`).
    window_remaining: ?*const fn (?*anyopaque) u64 = null,
    window_ctx: ?*anyopaque = null,
    /// The mount seed handed over ONCE (the arm's durable truth).
    seed_blocks: u64 = 0,
    seed_set_words: u64 = 0,
    seed_noted: bool = false,
    term: u64 = 0,

    pub fn holderView(self: *LeaseArm) ?block_alloc.HolderView {
        const remaining: u64 = if (self.window_remaining) |f| f(self.window_ctx) else 0;
        if (self.bitmap) |bm| return .{ .bitmap_blocks = bm.blocks(), .population = bm.populationNow(), .window_remaining = remaining, .frontier = bm.highestSetPlusOne() };
        const blocks = self.joiner_blocks.load(.acquire);
        if (blocks == 0) return null;
        return .{ .bitmap_blocks = blocks, .population = self.joiner_population.load(.acquire), .window_remaining = remaining, .frontier = self.joiner_frontier.load(.acquire) };
    }

    /// A joiner's word from the holder (carried on every grant reply).
    pub fn noteHolderWord(self: *LeaseArm, blocks: u64, population: u64, frontier: u64) void {
        self.joiner_blocks.store(blocks, .release);
        self.joiner_population.store(population, .release);
        self.joiner_frontier.store(frontier, .release);
    }

    pub fn holdsLease(self: *const LeaseArm) bool {
        return self.bitmap != null;
    }

    fn viewFn(ctx: *anyopaque) ?block_alloc.HolderView {
        const self: *LeaseArm = @ptrCast(@alignCast(ctx));
        return self.holderView();
    }

    fn seedFn(ctx: *anyopaque, _: u64, blocks: u64, set_words: []const u64) void {
        const self: *LeaseArm = @ptrCast(@alignCast(ctx));
        self.seed_blocks = blocks;
        var n: u64 = 0;
        for (set_words) |w| n += @popCount(w);
        self.seed_set_words = n;
        self.seed_noted = true;
    }

    const vtable: block_alloc.AllocationLease.VTable = .{ .holderView = &viewFn, .noteMountSeed = &seedFn };

    pub fn lease(self: *LeaseArm) block_alloc.AllocationLease {
        return .{ .ctx = self, .vtable = &vtable };
    }
};

// ---------------------------------------------------------------------------
// §1.6 The arm's verdicts (pure).
// ---------------------------------------------------------------------------

/// `derived.highest == 0 ∧ !durable.is_empty()` refuses the mount: every
/// live block would be re-granted.
pub fn unseededRefusal(derived_highest: u64, durable_population: u64) bool {
    return derived_highest == 0 and durable_population != 0;
}

pub const SecondSnapshotVerdict = union(enum) {
    clean,
    /// The cursor moved or a free-listed block minted during the arm.
    allocated_during_arm,
    /// A seed block the allocator now free-lists: a FREE the arm absorbs.
    absorb_free: u64,
    /// A block the allocator holds LIVE reads CLEAR in the recovered
    /// bitmap: the LOSS direction, refused loud.
    loss: u64,
};

/// Judge the second snapshot by DIRECTION against the first (both bitsets
/// over `[0, highest)`), with `live(idx)` = "off the list, not in flight,
/// not graced, not quarantined" and `recovered_set(idx)` the bitmap.
pub fn judgeSecondSnapshot(first_highest: u64, second_highest: u64, first: []const u64, second: []const u64, ctx: anytype, comptime live: fn (@TypeOf(ctx), u64) bool, comptime recovered_set: fn (@TypeOf(ctx), u64) bool) SecondSnapshotVerdict {
    if (second_highest != first_highest) return .allocated_during_arm;
    var idx: u64 = 0;
    while (idx < first_highest) : (idx += 1) {
        const w: usize = @intCast(idx / 64);
        const m = @as(u64, 1) << @intCast(idx % 64);
        const was_set = first[w] & m != 0;
        const is_set = second[w] & m != 0;
        if (!was_set and is_set) return .allocated_during_arm;
        if (was_set and !is_set) return .{ .absorb_free = idx };
        if (is_set and live(ctx, idx) and !recovered_set(ctx, idx)) return .{ .loss = idx };
    }
    return .clean;
}

// ---------------------------------------------------------------------------
// A RAM tree 0 (the portable contracts' store).
// ---------------------------------------------------------------------------

pub const RamControlStore = struct {
    gpa: Allocator,
    map: std.StringHashMapUnmanaged([]u8) = .empty,
    /// The next N `try_` writes answer `WindowFull` (a full ring-0 window).
    refuse_try_next: u32 = 0,
    puts: u64 = 0,

    pub fn init(gpa: Allocator) RamControlStore {
        return .{ .gpa = gpa };
    }

    pub fn deinit(self: *RamControlStore) void {
        var it = self.map.iterator();
        while (it.next()) |kv| {
            self.gpa.free(kv.key_ptr.*);
            self.gpa.free(kv.value_ptr.*);
        }
        self.map.deinit(self.gpa);
    }

    pub fn count(self: *const RamControlStore) usize {
        return self.map.count();
    }

    fn get(ctx: *anyopaque, gpa: Allocator, k: []const u8) StoreError!?[]u8 {
        const self: *RamControlStore = @ptrCast(@alignCast(ctx));
        const v = self.map.get(k) orelse return null;
        return try gpa.dupe(u8, v);
    }

    fn put(ctx: *anyopaque, k: []const u8, v: []const u8, admission: Admission) StoreError!void {
        const self: *RamControlStore = @ptrCast(@alignCast(ctx));
        if (admission == .try_ and self.refuse_try_next > 0) {
            self.refuse_try_next -= 1;
            return error.WindowFull;
        }
        const vd = try self.gpa.dupe(u8, v);
        errdefer self.gpa.free(vd);
        const gop = try self.map.getOrPut(self.gpa, k);
        if (gop.found_existing) {
            self.gpa.free(gop.value_ptr.*);
        } else {
            gop.key_ptr.* = self.gpa.dupe(u8, k) catch |e| {
                _ = self.map.remove(k);
                return e;
            };
        }
        gop.value_ptr.* = vd;
        self.puts += 1;
    }

    fn delete(ctx: *anyopaque, k: []const u8, admission: Admission) StoreError!void {
        const self: *RamControlStore = @ptrCast(@alignCast(ctx));
        if (admission == .try_ and self.refuse_try_next > 0) {
            self.refuse_try_next -= 1;
            return error.WindowFull;
        }
        if (self.map.fetchRemove(k)) |kv| {
            self.gpa.free(kv.key);
            self.gpa.free(kv.value);
        }
    }

    const vtable: ControlStore.VTable = .{ .get = &get, .put = &put, .delete = &delete };

    pub fn store(self: *RamControlStore) ControlStore {
        return .{ .ctx = self, .vtable = &vtable };
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

const test_heaps = [_]data_bitmap.HeapGeometry{ .{ .vol = 0, .heap_start = 0, .heap_len = 1 << 24, .node_size = 8192 }, .{ .vol = 1, .heap_start = 0, .heap_len = 1 << 24, .node_size = 8192 } };

fn knownBlocks(_: ?*anyopaque, vol_tag: u64) ?u64 {
    return if (vol_tag == 0xD1 or vol_tag == 0xD2) 1024 else null;
}

const test_screen: Screen = .{ .ctx = null, .dataVolumeBlocks = &knownBlocks, .set_width = 2, .heaps = &test_heaps };

const A: WriterIdentity = .{ .node_token = 1, .mount_slot = 0, .writer_id = 0xA };
const B: WriterIdentity = .{ .node_token = 2, .mount_slot = 0, .writer_id = 0xB };
/// A's successor incarnation at the same mount point.
const A2: WriterIdentity = .{ .node_token = 1, .mount_slot = 0, .writer_id = 0xA2 };

fn acqReq(id: WriterIdentity, home: u16) AcquireRequest {
    return .{ .vol_tag = 0xD1, .identity = id, .home_vol = home, .appender_id = 3, .blocks = 1024 };
}

test "the_alloc_lease_record_round_trips_and_a_release_is_idempotent: term 1 first-come, already for the holder, Busy for a live peer, release only by the holder at its term" {
    var ram = RamControlStore.init(testing.allocator);
    defer ram.deinit();
    const st = ram.store();
    const g = acquire(st, testing.allocator, test_screen, true, acqReq(A, 0)).granted;
    try testing.expectEqual(@as(u64, 1), g.term);
    try testing.expect(!g.already);
    try testing.expectEqual(@as(u8, 0), g.refs.n);
    const again = acquire(st, testing.allocator, test_screen, true, acqReq(A, 0)).granted;
    try testing.expect(again.already and again.term == 1);
    try testing.expectEqual(AcquireVerdict.busy, acquire(st, testing.allocator, test_screen, true, acqReq(B, 1)));
    try testing.expectEqual(AcquireVerdict.not_manager, acquire(st, testing.allocator, test_screen, false, acqReq(B, 1)));
    // Release by the wrong holder / term is `already`; by the holder deletes.
    try testing.expectEqual(ReleaseVerdict.already, release(st, testing.allocator, 0xD1, B, 1));
    try testing.expectEqual(ReleaseVerdict.already, release(st, testing.allocator, 0xD1, A, 2));
    try testing.expectEqual(ReleaseVerdict.released, release(st, testing.allocator, 0xD1, A, 1));
    try testing.expectEqual(ReleaseVerdict.already, release(st, testing.allocator, 0xD1, A, 1));
    // B now takes term 1 fresh (a release does not bump the term).
    const gb = acquire(st, testing.allocator, test_screen, true, acqReq(B, 1)).granted;
    try testing.expectEqual(@as(u64, 1), gb.term);
}

test "the screen refuses before anything is written: an unknown volume, blocks outside (0, known], a home volume outside the set" {
    var ram = RamControlStore.init(testing.allocator);
    defer ram.deinit();
    const st = ram.store();
    var r = acqReq(A, 0);
    r.vol_tag = 0xBAD;
    try testing.expectEqual(AcquireVerdict{ .rejected = .unknown_volume }, acquire(st, testing.allocator, test_screen, true, r));
    r = acqReq(A, 0);
    r.blocks = 0;
    try testing.expectEqual(AcquireVerdict{ .rejected = .blocks_out_of_range }, acquire(st, testing.allocator, test_screen, true, r));
    r.blocks = 1025;
    try testing.expectEqual(AcquireVerdict{ .rejected = .blocks_out_of_range }, acquire(st, testing.allocator, test_screen, true, r));
    r = acqReq(A, 2);
    try testing.expectEqual(AcquireVerdict{ .rejected = .home_vol_out_of_set }, acquire(st, testing.allocator, test_screen, true, r));
    try testing.expectEqual(@as(usize, 0), ram.count());
    // A smaller ask is a smaller bitmap over the same volume.
    r = acqReq(A, 0);
    r.blocks = 512;
    try testing.expectEqual(@as(u64, 512), acquire(st, testing.allocator, test_screen, true, r).granted.blocks);
}

test "an_allocation_lease_successor_never_regrants_a_referenced_block — the ladder: a dead holder is LeaseDeferred until recovered:{holder, home_vol}, then term + 1 naming the predecessor's pages" {
    var ram = RamControlStore.init(testing.allocator);
    defer ram.deinit();
    const st = ram.store();
    _ = acquire(st, testing.allocator, test_screen, true, acqReq(A, 1)).granted;
    const refs = [_]BitmapRef{.{ .vol = 1, .start = 0, .len = 8192 }};
    try testing.expectEqual(BitmapVerdict.published, publishBitmap(st, testing.allocator, test_screen, .{ .vol_tag = 0xD1, .identity = A, .term = 1, .refs = &refs }));
    try testing.expectEqual(BitmapVerdict.already, publishBitmap(st, testing.allocator, test_screen, .{ .vol_tag = 0xD1, .identity = A, .term = 1, .refs = &refs }));
    try testing.expectEqual(BitmapVerdict.busy, publishBitmap(st, testing.allocator, test_screen, .{ .vol_tag = 0xD1, .identity = B, .term = 1, .refs = &refs }));
    try testing.expectEqual(BitmapVerdict.busy, publishBitmap(st, testing.allocator, test_screen, .{ .vol_tag = 0xD1, .identity = A, .term = 2, .refs = &refs }));
    const bad = [_]BitmapRef{.{ .vol = 1, .start = 4096, .len = 8192 }};
    try testing.expectEqual(BitmapVerdict{ .rejected = .bad_refs }, publishBitmap(st, testing.allocator, test_screen, .{ .vol_tag = 0xD1, .identity = A, .term = 1, .refs = &bad }));
    // B is Busy while A is live.
    try testing.expectEqual(AcquireVerdict.busy, acquire(st, testing.allocator, test_screen, true, acqReq(B, 0)));
    // A dies: recorded, not yet recovered ⇒ LeaseDeferred.
    const recorded0 = stats.dead_members_recorded.load();
    try recordDeath(st, testing.allocator, A, .{ .epoch = 7, .ts_realtime_ns = 1000, .pr_key = 0x11, .early = false });
    try recordDeath(st, testing.allocator, A, .{ .epoch = 8, .ts_realtime_ns = 2000, .pr_key = 0x11, .early = false }); // idempotent: the first clock stands
    try testing.expectEqual(recorded0 + 1, stats.dead_members_recorded.load());
    try testing.expectEqual(@as(u64, 1000), (try readDeath(st, testing.allocator, A)).?.ts_realtime_ns);
    const deferred0 = stats.alloc_lease_deferrals.load();
    try testing.expectEqual(AcquireVerdict.deferred, acquire(st, testing.allocator, test_screen, true, acqReq(B, 0)));
    try testing.expectEqual(deferred0 + 1, stats.alloc_lease_deferrals.load());
    // Recovered on the WRONG volume is not enough: the gate is the holder's home_vol.
    try recordRecovered(st, testing.allocator, A, 0, 1, 3000);
    try testing.expectEqual(AcquireVerdict.deferred, acquire(st, testing.allocator, test_screen, true, acqReq(B, 0)));
    try recordRecovered(st, testing.allocator, A, 1, 1, 3000);
    const g = acquire(st, testing.allocator, test_screen, true, acqReq(B, 0)).granted;
    try testing.expectEqual(@as(u64, 2), g.term);
    try testing.expect(!g.already);
    try testing.expectEqual(@as(u64, 1024), g.blocks);
    try testing.expect(g.refs.eql(&refs)); // the predecessor's pages until B publishes its copy
    try testing.expect(try namedAsHolder(st, testing.allocator, B, &.{0xD1}));
    try testing.expect(!(try namedAsHolder(st, testing.allocator, A, &.{0xD1})));
    // B's re-acquire is a replay naming the same (predecessor) refs; its
    // own publish moves them.
    try testing.expect(acquire(st, testing.allocator, test_screen, true, acqReq(B, 0)).granted.already);
    const mine = [_]BitmapRef{.{ .vol = 0, .start = 8192, .len = 8192 }};
    try testing.expectEqual(BitmapVerdict.published, publishBitmap(st, testing.allocator, test_screen, .{ .vol_tag = 0xD1, .identity = B, .term = 2, .refs = &mine }));
    try testing.expect(acquire(st, testing.allocator, test_screen, true, acqReq(B, 0)).granted.refs.eql(&mine));
    // Retirement deletes the death AND every recovered row; A's rejoin takes a fresh term-less path (Busy against B).
    const retired0 = stats.dead_member_records_retired.load();
    try retireDeathRecords(st, testing.allocator, A, 2);
    try testing.expectEqual(retired0 + 1, stats.dead_member_records_retired.load());
    try testing.expect(!(try isDead(st, testing.allocator, A)));
    try testing.expect(!(try isRecovered(st, testing.allocator, A, 1)));
    try testing.expectEqual(AcquireVerdict.busy, acquire(st, testing.allocator, test_screen, true, acqReq(A, 1)));
    try testing.expect(deathRecordRetirable(1000, 1000 + 2 * T_OWNER_NS, false, false));
    try testing.expect(!deathRecordRetirable(1000, 1000 + 2 * T_OWNER_NS - 1, false, false));
    try testing.expect(!deathRecordRetirable(0, 1 << 40, true, false));
    try testing.expect(!deathRecordRetirable(0, 1 << 40, false, true));
}

test "a full ring-0 window is the typed Deferred on every Try-admitted write and never a parked handler; the death record parks" {
    var ram = RamControlStore.init(testing.allocator);
    defer ram.deinit();
    const st = ram.store();
    ram.refuse_try_next = 1;
    try testing.expectEqual(AcquireVerdict.deferred, acquire(st, testing.allocator, test_screen, true, acqReq(A, 0)));
    try testing.expectEqual(@as(usize, 0), ram.count());
    _ = acquire(st, testing.allocator, test_screen, true, acqReq(A, 0)).granted;
    ram.refuse_try_next = 1;
    const refs = [_]BitmapRef{.{ .vol = 0, .start = 0, .len = 8192 }};
    try testing.expectEqual(BitmapVerdict.deferred, publishBitmap(st, testing.allocator, test_screen, .{ .vol_tag = 0xD1, .identity = A, .term = 1, .refs = &refs }));
    ram.refuse_try_next = 1;
    try testing.expectEqual(ReleaseVerdict.deferred, release(st, testing.allocator, 0xD1, A, 1));
    ram.refuse_try_next = 1;
    try recordDeath(st, testing.allocator, B, .{ .epoch = 1, .ts_realtime_ns = 1, .pr_key = 0, .early = false }); // parking: lands
    try testing.expect(try isDead(st, testing.allocator, B));
    try testing.expectEqual(@as(u32, 1), ram.refuse_try_next); // the Try budget untouched
}

const TakeoverFixture = struct {
    recovered: u32 = 0,
    proves: bool = true,
    fn provesDead(ctx: ?*anyopaque, _: WriterIdentity) bool {
        const f: *TakeoverFixture = @ptrCast(@alignCast(ctx.?));
        return f.proves;
    }
    fn recover(ctx: ?*anyopaque, _: WriterIdentity, _: u16, _: u64) bool {
        const f: *TakeoverFixture = @ptrCast(@alignCast(ctx.?));
        f.recovered += 1;
        return true;
    }
};

test "a_clean_leave_then_a_mount_at_another_mount_point_of_the_same_host_arms / the same-node takeover: the D0 flock proves the predecessor dead, the arm records the EARLY death, recovers, and succeeds; a foreign dead holder is deferred loud; a joined appender is never taken over" {
    var ram = RamControlStore.init(testing.allocator);
    defer ram.deinit();
    const st = ram.store();
    _ = acquire(st, testing.allocator, test_screen, true, acqReq(A, 0)).granted;
    var f: TakeoverFixture = .{};
    const d0: D0Proof = .{ .ctx = &f, .provesDead = &TakeoverFixture.provesDead };
    const rec: Recover = .{ .ctx = &f, .recover = &TakeoverFixture.recover };
    const takeovers0 = stats.alloc_lease_same_node_takeovers.load();
    const out = acquireAndHold(st, testing.allocator, test_screen, true, acqReq(A2, 0), d0, rec, 5000);
    try testing.expectEqual(@as(u64, 2), out.granted.term);
    try testing.expectEqual(@as(u32, 1), f.recovered);
    try testing.expectEqual(takeovers0 + 1, stats.alloc_lease_same_node_takeovers.load());
    // The rejoin retired its own mount's death records (the ledger keys a
    // death by the mount, so A2 would otherwise read as dead).
    try testing.expect(!(try isDead(st, testing.allocator, A)));
    try testing.expect(!(try isRecovered(st, testing.allocator, A, 0)));
    // B (foreign) against the live A2: Busy, never taken over (another
    // mount's holder is never ours whatever the flock says).
    try testing.expectEqual(HoldOutcome.busy, acquireAndHold(st, testing.allocator, test_screen, true, acqReq(B, 1), d0, rec, 6000));
    // A same-node successor whose flock does NOT prove the holder dead (a
    // joined appender at this mount point is live): Busy.
    f.proves = false;
    try testing.expectEqual(HoldOutcome.busy, acquireAndHold(st, testing.allocator, test_screen, true, acqReq(.{ .node_token = 1, .mount_slot = 0, .writer_id = 0xA9 }, 0), d0, rec, 6000));
    f.proves = true;
    // A2 dies (recorded by a peer, not yet recovered): B's arm does not
    // wait for the foreign recovery — deferred loud.
    try recordDeath(st, testing.allocator, A2, .{ .epoch = 2, .ts_realtime_ns = 7000, .pr_key = 0, .early = false });
    try testing.expectEqual(HoldOutcome.deferred, acquireAndHold(st, testing.allocator, test_screen, true, acqReq(B, 1), d0, rec, 7000));
    // The same-node successor of A2 (A3) finds its own predecessor dead-but-
    // unrecovered: the arm's own recovery completes it within the attempts.
    const A3: WriterIdentity = .{ .node_token = 1, .mount_slot = 0, .writer_id = 0xA3 };
    const out3 = acquireAndHold(st, testing.allocator, test_screen, true, acqReq(A3, 0), d0, rec, 8000);
    try testing.expectEqual(@as(u64, 3), out3.granted.term);
    try testing.expectEqual(@as(u32, 2), f.recovered);
}

test "the lease arm fills the allocator's HolderView: a holder reads its bitmap, a joiner the holder's last word; the seed is noted once; df = population − own window" {
    const a = try block_alloc.BlockAllocator.init(testing.allocator, .{ .vol_tag = 0xD1, .vol = 0, .block_size = 4096, .capacity_bytes = 128 * 4096 });
    defer a.deinit();
    var i: u64 = 0;
    while (i < 8) : (i += 1) _ = try a.allocateBlock();
    const bm = try data_bitmap.DataBitmap.init(testing.allocator, a, .{ .vol_tag = 0xD1, .term = 1, .blocks = 128, .ring_capacity = 64 });
    defer bm.deinit();
    var arm: LeaseArm = .{ .bitmap = bm, .term = 1 };
    const Win = struct {
        fn remaining(_: ?*anyopaque) u64 {
            return 3;
        }
    };
    arm.window_remaining = &Win.remaining;
    a.lease = arm.lease();
    _ = bm.carveRun(8, 4).?; // 8..11 granted (3 unconsumed by the fixture's word)
    const v = arm.holderView().?;
    try testing.expectEqual(@as(u64, 128), v.bitmap_blocks);
    try testing.expectEqual(@as(u64, 12), v.population);
    try testing.expectEqual(@as(u64, 3), v.window_remaining);
    try testing.expectEqual(@as(u64, 12), v.frontier);
    try testing.expectEqual(@as(u64, 9), a.usedBlocks()); // population − own unconsumed window
    // A joiner's arm: no view until the holder's first word.
    var joiner: LeaseArm = .{};
    try testing.expect(joiner.holderView() == null);
    joiner.noteHolderWord(128, 40, 50);
    const jv = joiner.holderView().?;
    try testing.expectEqual(@as(u64, 40), jv.population);
    try testing.expectEqual(@as(u64, 0), jv.window_remaining);
    try testing.expect(!joiner.holdsLease() and arm.holdsLease());
    // The seed is handed over once through the allocator's seam.
    var idxs = [_]u64{ 20, 21 };
    a.seedFromDurableRefs(&idxs);
    try testing.expect(arm.seed_noted);
    try testing.expectEqual(@as(u64, 22), arm.seed_blocks);
}

test "the arm's verdicts: the un-seeded refusal; the second snapshot judged by direction — a mint during the arm refuses, a free is absorbed, a live block reading clear is the LOSS" {
    try testing.expect(unseededRefusal(0, 5));
    try testing.expect(!unseededRefusal(0, 0));
    try testing.expect(!unseededRefusal(3, 5));
    const Ctx = struct {
        live_mask: u64,
        recovered_mask: u64,
        fn live(c: @This(), idx: u64) bool {
            return c.live_mask & (@as(u64, 1) << @intCast(idx)) != 0;
        }
        fn rec(c: @This(), idx: u64) bool {
            return c.recovered_mask & (@as(u64, 1) << @intCast(idx)) != 0;
        }
    };
    const first = [_]u64{0b0111};
    const same = [_]u64{0b0111};
    const minted = [_]u64{0b1111};
    const freed = [_]u64{0b0101};
    const c_ok: Ctx = .{ .live_mask = 0b0111, .recovered_mask = 0b0111 };
    try testing.expectEqual(SecondSnapshotVerdict.clean, judgeSecondSnapshot(3, 3, &first, &same, c_ok, Ctx.live, Ctx.rec));
    try testing.expectEqual(SecondSnapshotVerdict.allocated_during_arm, judgeSecondSnapshot(3, 4, &first, &same, c_ok, Ctx.live, Ctx.rec));
    try testing.expectEqual(SecondSnapshotVerdict.allocated_during_arm, judgeSecondSnapshot(4, 4, &first, &minted, c_ok, Ctx.live, Ctx.rec));
    try testing.expectEqual(SecondSnapshotVerdict{ .absorb_free = 1 }, judgeSecondSnapshot(3, 3, &first, &freed, c_ok, Ctx.live, Ctx.rec));
    const c_loss: Ctx = .{ .live_mask = 0b0111, .recovered_mask = 0b0011 };
    try testing.expectEqual(SecondSnapshotVerdict{ .loss = 2 }, judgeSecondSnapshot(3, 3, &first, &same, c_loss, Ctx.live, Ctx.rec));
    // A set-but-not-live block (in flight / graced) reading clear is not a loss.
    const c_inflight: Ctx = .{ .live_mask = 0b0011, .recovered_mask = 0b0011 };
    try testing.expectEqual(SecondSnapshotVerdict.clean, judgeSecondSnapshot(3, 3, &first, &same, c_inflight, Ctx.live, Ctx.rec));
}
