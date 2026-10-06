//! Explorer models of the payload-lease commit gate and the wake coalescer
//! as the transport composes them (`docs/porting/fuse_uring_transport.md`
//! §6.6: the reference's five loom models; charter §9 "the payload-lease
//! commit gate … the Dekker pairs"). Dependency-free value types; one
//! atomic action per `step`; explored exhaustively by `model/explorer.zig`.
//!
//! Memory-model convention. The explorer is sequentially consistent, so the
//! two SeqCst points of the gate are what the model gets for free when a
//! store is performed as ONE step and every later load follows it. A
//! WEAKENING deletes exactly that: the worker's `parked = true` becomes a
//! buffered store the explorer may land AFTER the worker's own re-read of
//! `refs` (the TSO store buffer — a plain release store), and the
//! releaser's read of `parked` may be satisfied BEFORE its `refs` decrement
//! is globally visible (a release RMW followed by an acquire load on
//! ARMv8: the load may be hoisted above the RMW's store half). Both are
//! exactly "release/acquire alone" (`slot.zig` module doc).
//!
//! Weakening ledger (every `expectFails` below):
//!   - `LeaseGate(1)` / `LeaseGate(2)` without the fence: the releaser skips
//!     the wake AND the worker parks — a parked commit nobody ever wakes
//!     (`FinalViolated`: `parked ∧ !woke`), the deterministic hang at depth 4.
//!   - `LeaseGateShutdown` writing the PAYLOAD (not the header alone) on a
//!     false probe: the drain writes a buffer the lease still reads
//!     (`InvariantViolated`).
//!   - `WakeCoalescer` with the scan BEFORE the disarm, or the disarm as a
//!     plain store the buffer defers past the scan: one of two publications
//!     is stranded with the counter at 0 (`FinalViolated`).
//!   - `WakeCoalescerLeaseDrop` without the gate's fence: the reply
//!     publication's wake is consumed by the pass that parks the commit,
//!     and the lease drop's wake is skipped — the commit is never made
//!     (`FinalViolated`).

const std = @import("std");
const explorer = @import("../../model/explorer.zig");

// ---------------------------------------------------------------------------
// 1 + 3. The lease gate with `droppers` lease holders (1 = the WRITE payload
// lease; 2 = dest-DMA tokens at refs ∈ {2, 1, 0}) and the worker's gate.
// ---------------------------------------------------------------------------

