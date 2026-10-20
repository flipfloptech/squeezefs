//! The per-inode FUSE op guard — lock order level (1) (charter §2 "Lock
//! order", §6.2 "Shared-mode inode admission"; `docs/porting/
//! fuse_handlers_lanes.md` §2; the reference's `active_inode_locks` +
//! `inode_write_lock_scope` + `design-write-inode-convoy.md`).
//!
//! Structure: a fixed power-of-two table of `sync.RwLock` indexed by
//! `splitmix64(ino) & mask` (the ONE mix the 4a tables use, `dlm.
//! splitmix64`) — zero allocation after construction, one mask never a
//! division. Two inos may share a stripe: a collision WIDENS a critical
//! section, never under-locks. `INODE_GUARD_STRIPES = 4096` is the shipped
//! width and a never-regress floor; unlike the 4a classes it is NOT derived
//! from `cpus × q_depth` — a level-1 hold is a DATA-PATH hold (a cache-hit
//! meta-prep measured 82 µs, an inline RMW), never a commit park, so a
//! collision costs microseconds, not a conveyor round trip. The table
//! carries the stripe census the reference wired for every other striped
//! table but not this one (ledger L11): `inode_guard_{stripes,stripe_
//! collisions,key_waits}` so the load-factor argument can be checked here
//! too.
//!
//! Three classes and ONE classifier (`classify`, verbatim §2.2):
//!
//!   EntireOp     — exclusive, whole op: inline RMW / layout transition /
//!                  staged→striped (every rung-3 write: inline is the only
//!                  layout until rung 4).
//!   MetaPrepOnly — exclusive, dropped BEFORE data I/O: striped shapes
//!                  publishing size/map.
//!   Shared       — the READ guard, dropped at the SAME point (KD-1): a
//!                  cache-resident striped write whose range the resident
//!                  caches prove within-EOF (`end ≤ min(attr size, meta
//!                  size)`), or any resident striped write under the W-2
//!                  narrow class — every inode-plane mutation such a write
//!                  performs runs AFTER the drop point under (3) + (3.5),
//!                  so "the exclusive meta-prep protected nothing the read
//!                  guard does not" (design §4.4 row 15).
//!
//! The admission protocol (`admitWrite`, §2.4 step by step): validate the
//! range [no locks] → probe the caches WITHOUT awaiting [no locks] →
//! classify → acquire AS CLASSIFIED → revalidate under the guard (Shared
//! arm: re-probe on a fresh snapshot) → at most ONE upgrade to exclusive
//! (never a loop, never a downgrade; counted `write_lock_scope_shared_
//! upgrades`) → the handler resolves `(old_size, file_type)` (the Shared
//! snapshot: NO fetch under Shared; the exclusive arm may fetch) and
//! `finalize`s the scope → the guard drops BEFORE data I/O (KD-1: holding
//! across the DMA window would block truncate/fallocate for full device
//! windows — a new convoy — and make any nested inode-lock acquisition a
//! self-deadlock). Must-not: a Shared or MetaPrepOnly holder never
//! re-enters this table on the same ino. KD-7: the lifecycle boundary
//! (cold/no-open writes, the orphan probe) is exclusive BY CONSTRUCTION —
//! Shared requires both caches resident. The probe is a VTABLE the data
//! plane fills at rung 4; rung 3's `Probe.none` answers "miss" for every
//! ino, so every write classifies EntireOp.
//!
//! Write-preferring is the primitive's law (`sync.RwLock`: a fresh shared
//! attempt yields to ANY queued exclusive waiter), so "a waiting truncate
//! excludes newly-arriving Shared writers — no starvation, no upgrade
//! livelock"; a rand-4k storm cannot starve setattr-size.
//!
//! Two-ino order (`acquirePair`): ascending SHARD INDEX, raw ino tie-break
//! — two distinct ino pairs can map onto the same two stripes in opposite
//! raw-ino order (the ABBA the VL8 item-2 wedge capture implicated); equal
//! shards collapse to a single exclusive guard.
//!
//! Measured (design §1, §3; `fuse_client.rs:4807–4810`): the convoy was
//! ≈ 1.12 ms/op of `write_lock_wait` under the exclusive guard at 32 ×
//! qd32 while the data devices idled at qd ≈ 1–1.5 (the per-inode interval
//! was 82 µs = wake + cache-hit meta-prep); Shared ON vs OFF, A-B-B-A both
//! orders: 523.3–523.4 k / 529.6–530.3 k vs 489.4–490.1 k / 491.3–492.0 k
//! IOPS (kern/il), +7–8 % order-independent. `SQUEEZEFS_WRITE_SHARED=0` is
//! the one A/B lever (the exclusive control arm); the superseded v1
//! mapped-only class and its lever are not carried (L7).

