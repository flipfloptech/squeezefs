//! The 4a metadata object locks — `I{ino}` / `D{parent,name}` (charter §2
//! "Lock order" level 4a; survey §2.4; the reference's `dlm.rs` +
//! `stripe_locks.rs`).
//!
//! Two fixed stripe tables of `sync.RwLock`, one per class, selected by a
//! splitmix64 mix of the object's key and a mask (the width is a power of
//! two; never a division): zero allocation per acquisition, zero map growth.
//! Two distinct objects may share a stripe; a collision only WIDENS a
//! critical section (never under-locks) — but the guard is held across the
//! whole commit park (D5), so that widening costs the colliding op a full
//! commit. Hence (D-3) the width is DERIVED, `next_pow2(max(4096,
//! 16 × possible_cpus × q_depth))`: `possible_cpus × q_depth` is the most
//! ops the FUSE-over-io_uring ring can present at once, and the load factor
//! 1/16 keeps a false-sharing wait under one acquire in sixteen at full
//! delivered concurrency; 4096 is the shipped floor (never-regress). Each
//! class carries a collision CENSUS (`dlm_{inode,dentry}_stripe_collisions`
//! vs `_key_waits`) classified against the stripe's last-acquirer word on
//! the contended path only, and the 4a wait histogram
//! `lock_phase_ns.dlm_guard_wait` — one sample per contended acquire, so
//! its count ≡ Σ both classes' census.
//!
//! Correctness rests on ONE rule, enforced by `Manager.lockMany`: the
//! **canonical acquisition order** — every `I` lock before any `D` lock;
//! within a class dedup by stripe index and acquire ascending; a stripe
//! requested shared AND exclusive is taken exclusive. Re-locking a shared
//! stripe would self-deadlock the non-reentrant lock; opposite per-op orders
//! would ABBA. An op's set is one `lockMany` per volume, taken holding no
//! other 4a guard; an op that DISCOVERS targets mid-flight (unlink's child)
//! locks a first phase, reads, drops, re-locks the full set, revalidates —
//! never `I` after `D`. The ONE sanctioned out-of-order addition is
//! `tryLockMany`: try-only, all-or-nothing, a miss is the caller's retry
//! class — a parking acquire beside held guards could cycle with a peer's
//! canonical set.
//!
//! Guards drop BEFORE the ack (D-3's fan-out law): 11 % of many-dirs
//! renames at 32 writers self-waited on their own previous tx when the
//! guards outlived the answer (`dlm_inode_key_waits` p50 ≤ 16 µs).

const std = @import("std");
const testkit = @import("../core/testkit.zig");
const Io = std.Io;
const assert = std.debug.assert;
const derive = @import("../core/derive.zig");
const knobs = @import("../core/knobs.zig");
const counters = @import("../core/counters.zig");
const clock = @import("../core/clock.zig");
const checksum = @import("../core/checksum.zig");
const rwlock = @import("../sync/rwlock.zig");
const lock_order = @import("../sync/lock_order.zig");

pub const Mode = rwlock.Mode;
pub const Class = enum { inode, dentry };

// ===========================================================================
// Width derivation (D-3)
// ===========================================================================

/// `α = in-flight ÷ stripes` IS the false-sharing probability of one
/// acquire against the keys already held; 1/16 keeps it under one acquire
/// in sixteen at the transport's full delivered concurrency.
pub const stripe_load_factor_inv: usize = 16;
/// The shipped 4a width (both classes) — the derived table's floor.
pub const dlm_stripes_shipped: usize = 4096;

/// `next_pow2(max(shipped, 16 × possible_cpus × q_depth))`.
pub fn derivedStripeWidth(shipped: usize, possible_cpus: usize, q_depth: usize) usize {
    const concurrency = @max(possible_cpus, 1) *| @max(q_depth, 1) *| stripe_load_factor_inv;
    return @intCast(derive.nextPow2(@max(concurrency, @max(shipped, 1))));
}

/// `SQUEEZEFS_DLM_STRIPES` resolution, pure: explicit wins verbatim
/// (rounded UP to a power of two — the index is a mask; `1` is the
/// everything-serializes crucible, `4096` the shipped-4a A/B control), else
/// the derived law.
pub fn resolveStripes(explicit: ?usize, shipped: usize, possible_cpus: usize, q_depth: usize) usize {
    if (explicit) |n| return @intCast(derive.nextPow2(@max(n, 1)));
    return derivedStripeWidth(shipped, possible_cpus, q_depth);
}

/// The width in force for a process: the knob over the derived law.
/// `q_depth` is the transport's sizing depth (the explicit queue-depth knob
/// clamped to the desired depth, else the desired depth — the transport
/// supplies it; sizing for the ceiling is the conservative direction).
pub fn resolveWidth(env: *const std.process.Environ.Map, facts: derive.Facts, q_depth: usize) usize {
    return resolveStripes(knobs.optIntKnob("SQUEEZEFS_DLM_STRIPES", usize, env), dlm_stripes_shipped, facts.possible_cpus, q_depth);
}

// ===========================================================================
// Census words and histograms (survey §5.3; closure
// `lock_phase_ns.dlm_guard_wait.count ≡ Σ census`)
// ===========================================================================

