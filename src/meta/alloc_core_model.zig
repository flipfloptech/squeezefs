//! The exhaustive MODEL of `alloc_core.zig` (AGENTS.md "Model checks";
//! `docs/porting/kv_alloc_superblock.md` §4.7): every protocol of `ExtCore`
//! re-stated as a plain value type with explicit per-thread program counters,
//! ONE atomic action per `step`, explored by `model/explorer.zig` over every
//! schedule. Each micro-state names the production line it mirrors; the
//! differential test at the bottom runs both over the same sequential op
//! lists so a drift between core and model goes red.
//!
//! Five models — the single-owner invariants (format v4 deleted the
//! page-interleaved append partition, decision 0003 Q3, with its two
//! cross-appender models); each ships with the WEAKENING that breaks it
//! (`Weakening`), proven to fail with `expectFails`:
//!
//! | model | pins | weakening that fails it |
//! |---|---|---|
//! | 1 no double alloc | racing claimers never share an extent | `take_bit_load_store` |
//! | 2 coverage gate | a parked extent is never claimable before its gate is covered | `gate_off_by_one`, `release_budget_before_bit` |
//! | 3 headroom holds at push | `pending_has_room` observed true ⇒ the push succeeds | `full_on_stale_stamp` |
//! | 4 forced overflow | the overflow rides the same gate; extents conserved | `overflow_without_front_stop` |
//! | 5 reserve isolation | user claims never consume the reserve | `user_floor_strict` |

const std = @import("std");
const explorer = @import("../model/explorer.zig");
const alloc_core = @import("alloc_core.zig");

pub const Weakening = enum {
    none,
    /// `tryTakeBit` as a load then a store instead of one `fetchOr`.
    take_bit_load_store,
    /// `release` credits the budget BEFORE clearing the bit.
    release_budget_before_bit,
    /// The drain's front-stop compares off by one: releases `gate ≤ durable + 1`.
    gate_off_by_one,
    /// `freePending` reports full on a stale stamp without the cursor-distance check.
    full_on_stale_stamp,
    /// The overflow drains every entry regardless of its gate.
    overflow_without_front_stop,
    /// The user floor is checked with `<` instead of `≤`.
    user_floor_strict,
};

pub const Class = enum(u8) { user, internal };

/// One operation of a thread's program; the model runs each as a chain of
/// single-atomic-action micro-steps.
pub const Op = union(enum) {
    claim: struct { class: Class = .user },
    free_pending: struct { extent: u8, gate: u16 },
    free_pending_forced: struct { extent: u8, gate: u16 },
    /// `if pending_has_room() { free_pending(extent, gate) }` — the SMO
    /// task's admission probe then post-swap push.
    free_pending_if_room: struct { extent: u8, gate: u16 },
    advance: struct { seq: u16 },
    /// `freeExtents`: one load (a diagnostic, not a protocol action).
    read_free,
    read_durable,
    join: u8,
};

pub const Result = union(enum) {
    pending,
    claimed: struct { extent: u8, durable_seen: u16 },
    no_space,
    drift,
    parked,
    overflowed,
    full,
    skipped,
    released: u8,
    free: u8,
    durable: u16,
    joined,
};

pub const Config = struct {
    total: u8,
    reserve: u8,
    cap: u8,
    /// Run single-threaded to completion in `init`.
    setup: []const Op,
    programs: []const []const Op,
    weakening: Weakening = .none,
};

const max_extents = 8;
const max_cap = 2;
const max_threads = 2;
const max_ops = 8;
const max_overflow = 3;

const Micro = enum(u8) {
    start,
    // claim (ExtCore.claim)
    c_cas,
    c_hint,
    c_scan,
    c_scan_store,
    c_store_hint,
    c_read_durable,
    c_drift_refund,
    // free_pending (ExtCore.freePending) + the forced overflow push
    p_fetch_max,
    p_load_head,
    p_load_stamp,
    p_cas_head,
    p_store_extent,
    p_store_gate,
    p_store_stamp,
    p_load_tail,
    o_lock,
    o_push,
    o_len,
    o_unlock,
    // pending_has_room
    h_load_head,
    h_load_tail,
    // advance_durable (ExtCore.advanceDurable)
    a_fetch_max,
    a_load_durable,
    a_load_tail,
    a_load_head,
    a_load_stamp,
    a_load_gate,
    a_load_extent,
    a_cas_tail,
    a_store_vacate,
    a_release,
    a_ovf_len,
    a_ovf_lock,
    a_ovf_check,
    a_ovf_release,
    a_ovf_store_len,
    a_ovf_unlock,
};

