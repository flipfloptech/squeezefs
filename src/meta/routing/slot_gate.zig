//! The per-slot cutover gate (charter §5.4 "per-slot cutover gate checked
//! at op entry BEFORE any lock (parked ops hold nothing)"; `docs/porting/
//! dynamic_routing.md` §3.4, §7 Q8, Q10). PORTABLE: atomics over the
//! `sync.Notify` primitive.
//!
//! The protocol is the house Dekker idiom with a per-slot in-flight word:
//!
//!     op:      inflight[s].fetch_add(1)   ; load(closed) → closed for s ⇒ back out, park
//!     closer:  store(closed = s)           ; load(inflight[s]) == 0 ⇒ drained
//!
//! The four accesses are themselves SeqCst (Zig 0.17 has no standalone
//! fence; `fuse/transport/slot.zig`'s law: C11's one total order over
//! seq_cst operations forbids both sides reading the pre-race value — an
//! SC store IS a drained store), so the store-load window of TSO cannot
//! let an op slip past a closing gate while the closer reads the slot as
//! drained (`slot_gate_model.zig` keeps the fence as an explicit store-
//! buffer drain step and proves the weakening fails). The in-flight word
//! is kept for EVERY op,
//! armed or not (one RMW on the op's slot word — the 4a guards already pay
//! a per-inode RMW), so the drain covers an op admitted before the gate
//! was armed: it entered its slot's word, and the closer waits for that
//! word to reach zero.
//!
//! Position (charter §2 lock order): the op enters BEFORE its 4a guards
//! and exits at its terminal outcome, so a parked op holds no DLM guard
//! and no node lock; the cutover task never acquires lattice 1–3 locks and
//! never waits on a parked op — no cycle. A park is planned and bounded
//! (`routing_gate_parks`), never an escalation.
//!
//! Waiting is register-then-check on BOTH sides (no polling, no lost
//! wake): a parked op registers on `waiters` then re-tries `tryEnter`;
//! the closer registers on `drained` then re-reads the word. The last
//! `exit` of a closed slot and `reopen` are the notifiers.

const std = @import("std");
const Io = std.Io;
const Allocator = std.mem.Allocator;
const sync = @import("../../sync/sync.zig");
const route_table = @import("route_table.zig");
const rstats = @import("stats.zig");

const assert = std.debug.assert;

const W = route_table.W;

/// `closed_slot`'s encoding: 0 = open; `s + 1` = slot `s` is closed.
const OPEN: u32 = 0;

pub const SlotGates = struct {
    gpa: Allocator,
    /// One in-flight word per routing slot (256 KiB): the op side's RMW.
    inflight: []std.atomic.Value(u32),
    /// The ONE closed slot (+1), or `OPEN`: migrations are serial, so at
    /// most one gate is closed per set.
    closed_slot: std.atomic.Value(u32) = .init(OPEN),
    /// Parked ops.
    waiters: sync.Notify = .init,
    /// The cutover task, waiting for the closed slot's word to drain.
    drained: sync.Notify = .init,

    pub fn init(gpa: Allocator) Allocator.Error!SlotGates {
        const words = try gpa.alloc(std.atomic.Value(u32), W);
        for (words) |*w| w.* = .init(0);
        return .{ .gpa = gpa, .inflight = words };
    }

    pub fn deinit(self: *SlotGates) void {
        self.gpa.free(self.inflight);
        self.* = undefined;
    }

    /// The op side's one attempt: count in, fence, read the closed word.
    /// False = the slot is closing — the count is backed out and the
    /// caller parks.
    pub fn tryEnter(self: *SlotGates, slot: u16) bool {
        // SeqCst RMW then SeqCst load: the Dekker store side (module doc).
        _ = self.inflight[slot].fetchAdd(1, .seq_cst);
        if (self.closed_slot.load(.seq_cst) == @as(u32, slot) + 1) {
            self.exit(slot, null);
            return false;
        }
        return true;
    }

    /// The op's terminal outcome: count out; the last one out of a closed
    /// slot wakes the drain waiter (`io` null = a back-out inside
    /// `tryEnter`, which never completes a drain the closer observes late:
    /// the closer's own fence + load sees the word).
    pub fn exit(self: *SlotGates, slot: u16, io: ?Io) void {
        // SeqCst: the closer's SeqCst load of the word sees this decrement
        // or this exit sees its close — never neither.
        const prev = self.inflight[slot].fetchSub(1, .seq_cst);
        assert(prev >= 1);
        if (prev == 1) {
            if (self.closed_slot.load(.seq_cst) == @as(u32, slot) + 1) {
                if (io) |i| self.drained.notifyWaiters(i);
            }
        }
    }

    /// Enter slot `s`, parking while its gate is closed (register-then-
    /// check: the registration precedes the retry, so a `reopen` between
    /// the failed try and the park is never lost).
    pub fn enter(self: *SlotGates, slot: u16, io: Io) void {
        var parked = false;
        while (true) {
            var n = self.waiters.notified();
            _ = n.enable(io);
            if (self.tryEnter(slot)) {
                n.cancel(io);
                return;
            }
            if (!parked) {
                parked = true;
                rstats.stats.routing_gate_parks.inc();
            }
            n.waitUncancelable(io);
        }
    }

    /// Close slot `s`: new ops on it park; admitted ones drain.
    pub fn close(self: *SlotGates, slot: u16) void {
        assert(self.closed_slot.load(.monotonic) == OPEN);
        // SeqCst store: the Dekker store side of the closer.
        self.closed_slot.store(@as(u32, slot) + 1, .seq_cst);
    }

    /// The closer's read of the word (the Dekker load side, SeqCst).
    pub fn isDrained(self: *SlotGates, slot: u16) bool {
        return self.inflight[slot].load(.seq_cst) == 0;
    }

    pub const DrainOutcome = enum { drained, deadline };

    /// Wait for slot `s`'s word to drain, bounded by a monotonic deadline
    /// (register-then-check; the last `exit` notifies).
    pub fn waitDrained(self: *SlotGates, slot: u16, io: Io, deadline_ns: u64) DrainOutcome {
        while (true) {
            var n = self.drained.notified();
            _ = n.enable(io);
            if (self.isDrained(slot)) {
                n.cancel(io);
                return .drained;
            }
            switch (n.waitUntilUncancelable(io, deadline_ns)) {
                .notified => {},
                .deadline => return if (self.isDrained(slot)) .drained else .deadline,
            }
        }
    }

    /// Reopen: parked ops retry (and re-route through the published map).
    pub fn reopen(self: *SlotGates, io: Io) void {
        self.closed_slot.store(OPEN, .release);
        self.waiters.notifyWaiters(io);
    }

    pub fn closedSlot(self: *const SlotGates) ?u16 {
        const c = self.closed_slot.load(.acquire);
        return if (c == OPEN) null else @intCast(c - 1);
    }

    pub fn inflightOf(self: *const SlotGates, slot: u16) u32 {
        return self.inflight[slot].load(.acquire);
    }
};