/// `dlm_inode_stripes` / `dlm_dentry_stripes`: the width in force.
pub var dlm_inode_stripes: counters.Gauge = .{};
pub var dlm_dentry_stripes: counters.Gauge = .{};
/// Contended acquires whose stripe was last taken by a DIFFERENT key (false
/// sharing — the width's cost) vs the SAME key (true contention — the
/// workload's).
pub var dlm_inode_stripe_collisions: counters.ShardedCounter = .{};
pub var dlm_inode_key_waits: counters.ShardedCounter = .{};
pub var dlm_dentry_stripe_collisions: counters.ShardedCounter = .{};
pub var dlm_dentry_key_waits: counters.ShardedCounter = .{};
/// `lock_phase_ns.dlm_guard_wait`: the parked span of a CONTENDED acquire,
/// both classes, both modes (the uncontended fast path records nothing).
pub var lock_phase_dlm_guard_wait: counters.ShardedLatencyHistogram = .{};
/// `lock_phase_ns.dlm_guard_hold`: an EXCLUSIVE `I{ino}` guard's hold,
/// acquisition → release.
pub var lock_phase_dlm_guard_hold: counters.ShardedLatencyHistogram = .{};

/// The first-touch spin bound: a stripe's stamp trails its grant by a few
/// instructions, so a word still 0 under a storm's synchronized start is
/// re-read a few hundred times (well under a microsecond) before it
/// classifies as a key wait — the census under-reports collisions, never
/// over-reports them.
pub const first_touch_spins: u32 = 256;

/// splitmix64 finalizer — the ONE mix every stripe index and census key word
/// derives from (the level-1 op guard's table, `fuse/handler/op_guard.zig`,
/// indexes by the same mix so an ino's two stripes never diverge in shape).
pub fn splitmix64(v: u64) u64 {
    var x = v;
    x ^= x >> 30;
    x *%= 0xbf58476d1ce4e5b9;
    x ^= x >> 27;
    x *%= 0x94d049bb133111eb;
    x ^= x >> 31;
    return x;
}

/// Nonzero identity word for an `(a, b)` key: the table's own mix `| 1`, so 0
/// stays the never-acquired sentinel (two keys colliding on one WORD is a
/// ~2⁻⁶³ diagnostic misclassification).
pub fn keyWord(a: u64, b: u64) u64 {
    return splitmix64(a ^ std.math.rotl(u64, b, 32)) | 1;
}

/// The dentry class's hash input: `parent ^ rotl(hash64(name), 1)`.
pub fn dentryKey(parent: u64, name: []const u8) u64 {
    return parent ^ std.math.rotl(u64, checksum.hash64(name), 1);
}

// ===========================================================================
// The manager
// ===========================================================================

pub const InodeReq = struct { ino: u64, mode: Mode };
pub const DentryReq = struct { parent: u64, name: []const u8, mode: Mode };

/// Requests per class in one plan. Rename EXCHANGE across parents is 4
/// inode + 2 dentry requests; 16 leaves 4× headroom for the symmetric
/// plane's cross-owner plans and bounds the stack frame a plan needs.
pub const max_requests = 16;

/// The WIDE plan the symmetric plane's one-intent striped `rmdir` takes
/// (charter §5.5 "Directory striping"; `sym_tokens_intents_striping.md`
/// §4.7): `I{parent}` + `I{D}` + K `I{D_i}` + K + 3 `D{}` at K = 64 ⇒
/// `2 × MINT_SPREAD + 8 = 136` — the same bound as the intent's steps. Only
/// `lockManyWide` builds a set this wide; every ordinary op keeps the 16.
pub const wide_requests = 2 * 64 + 8;

pub const PlanError = error{PlanTooLarge};

/// One held 4a object lock, movable by value (an op's set travels into its
/// conveyor queue entry and is released at the terminal outcome).
pub const Guard = struct {
    lock: *rwlock.RwLock,
    mode: Mode,
    /// Nonzero = hold-timed (the exclusive inode class): the acquisition
    /// instant for `lock_phase_ns.dlm_guard_hold`.
    acquired_ns: u64,

    pub fn release(g: Guard, io: Io) void {
        if (g.acquired_ns != 0) lock_phase_dlm_guard_hold.record(clock.monotonicNs() -| g.acquired_ns);
        (rwlock.RwLock.Guard{ .lock = g.lock, .mode = g.mode }).release(io);
        lock_order.released(.object);
    }
};

/// An op's whole guard set: canonical order (`I` ascending, then `D`
/// ascending), one guard per distinct stripe; `cap` requests per class.
pub fn GuardSetOf(comptime cap: usize) type {
    return struct {
        const Self = @This();

        guards: [2 * cap]Guard = undefined,
        len: usize = 0,

        pub fn slice(self: *const Self) []const Guard {
            return self.guards[0..self.len];
        }

        pub fn release(self: *Self, io: Io) void {
            for (self.slice()) |g| g.release(io);
            self.len = 0;
        }

        /// Does the set hold `lock` in a mode at least `mode`?
        pub fn holds(self: *const Self, lock: *const rwlock.RwLock, mode: Mode) bool {
            for (self.slice()) |g| {
                if (g.lock != lock) continue;
                if (mode == .shared or g.mode == .exclusive) return true;
            }
            return false;
        }

        fn push(self: *Self, g: Guard) void {
            assert(self.len < self.guards.len);
            self.guards[self.len] = g;
            self.len += 1;
        }
    };
}

pub const GuardSet = GuardSetOf(max_requests);
/// The striped `rmdir`'s set (`wide_requests`).
pub const WideGuardSet = GuardSetOf(wide_requests);

const Census = struct {
    collisions: *counters.ShardedCounter,
    key_waits: *counters.ShardedCounter,
};