const std = @import("std");
const Io = std.Io;
const assert = std.debug.assert;
const rwlock = @import("../../sync/rwlock.zig");
const lock_order = @import("../../sync/lock_order.zig");
const dlm = @import("../../meta/dlm.zig");
const counters = @import("../../core/counters.zig");
const clock = @import("../../core/clock.zig");
const knobs = @import("../../core/knobs.zig");
const errors = @import("../../core/errors.zig");
const lease_epoch = @import("../../data/lease_epoch.zig");
const route_table = @import("../../meta/routing/route_table.zig");

pub const Mode = rwlock.Mode;

/// The shipped width (module doc).
pub const INODE_GUARD_STRIPES: usize = 4096;

pub const Class = enum(u8) {
    entire_op,
    meta_prep_only,
    shared,
};

/// The layout class the classifier dispatches on.
pub const FileType = enum(u8) {
    /// The payload IS the layout record (rung 3's only layout).
    inline_,
    /// The whole payload in the local staging ring (rung 4).
    staged,
    /// 4 MiB blocks across the data volumes (rung 4).
    striped,
    /// Nothing resident: never a fetch here, never an error (KD-2).
    unknown,
};

pub const ClassifyInput = struct {
    file_type: FileType,
    /// `max(old_size, end)`.
    expected_new_size: u64,
    block_size: u64,
    /// `min(metadata_cache.size, attr_cache.size)`; null on either miss —
    /// the CONSERVATIVE floor (the opposite direction of `freshest_size`).
    size_floor: ?u64,
    /// The write's exclusive end.
    range_end: u64,
    /// `cached_range_fully_mapped`: null when no resident map (anomaly),
    /// false on any hole.
    range_fully_mapped: ?bool,
    /// W-2: a RESIDENT striped snapshot is the whole predicate.
    narrow: bool,
};

/// THE classifier (`inode_write_lock_scope`, fuse_client.rs:4720–4763).
pub fn classify(in: ClassifyInput) Class {
    switch (in.file_type) {
        .striped => {
            const floor = in.size_floor orelse return .meta_prep_only;
            if (in.narrow) return .shared;
            if (in.range_fully_mapped == true and in.range_end <= floor) return .shared;
            return .meta_prep_only;
        },
        .staged => {
            if (in.expected_new_size <= in.block_size) return .meta_prep_only;
            return .entire_op;
        },
        .inline_, .unknown => return .entire_op,
    }
}

/// `(offset / bs, (end − 1) / bs)` on a validated `end > offset` — no
/// `len − 1` underflow anywhere.
pub fn writeTouchedBlocks(offset: u64, end: u64, bs: u64) struct { first: u64, last: u64 } {
    assert(end > offset and bs > 0);
    return .{ .first = offset / bs, .last = (end - 1) / bs };
}

/// The range law (R2-M2): `wlen == 0` is the caller's early `Ok(0)` (no
/// lease, no dirty, no time side effects) BEFORE this; `offset + wlen`
/// overflowing or past `max_file_size` is EFBIG.
pub fn validateRange(offset: u64, wlen: u64, max_file_size: u64) errors.Error!u64 {
    assert(wlen > 0);
    const end = std.math.add(u64, offset, wlen) catch return error.FileTooBig;
    if (end > max_file_size) return error.FileTooBig;
    return end;
}

/// What the RAM-only probe answers for a resident striped inode: the
/// classifier's striped inputs plus the metadata-cache size as the
/// admission SNAPSHOT (KD-6).
pub const SharedProbe = struct {
    /// `min(meta size, attr size)`.
    size_floor: u64,
    /// The metadata-cache size the Shared arm admits on.
    meta_size: u64,
    block_size: u64,
    range_fully_mapped: ?bool,
    narrow: bool,
    /// The snapshot names a striped file on a passthrough set EXACTLY (the
    /// shape table's whole row, `shape_table.zig`): a Shared write may run
    /// its prelude and postlude off it (kv_fs `writeStripedShared`). False
    /// on a transformed set, whose row cannot tell the kind.
    striped_passthrough: bool = false,

    pub fn classifyInput(self: SharedProbe, end: u64) ClassifyInput {
        return .{
            .file_type = .striped,
            .expected_new_size = @max(self.meta_size, end),
            .block_size = self.block_size,
            .size_floor = self.size_floor,
            .range_end = end,
            .range_fully_mapped = self.range_fully_mapped,
            .narrow = self.narrow,
        };
    }
};