const Slot = struct { stamp: u16 = 0, extent: u8 = 0, gate: u16 = 0 };
const Entry = struct { extent: u8 = 0, gate: u16 = 0 };

/// The one owner's allocator state (the production `ExtCore` minus its bits).
const Core = struct {
    budget: u8 = 0,
    hint: u8 = 0,
    durable: u16 = 0,
    last_retire: u16 = 0,
    slots: [max_cap]Slot = @splat(.{}),
    head: u16 = 0,
    tail: u16 = 0,
    /// The overflow deque (mutex-guarded in production).
    overflow: [max_overflow]Entry = @splat(.{}),
    ovf_len: u8 = 0,
    /// The lock-free length mirror.
    overflow_len: u8 = 0,
    lock: bool = false,
};

const Thread = struct {
    op: u8 = 0,
    micro: Micro = .start,
    pos: u16 = 0,
    stamp: u16 = 0,
    head: u16 = 0,
    tail: u16 = 0,
    free: u8 = 0,
    start: u8 = 0,
    k: u8 = 0,
    pass: u8 = 0,
    durable: u16 = 0,
    gate: u16 = 0,
    extent: u8 = 0,
    idx: u8 = 0,
    count: u8 = 0,
    bit: bool = false,
    rel_step: u8 = 0,
    scan_len: u8 = 0,
    scan: [max_extents]u8 = @splat(0),
    results: [max_ops]Result = @splat(.pending),
};