/// One lock class: the stripe table plus its census state (the stripe's
/// last-acquirer word — never cleared on release, "last acquirer"
/// semantics; a stale word misclassifies one wait, never a correctness
/// event).
const LockClass = struct {
    locks: []rwlock.RwLock,
    holders: []std.atomic.Value(u64),
    census: Census,

    fn init(gpa: std.mem.Allocator, stripes: usize, census: Census) !LockClass {
        const locks = try gpa.alloc(rwlock.RwLock, stripes);
        errdefer gpa.free(locks);
        const holders = try gpa.alloc(std.atomic.Value(u64), stripes);
        @memset(locks, .init);
        @memset(holders, .init(0));
        return .{ .locks = locks, .holders = holders, .census = census };
    }

    fn deinit(self: *LockClass, gpa: std.mem.Allocator) void {
        gpa.free(self.locks);
        gpa.free(self.holders);
    }

    /// The CONTENDED arm (the try refused): classify against the stripe's
    /// last acquirer BEFORE parking.
    fn classifyContended(self: *LockClass, stripe: usize, key: u64) void {
        var holder = self.holders[stripe].load(.monotonic);
        var spins: u32 = 0;
        while (holder == 0 and spins < first_touch_spins) : (spins += 1) {
            std.atomic.spinLoopHint();
            holder = self.holders[stripe].load(.monotonic);
        }
        if (holder != 0 and holder != key) self.census.collisions.inc() else self.census.key_waits.inc();
    }

    /// Every acquisition stamps its stripe (one relaxed store).
    fn stamp(self: *LockClass, stripe: usize, key: u64) void {
        self.holders[stripe].store(key, .monotonic);
    }

    /// Try-acquire fast path (zero clock reads); the contended path
    /// classifies, times the park, then stamps.
    fn lockStripe(self: *LockClass, io: Io, stripe: usize, key: u64, mode: Mode, hold_timed: bool, comptime cancelable: bool) Io.Cancelable!Guard {
        const cell = &self.locks[stripe];
        lock_order.acquiring(.object);
        errdefer lock_order.released(.object);
        if (cell.tryAcquireGuard(io, mode) == null) {
            self.classifyContended(stripe, key);
            const t0 = clock.monotonicNs();
            if (cancelable) {
                _ = try cell.acquireGuard(io, mode);
            } else {
                _ = cell.acquireGuardUncancelable(io, mode);
            }
            lock_phase_dlm_guard_wait.record(clock.monotonicNs() -| t0);
        }
        self.stamp(stripe, key);
        return .{ .lock = cell, .mode = mode, .acquired_ns = if (hold_timed) clock.monotonicNs() else 0 };
    }

    fn tryLockStripe(self: *LockClass, io: Io, stripe: usize, key: u64, mode: Mode, hold_timed: bool) ?Guard {
        const cell = &self.locks[stripe];
        if (cell.tryAcquireGuard(io, mode) == null) return null;
        lock_order.acquiredTry(.object);
        self.stamp(stripe, key);
        return .{ .lock = cell, .mode = mode, .acquired_ns = if (hold_timed) clock.monotonicNs() else 0 };
    }
};

/// One class's canonical plan: `(stripe, census key, mode)` sorted by stripe
/// ascending with exclusive before shared, deduped by stripe (so the
/// strongest mode survives and the FIRST object of a shared stripe is the
/// stripe's census identity — an in-group collision is one acquire, never a
/// self-collision).
fn PlanOf(comptime cap: usize) type {
    return struct {
        const Self = @This();
        const Entry = struct { stripe: usize, key: u64, mode: Mode };

        entries: [cap]Entry = undefined,
        len: usize = 0,

        fn add(self: *Self, stripe: usize, key: u64, mode: Mode) PlanError!void {
            if (self.len == cap) return error.PlanTooLarge;
            self.entries[self.len] = .{ .stripe = stripe, .key = key, .mode = mode };
            self.len += 1;
        }

        fn lessThan(_: void, a: Entry, b: Entry) bool {
            if (a.stripe != b.stripe) return a.stripe < b.stripe;
            return a.mode == .exclusive and b.mode == .shared;
        }

        fn canonical(self: *Self) []const Entry {
            std.mem.sort(Entry, self.entries[0..self.len], {}, lessThan);
            var out: usize = 0;
            for (self.entries[0..self.len]) |e| {
                if (out > 0 and self.entries[out - 1].stripe == e.stripe) continue;
                self.entries[out] = e;
                out += 1;
            }
            self.len = out;
            return self.entries[0..out];
        }
    };
}

const Plan = PlanOf(max_requests);