/// The data plane's probe: pure `peek` on both caches, zero awaits, zero
/// device touches. Null = any cache miss ("any failure falls through;
/// never an error, never a second admission").
pub const Probe = struct {
    ctx: ?*anyopaque,
    probe_fn: *const fn (ctx: ?*anyopaque, ino: u64, offset: u64, end: u64) ?SharedProbe,

    /// Rung 3: nothing is resident — every write is exclusive.
    pub const none: Probe = .{ .ctx = null, .probe_fn = &noProbe };

    fn noProbe(_: ?*anyopaque, _: u64, _: u64, _: u64) ?SharedProbe {
        return null;
    }

    pub fn probe(self: Probe, ino: u64, offset: u64, end: u64) ?SharedProbe {
        return self.probe_fn(self.ctx, ino, offset, end);
    }
};

// ---------------------------------------------------------------------------
// Instruments (§10).
// ---------------------------------------------------------------------------

/// `inode_guard_stripes`: the width in force.
pub var inode_guard_stripes: counters.Gauge = .{};
/// Contended acquires whose stripe was last taken by a DIFFERENT ino
/// (false sharing — the width's cost) vs the SAME ino (true contention).
pub var inode_guard_stripe_collisions: counters.ShardedCounter = .{};
pub var inode_guard_key_waits: counters.ShardedCounter = .{};
/// `write_lock_wait` (+ `_shared`, `_exclusive`): a CONTENDED acquire's
/// park (the aggregate kept for the convoy row).
pub var write_lock_wait: counters.ShardedLatencyHistogram = .{};
pub var write_lock_wait_shared: counters.ShardedLatencyHistogram = .{};
pub var write_lock_wait_exclusive: counters.ShardedLatencyHistogram = .{};
/// `write_lock_candidate_*`: the classifier's verdict (the preview ledger).
pub var write_lock_candidate_shared: counters.ShardedCounter = .{};
pub var write_lock_candidate_metaprep: counters.ShardedCounter = .{};
pub var write_lock_candidate_entire: counters.ShardedCounter = .{};
/// `write_lock_scope_*`: the FINAL scope held; closure `Σ ≡ WRITE ops`.
pub var write_lock_scope_shared: counters.ShardedCounter = .{};
pub var write_lock_scope_metaprep: counters.ShardedCounter = .{};
pub var write_lock_scope_entire: counters.ShardedCounter = .{};
/// `write_lock_scope_shared_upgrades`: the ONE upgrade fired (0 on
/// fully-mapped rows; nonzero around truncate storms = working).
pub var write_lock_scope_shared_upgrades: counters.ShardedCounter = .{};
/// `write_lock_hold_*`: the guard's hold per FINAL scope; `Σ hold ≡ Σ
/// final scope` (drop-based: an unclassified exit records nothing).
pub var write_lock_hold_shared: counters.ShardedLatencyHistogram = .{};
pub var write_lock_hold_metaprep: counters.ShardedLatencyHistogram = .{};
pub var write_lock_hold_entire: counters.ShardedLatencyHistogram = .{};

fn candidateCounter(c: Class) *counters.ShardedCounter {
    return switch (c) {
        .shared => &write_lock_candidate_shared,
        .meta_prep_only => &write_lock_candidate_metaprep,
        .entire_op => &write_lock_candidate_entire,
    };
}

fn scopeCounter(c: Class) *counters.ShardedCounter {
    return switch (c) {
        .shared => &write_lock_scope_shared,
        .meta_prep_only => &write_lock_scope_metaprep,
        .entire_op => &write_lock_scope_entire,
    };
}

fn holdHistogram(c: Class) *counters.ShardedLatencyHistogram {
    return switch (c) {
        .shared => &write_lock_hold_shared,
        .meta_prep_only => &write_lock_hold_metaprep,
        .entire_op => &write_lock_hold_entire,
    };
}

/// The one A/B lever, read once at mount (L7: never per op, never outside
/// the registry's map).
pub fn sharedEnabled(env: *const std.process.Environ.Map) bool {
    return knobs.boolKnob("SQUEEZEFS_WRITE_SHARED", env);
}

// ---------------------------------------------------------------------------
// The table and its guards.
// ---------------------------------------------------------------------------

/// A held level-1 guard. `scope` is set by `classify`/`finalize`; the
/// release records the hold against the FINAL scope only.
pub const Guard = struct {
    table: *Table,
    stripe: usize,
    mode: Mode,
    /// The acquisition's end clock read IS the hold's start (one read
    /// serves wait + hold).
    since_ns: u64,
    scope: ?Class = null,

    /// Classify the held guard (counted once).
    pub fn classify(self: *Guard, scope: Class) void {
        assert(self.scope == null);
        self.scope = scope;
        scopeCounter(scope).inc();
    }

    pub fn release(self: Guard, io: Io) void {
        if (self.scope) |s| holdHistogram(s).record(clock.monotonicNs() -| self.since_ns);
        (rwlock.RwLock.Guard{ .lock = &self.table.locks[self.stripe], .mode = self.mode }).release(io);
        lock_order.released(.op_guard);
    }
};