fn LeaseGate(comptime droppers: usize, comptime with_fence: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = droppers + 1;
        const worker = droppers;

        /// Live leases (the shared word).
        refs: u8 = droppers,
        /// The published park flag (the shared word's visible value).
        parked: bool = false,
        /// The worker's `parked = true` still in its store buffer.
        parked_pending: bool = false,
        /// Per dropper: the hoisted read of `parked` (weak model only).
        early_parked: [droppers]bool = @splat(false),
        /// Per dropper: this drop was the last and must wake.
        woke: [droppers]bool = @splat(false),
        /// Per dropper: this drop was the last.
        last: [droppers]bool = @splat(false),
        committed: u8 = 0,
        /// Worker-local: its commit is parked waiting on a wake.
        worker_parked: bool = false,
        pc: [droppers + 1]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == 255;
        }

        fn drain(self: *Self) void {
            if (self.parked_pending) {
                self.parked = true;
                self.parked_pending = false;
            }
        }

        /// What a dropper reads for `parked`: the visible word (an SC load
        /// sees a drained store; a pending store of the worker is, by
        /// construction, not yet visible).
        fn readParked(self: *const Self) bool {
            return self.parked;
        }

        pub fn step(self: *Self, t: usize) void {
            if (t < droppers) return self.stepDropper(t);
            self.stepWorker();
        }

        fn stepDropper(self: *Self, t: usize) void {
            if (with_fence) {
                switch (self.pc[t]) {
                    0 => {
                        // fetch_sub(1): atomic; the fence that follows makes
                        // the store half visible before the next load.
                        self.refs -= 1;
                        self.last[t] = self.refs == 0;
                        self.pc[t] = 1;
                    },
                    1 => {
                        // r1 = parked (after the fence).
                        if (self.last[t]) self.woke[t] = self.readParked();
                        self.pc[t] = 255;
                    },
                    else => unreachable,
                }
            } else {
                switch (self.pc[t]) {
                    0 => {
                        // The weakening: the load of `parked` is satisfied
                        // BEFORE the decrement is visible (hoisted above a
                        // release RMW).
                        self.early_parked[t] = self.readParked();
                        self.pc[t] = 1;
                    },
                    1 => {
                        self.refs -= 1;
                        self.last[t] = self.refs == 0;
                        if (self.last[t]) self.woke[t] = self.early_parked[t];
                        self.pc[t] = 255;
                    },
                    else => unreachable,
                }
            }
        }

        fn stepWorker(self: *Self) void {
            const w = worker;
            switch (self.pc[w]) {
                0 => {
                    // r0 = refs (acquire).
                    if (self.refs == 0) {
                        self.committed += 1;
                        self.pc[w] = 255;
                    } else self.pc[w] = 1;
                },
                1 => {
                    // parked = true: SC in the real gate (performed at once);
                    // a buffered store in the weakening.
                    if (with_fence) self.parked = true else self.parked_pending = true;
                    self.pc[w] = 2;
                },
                2 => {
                    // The re-check r2 = refs.
                    if (self.refs == 0) {
                        // Un-publish and commit now.
                        self.parked_pending = false;
                        self.parked = false;
                        self.committed += 1;
                        self.pc[w] = 255;
                    } else {
                        self.worker_parked = true;
                        self.pc[w] = 3;
                    }
                },
                3 => {
                    // The buffered store lands at some later point.
                    self.drain();
                    self.pc[w] = 255;
                },
                else => unreachable,
            }
        }

        pub fn invariant(self: *const Self) !void {
            // Never commit while any lease is live; never twice.
            if (self.committed > 0 and self.refs != 0) return error.CommittedWhileLeased;
            if (self.committed > 1) return error.CommittedTwice;
        }

        pub fn final(self: *const Self) !void {
            var wakes: u8 = 0;
            for (self.woke, 0..) |w, i| {
                if (w) wakes += 1;
                // A non-final drop never wakes.
                if (w and !self.last[i]) return error.NonFinalDropWoke;
            }
            if (self.committed == 1) {
                if (self.worker_parked) return error.CommittedAndParked;
                return;
            }
            // Not committed: the commit must be parked AND a wake must have
            // fired so the unpark scan (which now reads refs == 0) commits it.
            if (!self.worker_parked) return error.NeitherCommittedNorParked;
            if (wakes == 0) return error.ParkedWithoutWake;
            if (self.refs != 0) return error.LeaseOutlivedDrop;
        }
    };
}

// ---------------------------------------------------------------------------
// 2. Shutdown with a live lease: a late release races the drain's ONE
// `try_unpark` probe. A true probe ⇒ the lease is gone; a false probe ⇒ the
// drain commits HEADER-ONLY and never touches the payload.
// ---------------------------------------------------------------------------

fn LeaseGateShutdown(comptime header_only_fallback: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;

        refs: u8 = 1,
        parked: bool = true,
        /// The lease holder is still reading the payload.
        using: bool = true,
        /// The drain wrote the payload region.
        payload_written: bool = false,
        unparked: bool = false,
        committed: u8 = 0,
        pc: [2]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == 255;
        }
        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                0 => switch (self.pc[0]) {
                    // The lease's last access, then its drop (fetch_sub).
                    0 => {
                        self.using = false;
                        self.pc[0] = 1;
                    },
                    1 => {
                        self.refs -= 1;
                        self.pc[0] = 255;
                    },
                    else => unreachable,
                },
                1 => switch (self.pc[1]) {
                    // The drain's one probe.
                    0 => {
                        if (self.refs == 0) {
                            self.unparked = true;
                            self.parked = false;
                        }
                        self.pc[1] = 1;
                    },
                    1 => {
                        // Apply: the full reply (payload write) after a true
                        // probe; header-only after a false one — unless weakened.
                        if (self.unparked or !header_only_fallback) self.payload_written = true;
                        self.committed += 1;
                        self.pc[1] = 255;
                    },
                    else => unreachable,
                },
                else => unreachable,
            }
        }
        pub fn invariant(self: *const Self) !void {
            if (self.unparked and self.refs != 0) return error.UnparkedWhileLeased;
            if (self.payload_written and self.using) return error.PayloadWrittenUnderLease;
            if (self.committed > 1) return error.CommittedTwice;
        }
        pub fn final(self: *const Self) !void {
            if (self.committed != 1) return error.NotExactlyOnce;
            if (self.refs != 0) return error.LeaseOutlivedDrop;
        }
    };
}