pub const Manager = struct {
    gpa: std.mem.Allocator,
    /// `width − 1`; the index is `hash & mask`.
    mask: usize,
    inode: LockClass,
    dentry: LockClass,

    /// Both classes at `width` (a power of two: every shipped width is, and
    /// `resolveStripes` rounds up by construction — a violation is a
    /// construction-time programming error, never a runtime path).
    pub fn init(gpa: std.mem.Allocator, stripes: usize) !Manager {
        assert(std.math.isPowerOfTwo(stripes));
        var inode = try LockClass.init(gpa, stripes, .{ .collisions = &dlm_inode_stripe_collisions, .key_waits = &dlm_inode_key_waits });
        errdefer inode.deinit(gpa);
        const dentry = try LockClass.init(gpa, stripes, .{ .collisions = &dlm_dentry_stripe_collisions, .key_waits = &dlm_dentry_key_waits });
        dlm_inode_stripes.set(stripes);
        dlm_dentry_stripes.set(stripes);
        return .{ .gpa = gpa, .mask = stripes - 1, .inode = inode, .dentry = dentry };
    }

    pub fn deinit(self: *Manager) void {
        self.inode.deinit(self.gpa);
        self.dentry.deinit(self.gpa);
    }

    /// The stripe population of each class.
    pub fn width(self: *const Manager) usize {
        return self.mask + 1;
    }

    /// Stripe index of an inode-class lock (public: tests force collisions).
    pub fn inodeStripe(self: *const Manager, ino: u64) usize {
        return @intCast(splitmix64(ino) & self.mask);
    }

    /// Stripe index of a dentry-class lock (public: tests force collisions).
    pub fn dentryStripe(self: *const Manager, parent: u64, name: []const u8) usize {
        return @intCast(splitmix64(dentryKey(parent, name)) & self.mask);
    }

    /// Acquire a multi-object set in canonical order. Cancellation releases
    /// every guard taken so far.
    pub fn lockMany(self: *Manager, io: Io, inodes: []const InodeReq, dentries: []const DentryReq) (PlanError || Io.Cancelable)!GuardSet {
        return self.lockManyInner(io, inodes, dentries, true);
    }

    pub fn lockManyUncancelable(self: *Manager, io: Io, inodes: []const InodeReq, dentries: []const DentryReq) PlanError!GuardSet {
        return self.lockManyInner(io, inodes, dentries, false) catch |err| switch (err) {
            error.Canceled => unreachable,
            error.PlanTooLarge => |e| return e,
        };
    }

    /// The NON-PARKING form: the same canonical plan with the try fast path
    /// at every stripe; `null` — nothing held — at the first contended one.
    pub fn tryLockMany(self: *Manager, io: Io, inodes: []const InodeReq, dentries: []const DentryReq) PlanError!?GuardSet {
        return self.tryLockManyOf(max_requests, io, inodes, dentries);
    }

    /// The WIDE canonical acquisition (`wide_requests`): the striped
    /// `rmdir`'s one set per volume.
    pub fn lockManyWide(self: *Manager, io: Io, inodes: []const InodeReq, dentries: []const DentryReq) PlanError!WideGuardSet {
        return self.lockManyOf(wide_requests, io, inodes, dentries, false) catch |err| switch (err) {
            error.Canceled => unreachable,
            error.PlanTooLarge => |e| return e,
        };
    }

    pub fn tryLockManyWide(self: *Manager, io: Io, inodes: []const InodeReq, dentries: []const DentryReq) PlanError!?WideGuardSet {
        return self.tryLockManyOf(wide_requests, io, inodes, dentries);
    }

    fn tryLockManyOf(self: *Manager, comptime cap: usize, io: Io, inodes: []const InodeReq, dentries: []const DentryReq) PlanError!?GuardSetOf(cap) {
        var set: GuardSetOf(cap) = .{};
        var iplan = try self.inodePlanOf(cap, inodes);
        for (iplan.canonical()) |e| {
            const g = self.inode.tryLockStripe(io, e.stripe, e.key, e.mode, e.mode == .exclusive) orelse {
                set.release(io);
                return null;
            };
            set.push(g);
        }
        var dplan = try self.dentryPlanOf(cap, dentries);
        for (dplan.canonical()) |e| {
            const g = self.dentry.tryLockStripe(io, e.stripe, e.key, e.mode, false) orelse {
                set.release(io);
                return null;
            };
            set.push(g);
        }
        return set;
    }

    /// Does `set` (taken in THIS table) cover every request — each
    /// request's stripe held in a mode at least the one asked? The
    /// coverage law of a served cross-owner step under a travelling scope
    /// (`sym/xv/guards.zig`): by `(class, stripe)`, never by key.
    pub fn covers(self: *const Manager, comptime cap: usize, set: *const GuardSetOf(cap), inodes: []const InodeReq, dentries: []const DentryReq) bool {
        for (inodes) |r| if (!set.holds(&self.inode.locks[self.inodeStripe(r.ino)], r.mode)) return false;
        for (dentries) |r| if (!set.holds(&self.dentry.locks[self.dentryStripe(r.parent, r.name)], r.mode)) return false;
        return true;
    }

    /// One `I{ino}` lock (the exclusive form is hold-timed).
    pub fn lockInode(self: *Manager, io: Io, ino: u64, mode: Mode) Io.Cancelable!Guard {
        return self.inode.lockStripe(io, self.inodeStripe(ino), keyWord(ino, 0), mode, mode == .exclusive, true);
    }

    /// One `D{parent,name}` lock.
    pub fn lockDentry(self: *Manager, io: Io, parent: u64, name: []const u8, mode: Mode) Io.Cancelable!Guard {
        const key = dentryKey(parent, name);
        return self.dentry.lockStripe(io, self.dentryStripe(parent, name), keyWord(key, 0), mode, false, true);
    }

    fn lockManyInner(self: *Manager, io: Io, inodes: []const InodeReq, dentries: []const DentryReq, comptime cancelable: bool) (PlanError || Io.Cancelable)!GuardSet {
        return self.lockManyOf(max_requests, io, inodes, dentries, cancelable);
    }

    fn lockManyOf(self: *Manager, comptime cap: usize, io: Io, inodes: []const InodeReq, dentries: []const DentryReq, comptime cancelable: bool) (PlanError || Io.Cancelable)!GuardSetOf(cap) {
        var set: GuardSetOf(cap) = .{};
        errdefer set.release(io);
        var iplan = try self.inodePlanOf(cap, inodes);
        for (iplan.canonical()) |e| {
            set.push(try self.inode.lockStripe(io, e.stripe, e.key, e.mode, e.mode == .exclusive, cancelable));
        }
        var dplan = try self.dentryPlanOf(cap, dentries);
        for (dplan.canonical()) |e| {
            set.push(try self.dentry.lockStripe(io, e.stripe, e.key, e.mode, false, cancelable));
        }
        return set;
    }

    fn inodePlanOf(self: *const Manager, comptime cap: usize, inodes: []const InodeReq) PlanError!PlanOf(cap) {
        var plan: PlanOf(cap) = .{};
        for (inodes) |r| try plan.add(self.inodeStripe(r.ino), keyWord(r.ino, 0), r.mode);
        return plan;
    }

    fn dentryPlanOf(self: *const Manager, comptime cap: usize, dentries: []const DentryReq) PlanError!PlanOf(cap) {
        var plan: PlanOf(cap) = .{};
        for (dentries) |r| {
            const key = dentryKey(r.parent, r.name);
            try plan.add(self.dentryStripe(r.parent, r.name), keyWord(key, 0), r.mode);
        }
        return plan;
    }
};