pub fn Model(comptime cfg: Config, comptime Checks: type) type {
    comptime {
        std.debug.assert(cfg.total <= max_extents);
        std.debug.assert(cfg.cap <= max_cap and cfg.cap >= alloc_core.min_pending_cap);
        std.debug.assert(cfg.programs.len <= max_threads);
        std.debug.assert(cfg.setup.len <= max_ops);
        for (cfg.programs) |p| std.debug.assert(p.len <= max_ops);
    }
    const W = cfg.weakening;
    const setup_thread: usize = cfg.programs.len;

    return struct {
        const Self = @This();
        pub const thread_count: usize = cfg.programs.len;
        pub const config = cfg;

        words: u64 = 0,
        core: Core = .{},
        threads: [max_threads + 1]Thread = @splat(.{}),

        pub fn init() Self {
            var m: Self = .{};
            m.core.budget = cfg.total;
            var i: u8 = 0;
            while (i < cfg.cap) : (i += 1) m.core.slots[i].stamp = i;
            while (!m.done(setup_thread)) m.step(setup_thread);
            return m;
        }

        pub fn key(self: *const Self) u64 {
            var h = std.hash.Wyhash.init(0);
            std.hash.autoHash(&h, self.*);
            return h.final();
        }

        fn program(t: usize) []const Op {
            return if (t == setup_thread) cfg.setup else cfg.programs[t];
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.threads[t].op == program(t).len;
        }

        pub fn setupResults(self: *const Self) []const Result {
            return self.threads[setup_thread].results[0..cfg.setup.len];
        }

        pub fn results(self: *const Self, t: usize) []const Result {
            return self.threads[t].results[0..cfg.programs[t].len];
        }

        fn bitSet(self: *const Self, idx: u8) bool {
            return self.words & (@as(u64, 1) << @intCast(idx)) != 0;
        }

        fn setBit(self: *Self, idx: u8) void {
            self.words |= @as(u64, 1) << @intCast(idx);
        }

        fn clearBit(self: *Self, idx: u8) void {
            self.words &= ~(@as(u64, 1) << @intCast(idx));
        }

        pub fn freeExtents(self: *const Self) u64 {
            return self.core.budget;
        }

        pub fn pendingCount(self: *const Self) u64 {
            return (self.core.head - self.core.tail) + self.core.overflow_len;
        }

        pub fn isAllocated(self: *const Self, extent: u8) bool {
            return self.bitSet(extent);
        }

        fn finish(self: *Self, t: usize, result: Result) void {
            const th = &self.threads[t];
            const op = th.op;
            th.* = .{ .op = op + 1, .results = th.results };
            th.results[op] = result;
        }

        fn firstMicro(op: Op) Micro {
            return switch (op) {
                .claim => .c_cas,
                .free_pending, .free_pending_forced => .p_fetch_max,
                .free_pending_if_room => .h_load_head,
                .advance => .a_fetch_max,
                .read_free, .read_durable, .join => .start,
            };
        }

        pub fn step(self: *Self, t: usize) void {
            const th = &self.threads[t];
            const op = program(t)[th.op];
            if (th.micro == .start) {
                th.micro = firstMicro(op);
                // The first micro of a claim is the budget LOAD.
                if (op == .claim) {
                    th.free = self.core.budget;
                    return;
                }
            }
            switch (op) {
                .claim => |c| self.stepClaim(t, c.class),
                .free_pending => |f| self.stepFreePending(t, f.extent, f.gate, false),
                .free_pending_forced => |f| self.stepFreePending(t, f.extent, f.gate, true),
                .free_pending_if_room => |f| self.stepIfRoom(t, f.extent, f.gate),
                .advance => |a| self.stepAdvance(t, a.seq),
                .read_free => self.finish(t, .{ .free = @intCast(self.freeExtents()) }),
                .read_durable => self.finish(t, .{ .durable = self.core.durable }),
                .join => |other| if (self.done(other)) self.finish(t, .joined),
            }
        }

        // ---- claim: ExtCore.claim ----

        /// The flat sweep: `start..total` then `0..start`.
        fn buildScanOrder(self: *Self, t: usize) void {
            const th = &self.threads[t];
            const start: u8 = th.start;
            var k: u8 = 0;
            while (k < cfg.total) : (k += 1) {
                const s: u16 = @as(u16, start) + k;
                th.scan[k] = @intCast(if (s < cfg.total) s else s - cfg.total);
            }
            th.scan_len = cfg.total;
        }

        fn advanceScan(self: *Self, t: usize) void {
            const th = &self.threads[t];
            th.micro = .c_scan;
            th.k += 1;
            if (th.k == th.scan_len) {
                th.k = 0;
                th.pass += 1;
                if (th.pass == alloc_core.claim_rescan_limit) th.micro = .c_drift_refund;
            }
        }

        fn stepClaim(self: *Self, t: usize, class: Class) void {
            const th = &self.threads[t];
            const c = &self.core;
            switch (th.micro) {
                .c_cas => {
                    // `free <= floor ⇒ NoSpace`, else CAS(free → free − 1).
                    const floor: u8 = if (class == .user) cfg.reserve else 0;
                    // The weakened floor keeps the empty-budget guard: the bug
                    // under test is the reserve breach, not an underflow.
                    const refused = if (W == .user_floor_strict) th.free == 0 or th.free < floor else th.free <= floor;
                    if (refused) return self.finish(t, .no_space);
                    if (c.budget == th.free) {
                        c.budget -= 1;
                        th.micro = .c_hint;
                    } else {
                        th.free = c.budget;
                    }
                },
                .c_hint => {
                    // `start = hint.load(Relaxed).min(total)`.
                    th.start = @min(c.hint, cfg.total);
                    th.k = 0;
                    th.pass = 0;
                    self.buildScanOrder(t);
                    th.micro = .c_scan;
                },
                .c_scan => {
                    const idx = th.scan[th.k];
                    if (W == .take_bit_load_store) {
                        // The weakened take: a plain load now, a store next step.
                        th.bit = self.bitSet(idx);
                        th.idx = idx;
                        th.micro = .c_scan_store;
                        return;
                    }
                    // `fetch_or(mask) & mask == 0` — the unique 0→1 winner.
                    if (self.bitSet(idx)) return self.advanceScan(t);
                    self.setBit(idx);
                    th.idx = idx;
                    th.micro = .c_store_hint;
                },
                .c_scan_store => {
                    if (th.bit) return self.advanceScan(t);
                    self.setBit(th.idx);
                    th.micro = .c_store_hint;
                },
                .c_store_hint => {
                    // `hint.store(idx + 1, Relaxed)`.
                    c.hint = th.idx + 1;
                    th.micro = .c_read_durable;
                },
                .c_read_durable => {
                    // The model's post-claim `durable_seq()` read (one load).
                    self.finish(t, .{ .claimed = .{ .extent = th.idx, .durable_seen = c.durable } });
                },
                .c_drift_refund => {
                    // RES-14: `free_budget.fetch_add(1)` then InvariantDrift.
                    c.budget += 1;
                    self.finish(t, .drift);
                },
                else => unreachable,
            }
        }

        // ---- release: ExtCore.release, one atomic per call; true when complete ----

        fn releaseStep(self: *Self, t: usize, extent: u8) bool {
            const th = &self.threads[t];
            const c = &self.core;
            if (W == .release_budget_before_bit) {
                switch (th.rel_step) {
                    0 => c.budget += 1,
                    1 => self.clearBit(extent),
                    else => {
                        c.hint = @min(c.hint, extent);
                        return true;
                    },
                }
                th.rel_step += 1;
                return false;
            }
            switch (th.rel_step) {
                0 => {
                    // `fetch_and(!mask) & mask != 0`: clear first, credit after.
                    const was_set = self.bitSet(extent);
                    self.clearBit(extent);
                    if (!was_set) return true;
                },
                1 => c.budget += 1,
                else => {
                    // `hint.fetch_min(extent, Relaxed)`.
                    c.hint = @min(c.hint, extent);
                    return true;
                },
            }
            th.rel_step += 1;
            return false;
        }

        // ---- free_pending: ExtCore.freePending / freePendingForced ----

        fn stepIfRoom(self: *Self, t: usize, extent: u8, gate: u16) void {
            const th = &self.threads[t];
            const p = &self.core;
            switch (th.micro) {
                .h_load_head => {
                    th.head = p.head;
                    th.micro = .h_load_tail;
                },
                .h_load_tail => {
                    th.tail = p.tail;
                    // `(head − tail) < cap`.
                    if (th.head - th.tail < cfg.cap) {
                        th.micro = .p_fetch_max;
                    } else {
                        self.finish(t, .skipped);
                    }
                },
                else => self.stepFreePending(t, extent, gate, false),
            }
        }

        fn stepFreePending(self: *Self, t: usize, extent: u8, gate: u16, forced: bool) void {
            const th = &self.threads[t];
            const p = &self.core;
            switch (th.micro) {
                .p_fetch_max => {
                    std.debug.assert(self.bitSet(extent));
                    std.debug.assert(p.last_retire <= gate);
                    p.last_retire = @max(p.last_retire, gate);
                    th.micro = .p_load_head;
                },
                .p_load_head => {
                    th.pos = p.head;
                    th.micro = .p_load_stamp;
                },
                .p_load_stamp => {
                    const slot = &p.slots[th.pos % cfg.cap];
                    th.stamp = slot.stamp;
                    if (th.stamp == th.pos) {
                        th.micro = .p_cas_head;
                    } else if (th.stamp < th.pos) {
                        // The lap-old stamp: full, or a consumer mid-vacate.
                        if (W == .full_on_stale_stamp) return self.fullOutcome(t, forced);
                        th.micro = .p_load_tail;
                    } else {
                        th.micro = .p_load_head;
                    }
                },
                .p_cas_head => {
                    if (p.head == th.pos) {
                        p.head = th.pos + 1;
                        th.micro = .p_store_extent;
                    } else {
                        th.pos = p.head;
                        th.micro = .p_load_stamp;
                    }
                },
                .p_store_extent => {
                    p.slots[th.pos % cfg.cap].extent = extent;
                    th.micro = .p_store_gate;
                },
                .p_store_gate => {
                    p.slots[th.pos % cfg.cap].gate = gate;
                    th.micro = .p_store_stamp;
                },
                .p_store_stamp => {
                    // `stamp.store(pos + 1, Release)` publishes the entry.
                    p.slots[th.pos % cfg.cap].stamp = th.pos + 1;
                    self.finish(t, .parked);
                },
                .p_load_tail => {
                    // The cursor-distance check: `pos − tail ≥ cap ⇒ full`,
                    // else spin and re-read the head.
                    th.tail = p.tail;
                    if (th.pos -| th.tail >= cfg.cap) return self.fullOutcome(t, forced);
                    th.micro = .p_load_head;
                },
                .o_lock => {
                    if (!p.lock) {
                        p.lock = true;
                        th.micro = .o_push;
                    }
                },
                .o_push => {
                    if (p.ovf_len > 0) std.debug.assert(p.overflow[p.ovf_len - 1].gate <= gate);
                    p.overflow[p.ovf_len] = .{ .extent = extent, .gate = gate };
                    p.ovf_len += 1;
                    th.micro = .o_len;
                },
                .o_len => {
                    p.overflow_len = p.ovf_len;
                    th.micro = .o_unlock;
                },
                .o_unlock => {
                    p.lock = false;
                    self.finish(t, .overflowed);
                },
                else => unreachable,
            }
        }

        fn fullOutcome(self: *Self, t: usize, forced: bool) void {
            if (forced) {
                self.threads[t].micro = .o_lock;
            } else {
                self.finish(t, .full);
            }
        }

        // ---- advance_durable: ExtCore.advanceDurable ----

        fn stepAdvance(self: *Self, t: usize, seq: u16) void {
            const th = &self.threads[t];
            const p = &self.core;
            switch (th.micro) {
                .a_fetch_max => {
                    p.durable = @max(p.durable, seq);
                    th.micro = .a_load_durable;
                },
                .a_load_durable => {
                    th.durable = p.durable;
                    th.count = 0;
                    th.micro = .a_load_tail;
                },
                .a_load_tail => {
                    th.pos = p.tail;
                    th.micro = .a_load_head;
                },
                .a_load_head => {
                    th.head = p.head;
                    th.micro = if (th.pos == th.head) .a_ovf_len else .a_load_stamp;
                },
                .a_load_stamp => {
                    th.stamp = p.slots[th.pos % cfg.cap].stamp;
                    // Unpublished producer ⇒ stop.
                    th.micro = if (th.stamp != th.pos + 1) .a_ovf_len else .a_load_gate;
                },
                .a_load_gate => {
                    th.gate = p.slots[th.pos % cfg.cap].gate;
                    const uncovered = if (W == .gate_off_by_one) th.gate > th.durable + 1 else th.gate > th.durable;
                    th.micro = if (uncovered) .a_ovf_len else .a_load_extent;
                },
                .a_load_extent => {
                    th.extent = p.slots[th.pos % cfg.cap].extent;
                    th.micro = .a_cas_tail;
                },
                .a_cas_tail => {
                    if (p.tail == th.pos) {
                        p.tail = th.pos + 1;
                        th.micro = .a_store_vacate;
                    } else {
                        th.micro = .a_load_tail;
                    }
                },
                .a_store_vacate => {
                    // `stamp.store(pos + cap, Release)` BEFORE the release.
                    p.slots[th.pos % cfg.cap].stamp = th.pos + cfg.cap;
                    th.rel_step = 0;
                    th.micro = .a_release;
                },
                .a_release => {
                    if (self.releaseStep(t, th.extent)) {
                        th.count += 1;
                        th.micro = .a_load_tail;
                    }
                },
                .a_ovf_len => {
                    if (p.overflow_len == 0) return self.finish(t, .{ .released = th.count });
                    th.micro = .a_ovf_lock;
                },
                .a_ovf_lock => {
                    if (!p.lock) {
                        p.lock = true;
                        th.micro = .a_ovf_check;
                    }
                },
                .a_ovf_check => {
                    // The same front-stop as the FIFO: `gate > durable ⇒ stop`.
                    const stop = p.ovf_len == 0 or (W != .overflow_without_front_stop and p.overflow[0].gate > th.durable);
                    if (stop) {
                        th.micro = .a_ovf_store_len;
                        return;
                    }
                    th.extent = p.overflow[0].extent;
                    var i: u8 = 1;
                    while (i < p.ovf_len) : (i += 1) p.overflow[i - 1] = p.overflow[i];
                    p.overflow[p.ovf_len - 1] = .{};
                    p.ovf_len -= 1;
                    th.rel_step = 0;
                    th.micro = .a_ovf_release;
                },
                .a_ovf_release => {
                    if (self.releaseStep(t, th.extent)) {
                        th.count += 1;
                        th.micro = .a_ovf_check;
                    }
                },
                .a_ovf_store_len => {
                    p.overflow_len = p.ovf_len;
                    th.micro = .a_ovf_unlock;
                },
                .a_ovf_unlock => {
                    p.lock = false;
                    self.finish(t, .{ .released = th.count });
                },
                else => unreachable,
            }
        }

        // ---- the invariants every model shares + the model's own ----

        /// After every step: no claim ever reports InvariantDrift under a
        /// legal interleaving, and no two of the racing threads' claims hold
        /// the same extent (the setup's seed claims are parked and drained
        /// before the race, so they are legitimately re-claimable).
        pub fn invariant(self: *const Self) !void {
            var holders: [max_extents]u8 = @splat(0);
            var t: usize = 0;
            while (t <= thread_count) : (t += 1) {
                for (self.threads[t].results) |r| switch (r) {
                    .drift => return error.InvariantDriftReached,
                    .claimed => |c| if (t < thread_count) {
                        holders[c.extent] += 1;
                        if (holders[c.extent] > 1) return error.DoubleAllocation;
                    },
                    else => {},
                };
            }
            try Checks.invariant(self);
        }

        pub fn final(self: *const Self) !void {
            try Checks.final(self);
        }

        /// Claimed extents of thread `t`, in op order.
        pub fn claimedOf(self: *const Self, t: usize, out: *[max_ops]u8) []const u8 {
            var n: usize = 0;
            for (self.results(t)) |r| if (r == .claimed) {
                out[n] = r.claimed.extent;
                n += 1;
            };
            return out[0..n];
        }
    };
}