// ---------------------------------------------------------------------------
// 4. The wake coalescer: two producers (publish → arm → write iff armed
// flipped) and a worker running ≤ 3 passes of drain → disarm → scan,
// parking when the eventfd counter reads 0.
// ---------------------------------------------------------------------------

const CoalescerOrder = enum { drain_disarm_scan, scan_before_disarm, plain_store_disarm };

fn WakeCoalescer(comptime order: CoalescerOrder) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        const worker = 2;
        const max_passes = 3;

        produced: u8 = 0,
        consumed: u8 = 0,
        armed: bool = false,
        /// The plain-store disarm sitting in the worker's store buffer.
        disarm_pending: bool = false,
        efd: u8 = 0,
        passes: u8 = 0,
        pc: [3]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }
        /// The worker is parked in its ring (pc 3 with the counter at 0): a
        /// later eventfd write re-runs its pass, so it is "done" only once
        /// no producer is left to write.
        pub fn done(self: *const Self, t: usize) bool {
            if (t < 2) return self.pc[t] == 255;
            return self.pc[worker] == 3 and self.efd == 0 and self.pc[0] == 255 and self.pc[1] == 255;
        }

        fn drainStore(self: *Self) void {
            if (self.disarm_pending) {
                self.armed = false;
                self.disarm_pending = false;
            }
        }

        pub fn step(self: *Self, t: usize) void {
            if (t < 2) {
                switch (self.pc[t]) {
                    0 => {
                        // publish
                        self.produced += 1;
                        self.pc[t] = 1;
                    },
                    1 => {
                        // r = swap(armed, true); if !r: write(efd)
                        const was = self.armed;
                        self.armed = true;
                        if (!was) self.efd += 1;
                        self.pc[t] = 255;
                    },
                    else => unreachable,
                }
                return;
            }
            // The worker's pass: pc 0 drain, 1 disarm, 2 scan, 3 park check.
            switch (self.pc[worker]) {
                0 => {
                    self.efd = 0;
                    self.passes += 1;
                    self.pc[worker] = switch (order) {
                        .drain_disarm_scan, .plain_store_disarm => 1,
                        .scan_before_disarm => 2,
                    };
                },
                1 => {
                    switch (order) {
                        .drain_disarm_scan => self.armed = false,
                        // The RMW disarm reads its predecessor and is ordered
                        // before the scan; a plain store may sit in the buffer
                        // while the scan runs.
                        .plain_store_disarm => self.disarm_pending = true,
                        .scan_before_disarm => self.armed = false,
                    }
                    self.pc[worker] = switch (order) {
                        .drain_disarm_scan, .plain_store_disarm => 2,
                        .scan_before_disarm => 3,
                    };
                },
                2 => {
                    self.consumed = self.produced;
                    self.pc[worker] = switch (order) {
                        .drain_disarm_scan => 3,
                        .plain_store_disarm => 4,
                        .scan_before_disarm => 1,
                    };
                },
                3 => {
                    // Park: a non-zero counter re-runs the pass (the standing
                    // READ completes at once); 0 ⇒ stay parked (a self-loop
                    // the explorer prunes) until a producer writes.
                    if (self.efd > 0) {
                        std.debug.assert(self.passes < max_passes);
                        self.pc[worker] = 0;
                    }
                },
                4 => {
                    // The buffered disarm lands after the scan.
                    self.drainStore();
                    self.pc[worker] = 3;
                },
                else => unreachable,
            }
        }
        pub fn invariant(_: *const Self) !void {}
        pub fn final(self: *const Self) !void {
            // Both producers finished: every publication must have been
            // consumed, or a wake must still be pending (it never is within
            // 3 passes — two producers can pay at most two writes).
            if (self.consumed != 2) return error.PublicationStranded;
        }
    };
}