// ===========================================================================
// Tests (survey row 9: `meta_dlm_stripe_tests`, the derivation tie, the
// census closure; plus the multi-threaded no-deadlock storm)
// ===========================================================================

const testing = std.testing;

test "dlm_stripe_width_derives_from_possible_cpus_times_q_depth: the tie test" {
    try testing.expectEqual(@as(usize, 16), stripe_load_factor_inv);
    try testing.expectEqual(@as(usize, 4096), dlm_stripes_shipped);
    const cpus = [_]usize{ 1, 2, 4, 8, 16, 32, 64, 128, 192 };
    const depths = [_]usize{ 4, 8, 16, 32 };
    for ([_]usize{ 4096, 1024 }) |shipped| {
        var prev_by_depth: [4]usize = @splat(0);
        for (cpus) |c| {
            for (depths, 0..) |d, di| {
                const w = derivedStripeWidth(shipped, c, d);
                try testing.expect(std.math.isPowerOfTwo(w));
                try testing.expect(w >= shipped);
                try testing.expect(w >= 16 * c * d);
                try testing.expect(w < 2 * @max(16 * c * d, shipped));
                try testing.expect(w >= prev_by_depth[di]);
                prev_by_depth[di] = w;
                if (di > 0) try testing.expect(w >= derivedStripeWidth(shipped, c, depths[di - 1]));
            }
        }
    }
    try testing.expectEqual(@as(usize, 16_384), derivedStripeWidth(4096, 32, 32));
    try testing.expectEqual(@as(usize, 4096), derivedStripeWidth(4096, 2, 4));
    // The depth-degraded field box: 32 × 4 × 16 = 2048 < 4096 — the floor holds.
    try testing.expectEqual(@as(usize, 4096), derivedStripeWidth(4096, 32, 4));
    try testing.expectEqual(@as(usize, 2048), derivedStripeWidth(1024, 32, 4));
    try testing.expectEqual(@as(usize, 131_072), derivedStripeWidth(4096, 192, 32));
}

test "dlm_stripes_knob_is_explicit_over_derived_and_rounds_up_to_pow2" {
    try testing.expectEqual(@as(usize, 16_384), resolveStripes(null, 4096, 32, 32));
    try testing.expectEqual(@as(usize, 4096), resolveStripes(4096, 4096, 32, 32));
    try testing.expectEqual(@as(usize, 1024), resolveStripes(1024, 4096, 32, 32));
    try testing.expectEqual(@as(usize, 1), resolveStripes(1, 4096, 32, 32));
    try testing.expectEqual(@as(usize, 1024), resolveStripes(1000, 4096, 32, 32));
    try testing.expectEqual(@as(usize, 65_536), resolveStripes(65_536, 1024, 2, 4));
    // Through the registry: absent derives, present wins.
    const k = knobs.lookup("SQUEEZEFS_DLM_STRIPES").?;
    try testing.expectEqual(@as(i128, 1), k.kind.int.lo);
    try testing.expectEqual(@as(i128, 1 << 24), k.kind.int.hi);
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    const facts = derive.Facts.synthetic(32, 0);
    try testing.expectEqual(@as(usize, 16_384), resolveWidth(&env, facts, 32));
    try env.put("SQUEEZEFS_DLM_STRIPES", "1000");
    try testing.expectEqual(@as(usize, 1024), resolveWidth(&env, facts, 32));
}