const testing = std.testing;
const expectHolds = explorer.expectHolds;
const expectFails = explorer.expectFails;

fn claimOp(class: Class) Op {
    return .{ .claim = .{ .class = class } };
}

const NoExtraInvariant = struct {
    pub fn invariant(_: anytype) !void {}
};

// ---- model 1: no double alloc under concurrent claimers ----

const m1_setup = [_]Op{ claimOp(.user), .{ .free_pending = .{ .extent = 0, .gate = 1 } }, .{ .advance = .{ .seq = 1 } } };
const m1_prog = [_]Op{ claimOp(.user), claimOp(.user) };

const Model1Checks = struct {
    pub fn invariant(m: anytype) !void {
        // Setup: the seed claim parked at gate 1 drained at durable 1.
        const s = m.setupResults();
        if (s[0] != .claimed or s[0].claimed.extent != 0) return error.SetupSeedClaim;
        if (s[2] != .released or s[2].released != 1) return error.SetupDrainReleasedSeed;
    }
    pub fn final(m: anytype) !void {
        var buf0: [max_ops]u8 = undefined;
        var buf1: [max_ops]u8 = undefined;
        const n = m.claimedOf(0, &buf0).len + m.claimedOf(1, &buf1).len;
        if (n != 3) return error.NotExactlyTheHeap;
        if (m.freeExtents() != 0) return error.BudgetDidNotSettle;
    }
};