/// A two-ino acquisition (COPY_FILE_RANGE's `src read + dest write`): the
/// canonical order, or ONE exclusive guard when both map to one stripe.
pub const PairGuard = struct {
    first: Guard,
    second: ?Guard,

    pub fn release(self: PairGuard, io: Io) void {
        // Release in reverse acquisition order.
        if (self.second) |g| g.release(io);
        self.first.release(io);
    }
};

pub const Table = struct {
    gpa: std.mem.Allocator,
    mask: usize,
    locks: []rwlock.RwLock,
    /// The stripe census word: the last acquirer's ino (never cleared on
    /// release — "last acquirer" semantics; a stale word misclassifies one
    /// wait, never a correctness event). 0 = never acquired, so ino 0
    /// (never a FUSE nodeid) is the sentinel.
    holders: []std.atomic.Value(u64),

    pub fn init(gpa: std.mem.Allocator, stripes: usize) std.mem.Allocator.Error!Table {
        assert(std.math.isPowerOfTwo(stripes));
        const locks = try gpa.alloc(rwlock.RwLock, stripes);
        errdefer gpa.free(locks);
        const holders = try gpa.alloc(std.atomic.Value(u64), stripes);
        @memset(locks, .init);
        @memset(holders, .init(0));
        inode_guard_stripes.set(stripes);
        return .{ .gpa = gpa, .mask = stripes - 1, .locks = locks, .holders = holders };
    }

    pub fn deinit(self: *Table) void {
        self.gpa.free(self.locks);
        self.gpa.free(self.holders);
        self.* = undefined;
    }

    pub fn width(self: *const Table) usize {
        return self.mask + 1;
    }

    pub fn stripeOf(self: *const Table, ino: u64) usize {
        return @intCast(dlm.splitmix64(ino) & self.mask);
    }

    /// The parking acquire: the try fast path first (zero clock reads), the
    /// contended path classifies the census, times the park into
    /// `write_lock_wait[_mode]`, then stamps. Uncancelable: a handler is
    /// never cancelled (charter §9).
    pub fn acquire(self: *Table, io: Io, ino: u64, mode: Mode) Guard {
        const stripe = self.stripeOf(ino);
        const cell = &self.locks[stripe];
        var since: u64 = undefined;
        lock_order.acquiring(.op_guard);
        if (cell.tryAcquireGuard(io, mode) == null) {
            const holder = self.holders[stripe].load(.monotonic);
            if (holder != 0 and holder != ino) inode_guard_stripe_collisions.inc() else inode_guard_key_waits.inc();
            const t0 = clock.monotonicNs();
            _ = cell.acquireGuardUncancelable(io, mode);
            since = clock.monotonicNs();
            const waited = since -| t0;
            write_lock_wait.record(waited);
            switch (mode) {
                .shared => write_lock_wait_shared.record(waited),
                .exclusive => write_lock_wait_exclusive.record(waited),
            }
        } else {
            since = clock.monotonicNs();
        }
        self.holders[stripe].store(ino, .monotonic);
        return .{ .table = self, .stripe = stripe, .mode = mode, .since_ns = since };
    }

    /// The probe's `try_read`: a contended stripe is a Demote, never a spin.
    pub fn tryAcquire(self: *Table, io: Io, ino: u64, mode: Mode) ?Guard {
        const stripe = self.stripeOf(ino);
        if (self.locks[stripe].tryAcquireGuard(io, mode) == null) return null;
        lock_order.acquiredTry(.op_guard);
        self.holders[stripe].store(ino, .monotonic);
        return .{ .table = self, .stripe = stripe, .mode = mode, .since_ns = clock.monotonicNs() };
    }

    /// The canonical two-ino order: `(shard, raw ino)` ascending.
    pub fn pairOrder(self: *const Table, a: u64, b: u64) struct { first: u64, second: u64 } {
        const sa = self.stripeOf(a);
        const sb = self.stripeOf(b);
        if (sa < sb or (sa == sb and a <= b)) return .{ .first = a, .second = b };
        return .{ .first = b, .second = a };
    }

    /// Two inos in canonical order; a shared stripe collapses to ONE
    /// exclusive guard (the stronger mode: a shared+exclusive pair on one
    /// stripe would self-deadlock the non-reentrant lock).
    pub fn acquirePair(self: *Table, io: Io, a: u64, a_mode: Mode, b: u64, b_mode: Mode) PairGuard {
        if (self.stripeOf(a) == self.stripeOf(b)) {
            return .{ .first = self.acquire(io, a, .exclusive), .second = null };
        }
        const order = self.pairOrder(a, b);
        const first_mode = if (order.first == a) a_mode else b_mode;
        const second_mode = if (order.first == a) b_mode else a_mode;
        const first = self.acquire(io, order.first, first_mode);
        const second = self.acquire(io, order.second, second_mode);
        return .{ .first = first, .second = second };
    }

    /// Parked acquirers on `ino`'s stripe (a test observable, never a
    /// product decision).
    pub fn waiters(self: *Table, io: Io, ino: u64) u32 {
        return self.locks[self.stripeOf(ino)].waiters(io);
    }
};