test "the wide set takes 136 requests per class in one canonical acquisition (the striped rmdir's shape) and `covers` judges by (class, stripe) with the mode floor; an ordinary plan past 16 refuses" {
    var m = try Manager.init(testing.allocator, 4096);
    defer m.deinit();
    var inodes: [wide_requests]InodeReq = undefined;
    var dents: [wide_requests]DentryReq = undefined;
    for (&inodes, 0..) |*r, i| r.* = .{ .ino = 1000 + i, .mode = .exclusive };
    for (&dents, 0..) |*r, i| r.* = .{ .parent = 7, .name = &[_]u8{ 'n', @intCast(i & 0xFF), @intCast(i >> 8) }, .mode = .exclusive };
    try testing.expectError(error.PlanTooLarge, m.lockMany(testing.io, &inodes, &.{}));
    var wide = try m.lockManyWide(testing.io, &inodes, &dents);
    try testing.expect(wide.len <= 2 * wide_requests and wide.len > 0);
    try testing.expect(m.covers(wide_requests, &wide, inodes[0..3], dents[0..3]));
    try testing.expect(m.covers(wide_requests, &wide, &.{.{ .ino = 1000, .mode = .shared }}, &.{}));
    // A contended stripe: the try form answers null and holds nothing.
    try testing.expectEqual(@as(?WideGuardSet, null), try m.tryLockManyWide(testing.io, inodes[0..1], &.{}));
    wide.release(testing.io);
    var set = (try m.tryLockMany(testing.io, inodes[0..2], dents[0..1])).?;
    // A key outside the set is not covered; a shared ask over an
    // exclusive hold is.
    try testing.expect(!m.covers(max_requests, &set, &.{.{ .ino = 999_999, .mode = .shared }}, &.{}));
    try testing.expect(m.covers(max_requests, &set, &.{.{ .ino = 1001, .mode = .shared }}, dents[0..1]));
    set.release(testing.io);
}

test "a fresh manager is built at the requested width and publishes the stripes gauges" {
    var m = try Manager.init(testing.allocator, 4096);
    defer m.deinit();
    try testing.expectEqual(@as(usize, 4096), m.width());
    try testing.expectEqual(@as(u64, 4096), dlm_inode_stripes.load());
    try testing.expectEqual(@as(u64, 4096), dlm_dentry_stripes.load());
    try testing.expectEqual(m.inodeStripe(12345), m.inodeStripe(12345));
    try testing.expectEqual(m.dentryStripe(7, "a"), m.dentryStripe(7, "a"));
    try testing.expect(m.dentryStripe(7, "a") != m.dentryStripe(7, "b") or m.dentryStripe(8, "a") != m.dentryStripe(7, "a"));
    // The mix spreads: a thousand inos never collapse onto one stripe.
    var distinct: usize = 0;
    var i: u64 = 1;
    while (i < 1000) : (i += 1) distinct += @intFromBool(m.inodeStripe(i) != m.inodeStripe(0));
    try testing.expect(distinct > 900);
    try testing.expect(keyWord(0, 0) != 0);
}

/// Two distinct inos sharing a stripe (tiny width forces it).
fn collidingInos(m: *const Manager) [2]u64 {
    var i: u64 = 1;
    while (true) : (i += 1) {
        var j: u64 = i + 1;
        while (j < i + 64) : (j += 1) if (m.inodeStripe(i) == m.inodeStripe(j)) return .{ i, j };
    }
}

test "canonical order: I before D, ascending stripes, dedup by stripe, shared+exclusive on one stripe ⇒ exclusive (collision widening never under-locks)" {
    const io = testing.io;
    var m = try Manager.init(testing.allocator, 8);
    defer m.deinit();
    const pair = collidingInos(&m);
    const inodes = [_]InodeReq{
        .{ .ino = pair[0], .mode = .shared },
        .{ .ino = 1000, .mode = .exclusive },
        .{ .ino = pair[1], .mode = .exclusive },
        .{ .ino = 1000, .mode = .shared },
    };
    const dentries = [_]DentryReq{
        .{ .parent = 1, .name = "b", .mode = .exclusive },
        .{ .parent = 1, .name = "a", .mode = .shared },
    };
    var set = try m.lockMany(io, &inodes, &dentries);
    const guards = set.slice();
    // Distinct stripes only: 2 inode stripes (the colliding pair is one) and
    // the dentry stripes (one or two).
    const dentry_stripes: usize = if (m.dentryStripe(1, "a") == m.dentryStripe(1, "b")) 1 else 2;
    try testing.expectEqual(2 + dentry_stripes, guards.len);
    // I guards first, ascending; then D guards, ascending.
    const lo = @min(m.inodeStripe(pair[0]), m.inodeStripe(1000));
    const hi = @max(m.inodeStripe(pair[0]), m.inodeStripe(1000));
    try testing.expect(guards[0].lock == &m.inode.locks[lo]);
    try testing.expect(guards[1].lock == &m.inode.locks[hi]);
    for (guards[2..]) |g| {
        if (g.lock == &m.dentry.locks[m.dentryStripe(1, "b")]) {
            try testing.expectEqual(Mode.exclusive, g.mode);
        } else {
            try testing.expectEqual(Mode.shared, g.mode);
        }
    }
    if (dentry_stripes == 2) try testing.expect(@intFromPtr(guards[2].lock) < @intFromPtr(guards[3].lock));
    // Every inode guard is exclusive (the shared requests were widened), and
    // exclusive inode guards are hold-timed.
    for (guards[0..2]) |g| {
        try testing.expectEqual(Mode.exclusive, g.mode);
        try testing.expect(g.acquired_ns != 0);
    }
    // While held: both colliding inos AND the widened one refuse a shared try.
    try testing.expect(!m.inode.locks[m.inodeStripe(pair[0])].tryLockShared(io));
    try testing.expect(!m.inode.locks[m.inodeStripe(1000)].tryLockShared(io));
    try testing.expect(!m.dentry.locks[m.dentryStripe(1, "b")].tryLockShared(io));
    set.release(io);
    try testing.expectEqual(@as(usize, 0), set.slice().len);
    for (m.inode.locks) |*l| try testing.expectEqual(rwlock.RwLock.init, l.*);
    for (m.dentry.locks) |*l| try testing.expectEqual(rwlock.RwLock.init, l.*);
}