fn model1(comptime w: Weakening) type {
    return Model(.{ .total = 3, .reserve = 0, .cap = 2, .setup = &m1_setup, .programs = &.{ &m1_prog, &m1_prog }, .weakening = w }, Model1Checks);
}

test "model 1: racing claimers never share an extent, including a just-drained pending one; fails if the bit take is a load+store" {
    try expectHolds(model1(.none), .{});
    try expectFails(model1(.take_bit_load_store), .{}, error.InvariantViolated);
}

// ---- model 2: a pending-freed extent is never claimable before the covering tail ----

const m2_setup = [_]Op{ claimOp(.user), .{ .free_pending = .{ .extent = 0, .gate = 7 } }, .{ .advance = .{ .seq = 6 } } };
const m2_main = [_]Op{ claimOp(.user), .{ .join = 1 }, claimOp(.user) };
const m2_ckpt = [_]Op{.{ .advance = .{ .seq = 8 } }};

const Model2Checks = struct {
    pub fn invariant(m: anytype) !void {
        // A durable tail BELOW the free record (6 < 7) releases nothing:
        // record durability is not flip coverage.
        const s = m.setupResults();
        if (s[2] != .released or s[2].released != 0) return error.TailBelowFreeRecordReleased;
        // Success PROVES the clock had covered gate 7.
        for (m.results(0)) |r| if (r == .claimed) {
            if (r.claimed.extent != 0) return error.NotTheDrainedExtent;
            if (r.claimed.durable_seen < 7) return error.ReusedBeforeCoverage;
        };
    }
    pub fn final(m: anytype) !void {
        const r = m.results(0);
        const raced = r[0] == .claimed;
        const retry = r[2] == .claimed;
        if (raced == retry) return error.ClaimedNotExactlyOnce;
        if (m.freeExtents() != 0) return error.BudgetDidNotSettle;
    }
};