// ---------------------------------------------------------------------------
// The shared-mode admission (§2.4).
// ---------------------------------------------------------------------------

pub const Admission = struct {
    guard: Guard,
    /// KD-6: the admission snapshot the Shared arm resolves `(old_size,
    /// "striped")` from — NO fetch, refill or await under Shared; null on
    /// the exclusive arm (which may fetch).
    shared_snapshot: ?SharedProbe,
    /// Whether the ONE upgrade fired on this admission.
    upgraded: bool,

    pub fn isShared(self: *const Admission) bool {
        return self.guard.mode == .shared;
    }

    /// The handler's resolved candidate → the FINAL scope: Shared iff the
    /// candidate is Shared AND the Shared arm admitted it; a Shared-shaped
    /// candidate on the exclusive arm executes MetaPrepOnly. Counted on
    /// both ledgers; the guard carries the scope to its release.
    pub fn finalize(self: *Admission, candidate: Class) Class {
        candidateCounter(candidate).inc();
        const scope: Class = if (candidate == .shared)
            (if (self.shared_snapshot != null) Class.shared else Class.meta_prep_only)
        else
            candidate;
        assert(scope != .shared or self.guard.mode == .shared);
        self.guard.classify(scope);
        return scope;
    }

    pub fn release(self: Admission, io: Io) void {
        self.guard.release(io);
    }
};

/// The protocol's lock-taking half: probe → classify → acquire as
/// classified → revalidate → at most one upgrade. The caller validated the
/// range (`validateRange`) and answers `wlen == 0` before coming here.
pub fn admitWrite(table: *Table, io: Io, probe: Probe, shared_enabled: bool, ino: u64, offset: u64, end: u64) Admission {
    // A write admitted on this inode may hold bytes in RAM the device does
    // not yet: every direct client's lease of it is invalid from here
    // (`data/lease_epoch.zig`; a nodeid routes to its key ino as the plane's
    // tables do).
    lease_epoch.bump(if (route_table.isRoutable(ino)) route_table.effectiveIno(ino) else ino);
    const admit_shared = shared_enabled and sharedCandidate(probe, ino, offset, end) != null;
    if (!admit_shared) {
        return .{ .guard = table.acquire(io, ino, .exclusive), .shared_snapshot = null, .upgraded = false };
    }
    const read_guard = table.acquire(io, ino, .shared);
    // Revalidate on a fresh snapshot UNDER the guard: a truncate or an
    // eviction between the probe and the grant lands here.
    if (sharedCandidate(probe, ino, offset, end)) |snap| {
        return .{ .guard = read_guard, .shared_snapshot = snap, .upgraded = false };
    }
    read_guard.release(io);
    write_lock_scope_shared_upgrades.inc();
    return .{ .guard = table.acquire(io, ino, .exclusive), .shared_snapshot = null, .upgraded = true };
}