/// What an op holds between its entry and its terminal outcome: the slots
/// it entered (deduped; an op touches ≤ `MAX_SLOTS` — rename's two
/// parents, two inodes and a whiteout mint, plus a mint). Exited exactly
/// once.
pub const Pass = struct {
    pub const MAX_SLOTS: usize = 6;

    gates: *SlotGates,
    slots: [MAX_SLOTS]u16 = undefined,
    len: u8 = 0,

    pub fn init(gates: *SlotGates) Pass {
        return .{ .gates = gates };
    }

    pub fn holds(self: *const Pass, slot: u16) bool {
        for (self.slots[0..self.len]) |s| if (s == slot) return true;
        return false;
    }

    /// Enter `slot` (parking while closed); idempotent per slot.
    pub fn enter(self: *Pass, slot: u16, io: Io) void {
        if (self.holds(slot)) return;
        assert(self.len < MAX_SLOTS);
        self.gates.enter(slot, io);
        self.slots[self.len] = slot;
        self.len += 1;
    }

    /// Exit every entered slot.
    pub fn exit(self: *Pass, io: Io) void {
        for (self.slots[0..self.len]) |s| self.gates.exit(s, io);
        self.len = 0;
    }
};

// ---------------------------------------------------------------------------
// Tests (the model lives in `slot_gate_model.zig`).
// ---------------------------------------------------------------------------

const testing = std.testing;

test "an open gate admits and counts; a closed slot parks new entrants (counted once per park) and drains when the admitted ops exit; reopen releases the parkers; other slots are untouched" {
    const gpa = testing.allocator;
    const io = testing.io;
    var g = try SlotGates.init(gpa);
    defer g.deinit();
    try testing.expect(g.tryEnter(7));
    try testing.expectEqual(@as(u32, 1), g.inflightOf(7));
    try testing.expect(g.isDrained(8));
    g.close(7);
    try testing.expectEqual(@as(?u16, 7), g.closedSlot());
    try testing.expect(!g.tryEnter(7));
    try testing.expectEqual(@as(u32, 1), g.inflightOf(7));
    try testing.expect(g.tryEnter(8));
    g.exit(8, io);
    try testing.expect(!g.isDrained(7));
    // A parker on another thread; the admitted op exits → drained; reopen
    // → the parker enters.
    const parks0 = rstats.stats.routing_gate_parks.load();
    const Parker = struct {
        fn run(gates: *SlotGates, entered: *std.atomic.Value(bool)) void {
            gates.enter(7, testing.io);
            entered.store(true, .release);
        }
    };
    var entered = std.atomic.Value(bool).init(false);
    const th = try std.Thread.spawn(.{}, Parker.run, .{ &g, &entered });
    // The parker has COUNTED its park (its registration precedes the
    // failed try, so a reopen after this point is never lost) before the
    // closer proceeds; the closer then waits for the drain (deadline far).
    while (rstats.stats.routing_gate_parks.load() == parks0) std.Thread.yield() catch {};
    g.exit(7, io);
    try testing.expectEqual(SlotGates.DrainOutcome.drained, g.waitDrained(7, io, std.math.maxInt(u64)));
    try testing.expect(!entered.load(.acquire));
    g.reopen(io);
    th.join();
    try testing.expect(entered.load(.acquire));
    try testing.expectEqual(parks0 + 1, rstats.stats.routing_gate_parks.load());
    try testing.expectEqual(@as(u32, 1), g.inflightOf(7));
    g.exit(7, io);
    try testing.expect(g.isDrained(7));
}

test "waitDrained answers deadline (never a hang) while an admitted op is in flight, and drained once it exits; a Pass enters each slot once and exits all" {
    const gpa = testing.allocator;
    const io = testing.io;
    var g = try SlotGates.init(gpa);
    defer g.deinit();
    var p = Pass.init(&g);
    p.enter(3, io);
    p.enter(5, io);
    p.enter(3, io);
    try testing.expectEqual(@as(u8, 2), p.len);
    try testing.expectEqual(@as(u32, 1), g.inflightOf(3));
    g.close(3);
    try testing.expectEqual(SlotGates.DrainOutcome.deadline, g.waitDrained(3, io, @import("../../core/clock.zig").monotonicNs() + 20 * std.time.ns_per_ms));
    p.exit(io);
    try testing.expectEqual(SlotGates.DrainOutcome.drained, g.waitDrained(3, io, std.math.maxInt(u64)));
    g.reopen(io);
    try testing.expectEqual(@as(?u16, null), g.closedSlot());
}