fn model2(comptime w: Weakening) type {
    return Model(.{ .total = 1, .reserve = 0, .cap = 2, .setup = &m2_setup, .programs = &.{ &m2_main, &m2_ckpt }, .weakening = w }, Model2Checks);
}

test "model 2: the coverage gate — never claimable before the tail passes the free record; fails with an off-by-one gate or budget-before-bit release" {
    try expectHolds(model2(.none), .{});
    try expectFails(model2(.gate_off_by_one), .{}, error.InvariantViolated);
    try expectFails(model2(.release_budget_before_bit), .{}, error.InvariantViolated);
}

// ---- model 3: headroom observed at admission holds at the post-swap push ----

const m34_setup = [_]Op{ claimOp(.user), claimOp(.user), claimOp(.user) };
const m3_setup = m34_setup ++ [_]Op{ .{ .free_pending = .{ .extent = 0, .gate = 3 } }, .{ .free_pending = .{ .extent = 1, .gate = 5 } } };
const m3_main = [_]Op{.{ .free_pending_if_room = .{ .extent = 2, .gate = 9 } }};
const m3_ckpt = [_]Op{.{ .advance = .{ .seq = 4 } }};

const Model3Checks = struct {
    pub fn invariant(m: anytype) !void {
        // Headroom observed true was invalidated before the push: the
        // clause-a soundness argument is broken.
        if (m.results(0)[0] == .full) return error.HeadroomInvalidatedBeforePush;
    }
    pub fn final(m: anytype) !void {
        const r = m.results(0)[0];
        if (r != .parked and r != .skipped) return error.UnexpectedOutcome;
    }
};