// ---------------------------------------------------------------------------
// 5. One coalescer shared by a lease dropper and a reply submitter against
// a worker whose pass runs the gate on a commit it already admitted for the
// leased ent.
// ---------------------------------------------------------------------------

fn WakeCoalescerLeaseDrop(comptime with_fence: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        const dropper = 0;
        const submitter = 1;
        const worker = 2;
        const max_passes = 3;

        refs: u8 = 1,
        parked: bool = false,
        parked_pending: bool = false,
        early_parked: bool = false,
        armed: bool = false,
        efd: u8 = 0,
        /// The reply publication (another ent's commit) and its consumption.
        reply_published: bool = false,
        reply_consumed: bool = false,
        /// The leased ent's admitted commit: parked locally / committed.
        worker_parked: bool = false,
        committed: u8 = 0,
        passes: u8 = 0,
        pc: [3]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }
        /// The worker parked at pc 8 with the counter at 0 is done only
        /// once neither producer can write again.
        pub fn done(self: *const Self, t: usize) bool {
            if (t != worker) return self.pc[t] == 255;
            return self.pc[worker] == 8 and self.efd == 0 and self.pc[dropper] == 255 and self.pc[submitter] == 255;
        }

        fn arm(self: *Self) void {
            const was = self.armed;
            self.armed = true;
            if (!was) self.efd += 1;
        }

        fn drain(self: *Self) void {
            if (self.parked_pending) {
                self.parked = true;
                self.parked_pending = false;
            }
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                dropper => self.stepDropper(),
                submitter => switch (self.pc[submitter]) {
                    0 => {
                        self.reply_published = true;
                        self.pc[submitter] = 1;
                    },
                    1 => {
                        self.arm();
                        self.pc[submitter] = 255;
                    },
                    else => unreachable,
                },
                worker => self.stepWorker(),
                else => unreachable,
            }
        }

        fn stepDropper(self: *Self) void {
            const d = dropper;
            if (with_fence) {
                switch (self.pc[d]) {
                    0 => {
                        self.refs -= 1;
                        self.pc[d] = 1;
                    },
                    1 => {
                        // r1 = parked; true ⇒ arm (write iff it flipped).
                        if (self.parked) self.arm();
                        self.pc[d] = 255;
                    },
                    else => unreachable,
                }
            } else {
                switch (self.pc[d]) {
                    0 => {
                        self.early_parked = self.parked;
                        self.pc[d] = 1;
                    },
                    1 => {
                        self.refs -= 1;
                        if (self.early_parked) self.arm();
                        self.pc[d] = 255;
                    },
                    else => unreachable,
                }
            }
        }

        fn stepWorker(self: *Self) void {
            const w = worker;
            switch (self.pc[w]) {
                // drain
                0 => {
                    self.efd = 0;
                    self.passes += 1;
                    self.pc[w] = 1;
                },
                // disarm (RMW)
                1 => {
                    self.armed = false;
                    self.pc[w] = 2;
                },
                // scan: the reply publication
                2 => {
                    if (self.reply_published) self.reply_consumed = true;
                    self.pc[w] = if (self.committed == 0) (if (self.worker_parked) 6 else 3) else 7;
                },
                // the gate: r0 = refs
                3 => {
                    if (self.refs == 0) {
                        self.committed += 1;
                        self.pc[w] = 7;
                    } else self.pc[w] = 4;
                },
                // parked = true (SC, or buffered in the weakening)
                4 => {
                    if (with_fence) self.parked = true else self.parked_pending = true;
                    self.pc[w] = 5;
                },
                // the re-check
                5 => {
                    if (self.refs == 0) {
                        self.parked_pending = false;
                        self.parked = false;
                        self.committed += 1;
                    } else self.worker_parked = true;
                    self.pc[w] = 7;
                },
                // the unpark scan on a later pass
                6 => {
                    if (self.refs == 0) {
                        self.parked = false;
                        self.worker_parked = false;
                        self.committed += 1;
                    }
                    self.pc[w] = 7;
                },
                // the buffered store lands; then the park decision
                7 => {
                    self.drain();
                    self.pc[w] = 8;
                },
                8 => {
                    if (self.efd > 0) {
                        std.debug.assert(self.passes < max_passes);
                        self.pc[w] = 0;
                    }
                },
                else => unreachable,
            }
        }

        pub fn invariant(self: *const Self) !void {
            if (self.committed > 1) return error.CommittedTwice;
            if (self.committed > 0 and self.refs != 0) return error.CommittedWhileLeased;
        }
        pub fn final(self: *const Self) !void {
            if (!self.reply_consumed) return error.ReplyPublicationStranded;
            if (self.committed != 1) return error.ParkedCommitStranded;
        }
    };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