test "try_lock_many is all-or-nothing: a contended stripe anywhere in the plan releases what was taken" {
    const io = testing.io;
    var m = try Manager.init(testing.allocator, 8);
    defer m.deinit();
    const held = try m.lockDentry(io, 1, "busy", .exclusive);
    const inodes = [_]InodeReq{ .{ .ino = 1, .mode = .exclusive }, .{ .ino = 2, .mode = .shared } };
    const dentries = [_]DentryReq{.{ .parent = 1, .name = "busy", .mode = .shared }};
    try testing.expect((try m.tryLockMany(io, &inodes, &dentries)) == null);
    // Nothing is held: the inode stripes are free again.
    for (m.inode.locks) |*l| try testing.expectEqual(rwlock.RwLock.init, l.*);
    held.release(io);
    var set = (try m.tryLockMany(io, &inodes, &dentries)).?;
    try testing.expect(set.slice().len >= 2);
    set.release(io);
    for (m.dentry.locks) |*l| try testing.expectEqual(rwlock.RwLock.init, l.*);
}

test "a plan past the per-class bound is a refusal, never a stack overflow or an allocation" {
    const io = testing.io;
    var m = try Manager.init(testing.allocator, 8);
    defer m.deinit();
    var too_many: [max_requests + 1]InodeReq = undefined;
    for (&too_many, 0..) |*r, i| r.* = .{ .ino = i, .mode = .shared };
    try testing.expectError(error.PlanTooLarge, m.lockMany(io, &too_many, &.{}));
    try testing.expectError(error.PlanTooLarge, m.tryLockMany(io, &too_many, &.{}));
    for (m.inode.locks) |*l| try testing.expectEqual(rwlock.RwLock.init, l.*);
}

test "two-phase discovery: phase 1 locks, reads, DROPS; phase 2 re-locks the full canonical set and revalidates (never I after D)" {
    const io = testing.io;
    var m = try Manager.init(testing.allocator, 8);
    defer m.deinit();
    const parent: u64 = 1;
    // The child's ino collides with the parent's stripe — a per-ino second
    // acquisition would self-deadlock; the canonical set dedups it.
    var child: u64 = parent + 1;
    while (m.inodeStripe(child) != m.inodeStripe(parent)) child += 1;
    // Phase 1: the parent shared + the dentry shared; "read" the child.
    var p1 = try m.lockMany(io, &.{.{ .ino = parent, .mode = .shared }}, &.{.{ .parent = parent, .name = "victim", .mode = .shared }});
    const discovered = child;
    p1.release(io);
    // Phase 2: the full set, canonical — I{parent}, I{child} (one stripe,
    // exclusive) before D{parent,victim}.
    var p2 = try m.lockMany(io, &.{ .{ .ino = parent, .mode = .exclusive }, .{ .ino = discovered, .mode = .exclusive } }, &.{.{ .parent = parent, .name = "victim", .mode = .exclusive }});
    try testing.expectEqual(@as(usize, 2), p2.slice().len);
    try testing.expect(p2.slice()[0].lock == &m.inode.locks[m.inodeStripe(parent)]);
    try testing.expect(p2.slice()[1].lock == &m.dentry.locks[m.dentryStripe(parent, "victim")]);
    p2.release(io);
    for (m.inode.locks) |*l| try testing.expectEqual(rwlock.RwLock.init, l.*);
}

test "census closure: dlm_guard_wait.count ≡ Σ census; the contended arm classifies a different last acquirer as a collision and the same key as a key wait" {
    const io = testing.io;
    var m = try Manager.init(testing.allocator, 8);
    defer m.deinit();
    const pair = collidingInos(&m);
    const wait0 = lock_phase_dlm_guard_wait.snapshot().count;
    const coll0 = dlm_inode_stripe_collisions.load();
    const keyw0 = dlm_inode_key_waits.load();
    const Parked = struct {
        fn run(mgr: *Manager, ino: u64) void {
            const g = mgr.lockInode(testing.io, ino, .exclusive) catch unreachable;
            g.release(testing.io);
        }
    };
    // Uncontended: no sample, no census.
    const g0 = try m.lockInode(io, pair[0], .exclusive);
    try testing.expectEqual(wait0, lock_phase_dlm_guard_wait.snapshot().count);
    // A DIFFERENT key on the same stripe parks: one collision.
    const t1 = try std.Thread.spawn(.{}, Parked.run, .{ &m, pair[1] });
    while (m.inode.locks[m.inodeStripe(pair[0])].waiters(io) == 0) try std.Thread.yield();
    g0.release(io);
    t1.join();
    try testing.expectEqual(coll0 + 1, dlm_inode_stripe_collisions.load());
    try testing.expectEqual(keyw0, dlm_inode_key_waits.load());
    // The SAME key parks: one key wait.
    const g1 = try m.lockInode(io, pair[1], .exclusive);
    const t2 = try std.Thread.spawn(.{}, Parked.run, .{ &m, pair[1] });
    while (m.inode.locks[m.inodeStripe(pair[1])].waiters(io) == 0) try std.Thread.yield();
    g1.release(io);
    t2.join();
    try testing.expectEqual(coll0 + 1, dlm_inode_stripe_collisions.load());
    try testing.expectEqual(keyw0 + 1, dlm_inode_key_waits.load());
    try testing.expectEqual(wait0 + 2, lock_phase_dlm_guard_wait.snapshot().count);
    // Exclusive inode holds were timed: two guards released above.
    try testing.expect(lock_phase_dlm_guard_hold.snapshot().count >= 2);
}