fn model3(comptime w: Weakening) type {
    return Model(.{ .total = 3, .reserve = 0, .cap = 2, .setup = &m3_setup, .programs = &.{ &m3_main, &m3_ckpt }, .weakening = w }, Model3Checks);
}

test "model 3: headroom observed at admission holds at the push against a racing drain; fails if a stale stamp reads full mid-vacate" {
    try expectHolds(model3(.none), .{});
    try expectFails(model3(.full_on_stale_stamp), .{}, error.InvariantViolated);
}

// ---- model 4: forced overflow rides the same gate; extents conserved ----

// The forced retirement is extent 0 — the LOWEST — so a premature release
// is the next claim's first find (lowest-first) and the model can see it.
const m4_setup = m34_setup ++ [_]Op{ .{ .free_pending = .{ .extent = 1, .gate = 3 } }, .{ .free_pending = .{ .extent = 2, .gate = 5 } } };
const m4_main = [_]Op{
    .{ .free_pending_forced = .{ .extent = 0, .gate = 7 } },
    claimOp(.user),
    claimOp(.user),
    .{ .join = 1 },
    .{ .advance = .{ .seq = 8 } },
    claimOp(.user),
    claimOp(.user),
    claimOp(.user),
};
const m4_ckpt = [_]Op{.{ .advance = .{ .seq = 6 } }};

const Model4Checks = struct {
    pub fn invariant(m: anytype) !void {
        const r = m.results(0);
        for (r[1..3]) |c| if (c == .claimed) {
            // Never the forced one before its gate (7) is covered.
            if (c.claimed.extent == 0) return error.ForcedRetirementClaimedEarly;
            const gate: u16 = if (c.claimed.extent == 1) 3 else 5;
            if (c.claimed.durable_seen < gate) return error.ReusedBeforeCoverage;
        };
    }
    pub fn final(m: anytype) !void {
        const r = m.results(0);
        if (r[0] != .parked and r[0] != .overflowed) return error.ForcedPushRefused;
        var buf: [max_ops]u8 = undefined;
        if (m.claimedOf(0, &buf).len != 3) return error.ExtentsNotConserved;
        if (m.pendingCount() != 0) return error.SomethingStaysParked;
        if (m.freeExtents() != 0) return error.BudgetDidNotSettle;
    }
};

fn model4(comptime w: Weakening) type {
    return Model(.{ .total = 3, .reserve = 0, .cap = 2, .setup = &m4_setup, .programs = &.{ &m4_main, &m4_ckpt }, .weakening = w }, Model4Checks);
}

test "model 4: a forced retirement at cap overflows, rides the same gate and conserves extents; fails if the overflow drains without the front-stop" {
    try expectHolds(model4(.none), .{});
    try expectFails(model4(.overflow_without_front_stop), .{}, error.InvariantViolated);
}

// ---- model 5: the reserve is isolated from user claims ----

const m5_main = [_]Op{ claimOp(.user), .{ .join = 1 }, .read_free, claimOp(.internal), claimOp(.user), claimOp(.internal), claimOp(.internal) };
const m5_peer = [_]Op{claimOp(.user)};