const testing = std.testing;

fn report(comptime Model: type) !usize {
    var r: explorer.Report = undefined;
    try explorer.explore(Model, testing.allocator, .{}, &r);
    return r.states;
}

test "lease gate (one lease): commit exactly once, never while leased; a park always has its wake — and release/acquire alone (no fence) parks without a wake" {
    try explorer.expectHolds(LeaseGate(1, true), .{});
    try explorer.expectFails(LeaseGate(1, false), .{}, error.FinalViolated);
}

test "lease gate (two dest-DMA tokens): exactly once at refs ∈ {2,1,0}, a non-final drop never wakes, a park has ≥ 1 wake — and the unfenced pair hangs" {
    try explorer.expectHolds(LeaseGate(2, true), .{});
    try explorer.expectFails(LeaseGate(2, false), .{}, error.FinalViolated);
}

test "shutdown drain with a live lease: a true probe proves the lease gone; the header-only fallback never writes a leased payload — writing the payload on a false probe is the violation" {
    try explorer.expectHolds(LeaseGateShutdown(true), .{});
    try explorer.expectFails(LeaseGateShutdown(false), .{}, error.InvariantViolated);
}

test "wake coalescer: two publications are never stranded under drain → disarm → scan; scan-before-disarm strands one; a plain-store disarm the buffer defers past the scan strands one" {
    try explorer.expectHolds(WakeCoalescer(.drain_disarm_scan), .{});
    try explorer.expectFails(WakeCoalescer(.scan_before_disarm), .{}, error.FinalViolated);
    try explorer.expectFails(WakeCoalescer(.plain_store_disarm), .{}, error.FinalViolated);
}

test "one coalescer shared by a lease drop and a reply submitter: the parked commit is made exactly once and the reply observed — without the gate's fence the parked commit is stranded" {
    try explorer.expectHolds(WakeCoalescerLeaseDrop(true), .{});
    try explorer.expectFails(WakeCoalescerLeaseDrop(false), .{}, error.FinalViolated);
}

test "state counts: every model is exhaustive within the explorer's default limits (the evidence note records them)" {
    const counts = .{
        try report(LeaseGate(1, true)),
        try report(LeaseGate(2, true)),
        try report(LeaseGateShutdown(true)),
        try report(WakeCoalescer(.drain_disarm_scan)),
        try report(WakeCoalescerLeaseDrop(true)),
    };
    inline for (counts) |c| try testing.expect(c > 0 and c < 2_000_000);
    std.debug.print("\nslot models: lease_gate_1={d} lease_gate_2={d} shutdown={d} coalescer={d} coalescer_lease_drop={d}\n", .{ counts[0], counts[1], counts[2], counts[3], counts[4] });
}