/// The probe's answer delegated to the classifier: a snapshot iff the
/// classifier says Shared.
fn sharedCandidate(probe: Probe, ino: u64, offset: u64, end: u64) ?SharedProbe {
    const p = probe.probe(ino, offset, end) orelse return null;
    return if (classify(p.classifyInput(end)) == .shared) p else null;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

const testing = std.testing;

fn striped(floor: ?u64, end: u64, mapped: ?bool, narrow: bool) ClassifyInput {
    return .{ .file_type = .striped, .expected_new_size = end, .block_size = 4 << 20, .size_floor = floor, .range_end = end, .range_fully_mapped = mapped, .narrow = narrow };
}

test "the ONE classifier, verbatim: every doubt routes to a write class; the narrowed class admits every resident striped shape; mapped-within-floor is shared; staged folds; inline/unknown are EntireOp" {
    // Any probe miss on a striped file: MetaPrepOnly (never a fetch, never an error).
    try testing.expectEqual(Class.meta_prep_only, classify(striped(null, 4096, true, true)));
    // W-2: a resident snapshot is the whole predicate.
    try testing.expectEqual(Class.shared, classify(striped(1 << 20, 2 << 20, null, true)));
    try testing.expectEqual(Class.shared, classify(striped(1 << 20, 1 << 20, false, true)));
    // v1 mapped-only, closed bound: `end ≤ floor`.
    try testing.expectEqual(Class.shared, classify(striped(8192, 8192, true, false)));
    try testing.expectEqual(Class.meta_prep_only, classify(striped(8192, 8193, true, false)));
    try testing.expectEqual(Class.meta_prep_only, classify(striped(8192, 4096, false, false)));
    try testing.expectEqual(Class.meta_prep_only, classify(striped(8192, 4096, null, false)));
    // The folded staged bypass.
    try testing.expectEqual(Class.meta_prep_only, classify(.{ .file_type = .staged, .expected_new_size = 4 << 20, .block_size = 4 << 20, .size_floor = null, .range_end = 4 << 20, .range_fully_mapped = null, .narrow = true }));
    try testing.expectEqual(Class.entire_op, classify(.{ .file_type = .staged, .expected_new_size = (4 << 20) + 1, .block_size = 4 << 20, .size_floor = null, .range_end = 1, .range_fully_mapped = null, .narrow = true }));
    try testing.expectEqual(Class.entire_op, classify(.{ .file_type = .inline_, .expected_new_size = 100, .block_size = 4096, .size_floor = 100, .range_end = 100, .range_fully_mapped = true, .narrow = true }));
    try testing.expectEqual(Class.entire_op, classify(.{ .file_type = .unknown, .expected_new_size = 0, .block_size = 4096, .size_floor = null, .range_end = 1, .range_fully_mapped = null, .narrow = false }));
}

test "range law: touched blocks derive from the exclusive end without underflow; overflow and the max bound are exact EFBIG" {
    const t = writeTouchedBlocks(4096, 8192, 4096);
    try testing.expectEqual(@as(u64, 1), t.first);
    try testing.expectEqual(@as(u64, 1), t.last);
    const u = writeTouchedBlocks(0, 1, 4096);
    try testing.expectEqual(@as(u64, 0), u.last);
    try testing.expectEqual(@as(u64, 4097), try validateRange(1, 4096, 4097));
    try testing.expectError(error.FileTooBig, validateRange(1, 4096, 4096));
    try testing.expectError(error.FileTooBig, validateRange(std.math.maxInt(u64), 1, std.math.maxInt(u64)));
}

test "stripe index is the ONE mix and the pair order is shard-monotone with the raw-ino tie-break" {
    var t = try Table.init(testing.allocator, 64);
    defer t.deinit();
    try testing.expectEqual(@as(usize, 64), t.width());
    try testing.expectEqual(@as(usize, @intCast(dlm.splitmix64(77) & 63)), t.stripeOf(77));
    // Over many pairs the first is never on a higher shard, and on equal
    // shards the lower ino leads.
    var prng = std.Random.DefaultPrng.init(0x5EED);
    const r = prng.random();
    var i: usize = 0;
    while (i < 10_000) : (i += 1) {
        const a = r.int(u64) | 1;
        const b = r.int(u64) | 1;
        const o = t.pairOrder(a, b);
        const sa = t.stripeOf(o.first);
        const sb = t.stripeOf(o.second);
        try testing.expect(sa <= sb);
        if (sa == sb) try testing.expect(o.first <= o.second);
    }
}

/// A probe scripted per call: the n-th probe answers `answers[n]` (null =
/// miss), the last answer repeats.
const ScriptedProbe = struct {
    answers: []const ?SharedProbe,
    calls: usize = 0,

    fn probeFn(ctx: ?*anyopaque, _: u64, _: u64, _: u64) ?SharedProbe {
        const self: *ScriptedProbe = @ptrCast(@alignCast(ctx.?));
        const i = @min(self.calls, self.answers.len - 1);
        self.calls += 1;
        return self.answers[i];
    }

    fn probe(self: *ScriptedProbe) Probe {
        return .{ .ctx = self, .probe_fn = &probeFn };
    }
};

const resident: SharedProbe = .{ .size_floor = 1 << 20, .meta_size = 1 << 20, .block_size = 4 << 20, .range_fully_mapped = true, .narrow = true };

test "admission: a cold cache routes exclusive with zero upgrades; a consistent resident probe admits Shared with the snapshot; a truncate between probe and guard upgrades EXACTLY once; the posture-off arm maps a Shared candidate to exclusive dispatch" {
    const io = testing.io;
    var t = try Table.init(testing.allocator, 64);
    defer t.deinit();
    const up0 = write_lock_scope_shared_upgrades.load();
    const sh0 = write_lock_scope_shared.load();
    const mp0 = write_lock_scope_metaprep.load();
    const en0 = write_lock_scope_entire.load();
    const hs0 = write_lock_hold_shared.snapshot().count;
    const hm0 = write_lock_hold_metaprep.snapshot().count;
    const he0 = write_lock_hold_entire.snapshot().count;

    // Cold: the rung-3 posture.
    var a = admitWrite(&t, io, Probe.none, true, 42, 0, 4096);
    try testing.expect(!a.isShared());
    try testing.expect(!a.upgraded);
    try testing.expectEqual(Class.entire_op, a.finalize(.entire_op));
    a.release(io);

    // Consistent resident: Shared, snapshot carried, no upgrade.
    var consistent = ScriptedProbe{ .answers = &.{resident} };
    var b = admitWrite(&t, io, consistent.probe(), true, 43, 0, 4096);
    try testing.expect(b.isShared());
    try testing.expectEqual(@as(usize, 2), consistent.calls);
    try testing.expectEqual(resident.meta_size, b.shared_snapshot.?.meta_size);
    try testing.expectEqual(Class.shared, b.finalize(.shared));
    b.release(io);

    // Evicted/truncated between the probe and the grant: ONE upgrade.
    var evicted = ScriptedProbe{ .answers = &.{ resident, null } };
    var c = admitWrite(&t, io, evicted.probe(), true, 44, 0, 4096);
    try testing.expect(!c.isShared());
    try testing.expect(c.upgraded);
    try testing.expectEqual(@as(usize, 2), evicted.calls);
    try testing.expectEqual(up0 + 1, write_lock_scope_shared_upgrades.load());
    // A Shared-shaped candidate on the exclusive arm executes MetaPrepOnly.
    try testing.expectEqual(Class.meta_prep_only, c.finalize(.shared));
    c.release(io);

    // The lever off: the probe is never consulted; exclusive dispatch.
    var untouched = ScriptedProbe{ .answers = &.{resident} };
    var d = admitWrite(&t, io, untouched.probe(), false, 45, 0, 4096);
    try testing.expect(!d.isShared());
    try testing.expectEqual(@as(usize, 0), untouched.calls);
    try testing.expectEqual(Class.meta_prep_only, d.finalize(.shared));
    d.release(io);

    // Closure: Σ scope ≡ the four admissions; Σ hold ≡ Σ final scope.
    try testing.expectEqual(@as(u64, 1), write_lock_scope_shared.load() - sh0);
    try testing.expectEqual(@as(u64, 2), write_lock_scope_metaprep.load() - mp0);
    try testing.expectEqual(@as(u64, 1), write_lock_scope_entire.load() - en0);
    try testing.expectEqual(@as(u64, 1), write_lock_hold_shared.snapshot().count - hs0);
    try testing.expectEqual(@as(u64, 2), write_lock_hold_metaprep.snapshot().count - hm0);
    try testing.expectEqual(@as(u64, 1), write_lock_hold_entire.snapshot().count - he0);
    try testing.expectEqual(@as(u64, 0), t.locks[t.stripeOf(42)].readers);
    try testing.expect(!t.locks[t.stripeOf(42)].writer);
}

test "write preference at the table: a reader holds, a writer parks, a fresh shared try is refused; the writer is granted when the reader leaves" {
    const io = testing.io;
    var t = try Table.init(testing.allocator, 16);
    defer t.deinit();
    const ino: u64 = 9;
    const census0 = inode_guard_key_waits.load() + inode_guard_stripe_collisions.load();
    const wait0 = write_lock_wait_exclusive.snapshot().count;
    const reader = t.acquire(io, ino, .shared);
    const Writer = struct {
        fn run(tbl: *Table, i: u64, got: *std.atomic.Value(bool)) void {
            const g = tbl.acquire(testing.io, i, .exclusive);
            got.store(true, .release);
            g.release(testing.io);
        }
    };
    var got: std.atomic.Value(bool) = .init(false);
    const th = try std.Thread.spawn(.{}, Writer.run, .{ &t, ino, &got });
    while (t.waiters(io, ino) == 0) std.atomic.spinLoopHint();
    // The writer is queued: a fresh shared attempt yields to it.
    try testing.expectEqual(@as(?Guard, null), t.tryAcquire(io, ino, .shared));
    try testing.expect(!got.load(.acquire));
    reader.release(io);
    th.join();
    try testing.expect(got.load(.acquire));
    // The writer's contended acquire: one census classification (a key
    // wait — the same ino held the stripe) and one exclusive wait sample.
    try testing.expectEqual(census0 + 1, inode_guard_key_waits.load() + inode_guard_stripe_collisions.load());
    try testing.expectEqual(wait0 + 1, write_lock_wait_exclusive.snapshot().count);
}

/// The storm: `threads` workers each take `ops` guards on ONE ino in
/// `mode`, hold for a short spin, and record the peak concurrent holders.
fn storm(t: *Table, mode: Mode, threads: usize, ops: usize) !struct { peak: u32, ns_per_op: u64 } {
    const Shared = struct {
        inside: std.atomic.Value(u32) = .init(0),
        peak: std.atomic.Value(u32) = .init(0),
    };
    const Worker = struct {
        fn run(tbl: *Table, m: Mode, n: usize, sh: *Shared) void {
            var i: usize = 0;
            while (i < n) : (i += 1) {
                const g = tbl.acquire(testing.io, 1234, m);
                const now_inside = sh.inside.fetchAdd(1, .acq_rel) + 1;
                _ = sh.peak.fetchMax(now_inside, .acq_rel);
                var spin: u32 = 0;
                while (spin < 200) : (spin += 1) std.atomic.spinLoopHint();
                _ = sh.inside.fetchSub(1, .acq_rel);
                g.release(testing.io);
            }
        }
    };
    var sh: Shared = .{};
    var handles: [16]std.Thread = undefined;
    const t0 = clock.monotonicNs();
    for (handles[0..threads]) |*h| h.* = try std.Thread.spawn(.{}, Worker.run, .{ t, mode, ops, &sh });
    for (handles[0..threads]) |h| h.join();
    const elapsed = clock.monotonicNs() -| t0;
    return .{ .peak = sh.peak.load(.acquire), .ns_per_op = elapsed / (threads * ops) };
}

test "storm: readers run CONCURRENTLY under Shared (peak holders > 1) and SERIALIZE under EntireOp (peak == 1); the table is clean after" {
    var t = try Table.init(testing.allocator, INODE_GUARD_STRIPES);
    defer t.deinit();
    const threads: usize = @min(8, @max(2, std.Thread.getCpuCount() catch 2));
    const ops: usize = 2000;
    const shared = try storm(&t, .shared, threads, ops);
    const exclusive = try storm(&t, .exclusive, threads, ops);
    std.debug.print("\nop-guard storm ({d} threads × {d} ops, one ino): shared peak holders {d}, {d} ns/op; exclusive peak holders {d}, {d} ns/op\n", .{ threads, ops, shared.peak, shared.ns_per_op, exclusive.peak, exclusive.ns_per_op });
    try testing.expect(shared.peak > 1);
    try testing.expectEqual(@as(u32, 1), exclusive.peak);
    const cell = &t.locks[t.stripeOf(1234)];
    try testing.expectEqual(@as(u32, 0), cell.readers);
    try testing.expect(!cell.writer);
    try testing.expectEqual(@as(u32, 0), cell.waiters(testing.io));
}

test "no deadlock over random two-ino plans: every worker acquires random pairs in the canonical order (shared stripes collapse) and the storm completes" {
    var t = try Table.init(testing.allocator, 8);
    defer t.deinit();
    const Worker = struct {
        fn run(tbl: *Table, seed: u64) void {
            var prng = std.Random.DefaultPrng.init(seed);
            const r = prng.random();
            var i: usize = 0;
            while (i < 3000) : (i += 1) {
                // A small ino space on an 8-stripe table forces shared
                // stripes and every pair order.
                const a = r.intRangeAtMost(u64, 1, 12);
                const b = r.intRangeAtMost(u64, 1, 12);
                const am: Mode = if (r.boolean()) .shared else .exclusive;
                const bm: Mode = if (r.boolean()) .shared else .exclusive;
                const pg = tbl.acquirePair(testing.io, a, am, b, bm);
                pg.release(testing.io);
            }
        }
    };
    var handles: [6]std.Thread = undefined;
    for (&handles, 0..) |*h, i| h.* = try std.Thread.spawn(.{}, Worker.run, .{ &t, @as(u64, i) + 1 });
    for (handles) |h| h.join();
    for (t.locks) |*l| {
        try testing.expectEqual(@as(u32, 0), l.readers);
        try testing.expect(!l.writer);
    }
}

test "a shared stripe pair collapses to ONE exclusive guard; distinct stripes take two in canonical order" {
    const io = testing.io;
    var t = try Table.init(testing.allocator, 1);
    defer t.deinit();
    const pg = t.acquirePair(io, 3, .shared, 5, .shared);
    try testing.expect(pg.second == null);
    try testing.expectEqual(Mode.exclusive, pg.first.mode);
    pg.release(io);
    var t2 = try Table.init(testing.allocator, 1 << 16);
    defer t2.deinit();
    var a: u64 = 1;
    while (t2.stripeOf(a) == t2.stripeOf(2)) a += 1;
    const pg2 = t2.acquirePair(io, a, .shared, 2, .exclusive);
    try testing.expect(pg2.second != null);
    const o = t2.pairOrder(a, 2);
    try testing.expectEqual(t2.stripeOf(o.first), pg2.first.stripe);
    pg2.release(io);
}