const Model5Checks = struct {
    pub fn invariant(_: anytype) !void {}
    pub fn final(m: anytype) !void {
        const r = m.results(0);
        const mine = r[0] == .claimed;
        const theirs = m.results(1)[0] == .claimed;
        if (mine == theirs) return error.UserBudgetNotExactlyOne;
        if (r[2] != .free or r[2].free != 2) return error.ReserveConsumedByUser;
        if (r[3] != .claimed) return error.InternalMustReachReserve;
        if (r[4] != .no_space) return error.UserRetryConsumedReserve;
        if (r[5] != .claimed) return error.WholeReserveInternalClaimable;
        if (r[6] != .no_space) return error.ExhaustedHeapAdmittedInternal;
        if (m.freeExtents() != 0) return error.BudgetDidNotSettle;
    }
};

fn model5(comptime w: Weakening) type {
    return Model(.{ .total = 3, .reserve = 2, .cap = 2, .setup = &.{}, .programs = &.{ &m5_main, &m5_peer }, .weakening = w }, Model5Checks);
}

test "model 5: exactly one of two racing user claims fits above the reserve, internal drains it; fails if the floor is checked with <" {
    try expectHolds(model5(.none), .{});
    try expectFails(model5(.user_floor_strict), .{}, error.FinalViolated);
}

// ---- the differential test: model ≡ production over sequential op lists ----

fn noRelease(_: void, _: u64) void {}

fn runProduction(core: *const alloc_core.ExtCore, ops: []const Op, out: []Result) !void {
    for (ops, 0..) |op, i| {
        out[i] = switch (op) {
            .claim => |c| blk: {
                const class: alloc_core.AllocClass = if (c.class == .user) .user else .internal;
                const e = core.claim(class) catch |err| switch (err) {
                    error.NoSpace => break :blk .no_space,
                    error.InvariantDrift => break :blk .drift,
                };
                break :blk .{ .claimed = .{ .extent = @intCast(e), .durable_seen = @intCast(core.durableSeq()) } };
            },
            .free_pending => |f| blk: {
                core.freePending(f.extent, f.gate) catch break :blk .full;
                break :blk .parked;
            },
            .free_pending_forced => |f| if (try core.freePendingForced(f.extent, f.gate)) .overflowed else .parked,
            .free_pending_if_room => |f| blk: {
                if (!core.pendingHasRoom()) break :blk .skipped;
                core.freePending(f.extent, f.gate) catch break :blk .full;
                break :blk .parked;
            },
            .advance => |a| .{ .released = @intCast(core.advanceDurable(a.seq, {}, noRelease)) },
            .read_free => .{ .free = @intCast(core.freeExtents()) },
            .read_durable => .{ .durable = @intCast(core.durableSeq()) },
            .join => .joined,
        };
    }
}

fn expectSameTrace(comptime M: type) !void {
    const cfg = M.config;
    // The model: setup ran in init; then thread 1 to completion, then thread 0
    // (so a join on 1 never blocks).
    var m = M.init();
    var t: usize = cfg.programs.len;
    while (t > 0) {
        t -= 1;
        while (!m.done(t)) m.step(t);
    }
    // Production over the same sequence.
    var core = try alloc_core.ExtCore.init(testing.allocator, cfg.total, cfg.reserve, cfg.cap);
    defer core.deinit();
    var got: [max_ops]Result = undefined;
    try runProduction(&core, cfg.setup, &got);
    try testing.expectEqualSlices(Result, m.setupResults(), got[0..cfg.setup.len]);
    t = cfg.programs.len;
    while (t > 0) {
        t -= 1;
        try runProduction(&core, cfg.programs[t], &got);
        try testing.expectEqualSlices(Result, m.results(t), got[0..cfg.programs[t].len]);
    }
    try testing.expectEqual(m.freeExtents(), core.freeExtents());
    try testing.expectEqual(m.pendingCount(), core.pendingCount());
}

test "the model and the production core agree on every sequential trace of the five models" {
    try expectSameTrace(model1(.none));
    try expectSameTrace(model2(.none));
    try expectSameTrace(model3(.none));
    try expectSameTrace(model4(.none));
    try expectSameTrace(model5(.none));
}

test "model state counts stay within the explorer's default limits" {
    inline for (.{ model1(.none), model2(.none), model3(.none), model4(.none), model5(.none) }) |M| {
        var report: explorer.Report = undefined;
        try explorer.explore(M, testing.allocator, .{}, &report);
        try testing.expect(report.states > 1);
        try testing.expect(report.max_depth_seen < 256);
    }
}