test "no deadlock over random canonical plans across threads on a tiny table (every exclusive guard excludes, every shared guard coexists)" {
    const io = testing.io;
    const width = 8;
    const Storm = struct {
        m: *Manager,
        /// Per-stripe exclusive owner (thread id + 1) for the exclusion check.
        inode_owner: [width]std.atomic.Value(u32) = @splat(.init(0)),
        dentry_owner: [width]std.atomic.Value(u32) = @splat(.init(0)),
        done: std.atomic.Value(u32) = .init(0),
        failed: std.atomic.Value(bool) = .init(false),
        finished: Io.Event = .unset,
        const names = [_][]const u8{ "a", "b", "c", "d", "e", "f", "g", "h" };
        const threads = 8;
        const rounds = 400;

        fn owners(s: *@This(), g: Guard) *std.atomic.Value(u32) {
            const base_i = @intFromPtr(s.m.inode.locks.ptr);
            const p = @intFromPtr(g.lock);
            if (p >= base_i and p < base_i + width * @sizeOf(rwlock.RwLock)) {
                return &s.inode_owner[(p - base_i) / @sizeOf(rwlock.RwLock)];
            }
            const base_d = @intFromPtr(s.m.dentry.locks.ptr);
            return &s.dentry_owner[(p - base_d) / @sizeOf(rwlock.RwLock)];
        }

        fn run(s: *@This(), tid: u32) !void {
            var prng = std.Random.DefaultPrng.init(tid + 1);
            const rnd = prng.random();
            var r: usize = 0;
            while (r < rounds) : (r += 1) {
                var inodes: [4]InodeReq = undefined;
                const ni = rnd.intRangeAtMost(usize, 1, 4);
                for (inodes[0..ni]) |*q| q.* = .{ .ino = rnd.intRangeAtMost(u64, 1, 16), .mode = if (rnd.boolean()) .exclusive else .shared };
                var dentries: [3]DentryReq = undefined;
                const nd = rnd.intRangeAtMost(usize, 0, 3);
                for (dentries[0..nd]) |*q| q.* = .{ .parent = rnd.intRangeAtMost(u64, 1, 4), .name = names[rnd.intRangeAtMost(usize, 0, 7)], .mode = if (rnd.boolean()) .exclusive else .shared };
                var set = if (rnd.intRangeAtMost(u8, 0, 3) == 0)
                    ((try s.m.tryLockMany(testing.io, inodes[0..ni], dentries[0..nd])) orelse continue)
                else
                    try s.m.lockMany(testing.io, inodes[0..ni], dentries[0..nd]);
                for (set.slice()) |g| {
                    const o = s.owners(g);
                    switch (g.mode) {
                        .exclusive => try testing.expectEqual(@as(?u32, null), o.cmpxchgStrong(0, tid + 1, .acq_rel, .acquire)),
                        .shared => try testing.expectEqual(@as(u32, 0), o.load(.acquire)),
                    }
                }
                try std.Thread.yield();
                for (set.slice()) |g| {
                    const o = s.owners(g);
                    switch (g.mode) {
                        .exclusive => try testing.expectEqual(@as(?u32, null), o.cmpxchgStrong(tid + 1, 0, .acq_rel, .acquire)),
                        .shared => try testing.expectEqual(@as(u32, 0), o.load(.acquire)),
                    }
                }
                set.release(testing.io);
            }
        }

        fn worker(s: *@This(), tid: u32) Io.Cancelable!void {
            s.run(tid) catch s.failed.store(true, .release);
            if (s.done.fetchAdd(1, .acq_rel) + 1 == threads) s.finished.set(testing.io);
        }
    };
    var m = try Manager.init(testing.allocator, width);
    defer m.deinit();
    var storm: Storm = .{ .m = &m };
    var group: Io.Group = .init;
    var t: u32 = 0;
    while (t < Storm.threads) : (t += 1) {
        group.concurrent(io, Storm.worker, .{ &storm, t }) catch |err| switch (err) {
            error.ConcurrencyUnavailable => {
                group.cancel(io);
                return testkit.skip(@src(), .no_concurrency, "the executor refused a unit of concurrency");
            },
        };
    }
    // A deadlock is a loud failure, not a hang: the storm must finish well
    // inside this window (it takes well under a second). A spurious wake
    // reads as `error.Timeout` too, so the deadline is checked explicitly.
    const deadline = clock.monotonicNs() + 60 * std.time.ns_per_s;
    while (!storm.finished.isSet()) {
        const now = clock.monotonicNs();
        if (now >= deadline) {
            std.debug.print("dlm storm deadlocked: {d} of {d} workers finished\n", .{ storm.done.load(.acquire), Storm.threads });
            return error.DlmStormDeadlock;
        }
        storm.finished.waitTimeout(io, .{ .duration = .{ .raw = .fromNanoseconds(@intCast(deadline - now)), .clock = .awake } }) catch |err| switch (err) {
            error.Timeout => {},
            error.Canceled => |e| return e,
        };
    }
    try group.await(io);
    try testing.expect(!storm.failed.load(.acquire));
    for (m.inode.locks) |*l| try testing.expectEqual(rwlock.RwLock.init, l.*);
    for (m.dentry.locks) |*l| try testing.expectEqual(rwlock.RwLock.init, l.*);
}
