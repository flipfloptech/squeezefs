//! Explorer model of the drain worker's MERGED pass (decision 0003 §AB.3):
//! the worker drives its handler lane — no lane thread — so ONE thread runs
//! the FUSE ring's pass AND the lane's fibers, and ONE wait covers both: the
//! worker's ring holds a standing READ of the lane's wake eventfd beside its
//! own.
//!
//! The worker's pass, in the shipped order:
//!   reap (the lane-wake READ's CQE consumed) → pump the commit cells →
//!   lane DISARM → scan the lane's inbox → run the fibers ready at entry →
//!   pump the cells they published → arm the lane-wake READ → park ONLY when
//!   no cell is published (`self_kick`) and no fiber is ready; else loop
//!   non-blocking.
//!
//! Three threads: two FOREIGN producers (journal lanes acking commits: each
//! queues its parked handler in the lane's inbox → arms the lane's
//! coalescer → writes the eventfd iff the arm won) and the WORKER. Handler A
//! (woken by producer 0), when run, wakes handler B on the same lane (the
//! ready FIFO, no eventfd) and finishes; B and C (woken by producer 1), when
//! run, each publish a reply into a commit cell on the worker's own thread
//! (no wake: the publisher IS the parker).
//! The kernel's half (the armed READ completing when the counter is
//! non-zero) folds into the worker's reap and park steps.
//!
//! Weakening ledger (each fails the explorer):
//!   `.park_with_ready` — the park check ignores the lane's ready FIFO: A's
//!     same-lane wake of B made B runnable during the pass, nothing writes the
//!     eventfd for it, and the worker parks with B runnable — the lost fiber.
//!   `.same_thread_cell_unconsumed` — neither the post-run pump nor the
//!     park check takes the commit cells: a reply a fiber published on the
//!     worker's own thread elides its wake, and the worker parks with the
//!     reply unsent — the lost reply. (Either one alone suffices: the
//!     post-run pump is the fast path, the park check the net under it.)
//!   `.lane_disarm_after_scan` — the lane's coalescer is disarmed AFTER its
//!     inbox scan: a foreign push between the scan and the disarm finds the
//!     coalescer armed, writes nothing, and the worker parks on a non-empty
//!     inbox — the lost foreign wake.
//!   `.no_lane_read` — the worker parks without arming its READ of the
//!     lane's eventfd: the foreign write lands in a counter nobody waits on —
//!     the wake lost between the lane's queue and the worker's CQ wait.

const std = @import("std");
const explorer = @import("explorer.zig");

const Weakening = struct {
    park_with_ready: bool = false,
    same_thread_cell_unconsumed: bool = false,
    lane_disarm_after_scan: bool = false,
    no_lane_read: bool = false,
};

const Fiber = enum(u8) { parked, ready, finished };

fn PassModel(comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        /// Threads 0 and 1: the foreign producers; 2: the worker.
        pub const thread_count: usize = 3;
        const worker = 2;

        // The lane's wake pair (any thread pushes; the worker scans): the
        // fibers queued in the inbox.
        inbox_a: bool = false,
        inbox_c: bool = false,
        armed: bool = false,
        efd: u8 = 0,
        // The worker ring's READ of the lane eventfd.
        read_armed: bool = false,
        read_done: bool = false,
        /// A lane-wake CQE reaped and not yet serviced (the disarm closes it).
        consumed: bool = false,
        // The handler lane: A waits on the foreign ack, B on A.
        a: Fiber = .parked,
        b: Fiber = .parked,
        c: Fiber = .parked,
        /// The ready FIFO's population.
        ready: u8 = 0,
        // The worker's commit cells: published, and committed to the ring.
        cells: u8 = 0,
        committed: u8 = 0,
        fpc: [2]u8 = @splat(0),
        /// 9 = exited.
        wpc: u8 = 0,

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return if (t == worker) self.wpc == 9 else self.fpc[t] == 9;
        }
        fn producersDone(self: *const Self) bool {
            return self.fpc[0] == 9 and self.fpc[1] == 9;
        }
        pub fn step(self: *Self, t: usize) void {
            if (t == worker) return self.workerStep();
            switch (self.fpc[t]) {
                // queue the producer's fiber in the lane's inbox (its ack landed)
                0 => {
                    if (t == 0) self.inbox_a = true else self.inbox_c = true;
                    self.fpc[t] = 1;
                },
                // arm: swap(true) — the write is owed iff it was unarmed
                1 => {
                    const was = self.armed;
                    self.armed = true;
                    self.fpc[t] = if (was) 9 else 2;
                },
                // the eventfd write
                2 => {
                    self.efd += 1;
                    self.fpc[t] = 9;
                },
                else => unreachable,
            }
        }
        /// The kernel: an armed READ completes when the counter is non-zero.
        fn kernelRead(self: *Self) void {
            if (self.read_armed and self.efd > 0) {
                self.efd = 0;
                self.read_armed = false;
                self.read_done = true;
            }
        }
        fn runOne(self: *Self) void {
            if (self.ready == 0) return;
            self.ready -= 1;
            if (self.a == .ready) {
                // A wakes B on the same lane and finishes.
                self.a = .finished;
                if (self.b == .parked) {
                    self.b = .ready;
                    self.ready += 1;
                }
            } else if (self.b == .ready) {
                // B replies: its cell, on this thread — the wake is elided.
                self.b = .finished;
                self.cells += 1;
            } else if (self.c == .ready) {
                self.c = .finished;
                self.cells += 1;
            }
        }
        fn workerStep(self: *Self) void {
            switch (self.wpc) {
                // reap: the lane-wake READ's CQE
                0 => {
                    self.kernelRead();
                    if (self.read_done) {
                        self.read_done = false;
                        self.consumed = true;
                    }
                    self.wpc = 1;
                },
                // pump the commit cells
                1 => {
                    self.committed += self.cells;
                    self.cells = 0;
                    self.wpc = if (weakening.lane_disarm_after_scan) 3 else 2;
                },
                // lane DISARM (the shipped order: before the scan)
                2 => {
                    if (self.consumed) {
                        self.armed = false;
                        self.consumed = false;
                    }
                    self.wpc = if (weakening.lane_disarm_after_scan) 4 else 3;
                },
                // scan the lane's inbox: every queued fiber becomes ready
                3 => {
                    if (self.inbox_a) {
                        self.inbox_a = false;
                        self.a = .ready;
                        self.ready += 1;
                    }
                    if (self.inbox_c) {
                        self.inbox_c = false;
                        self.c = .ready;
                        self.ready += 1;
                    }
                    self.wpc = if (weakening.lane_disarm_after_scan) 2 else 4;
                },
                // run the fibers ready at entry (one per pass here: a fiber
                // made ready during the run waits for the next pass), then
                // pump the cells they published
                4 => {
                    self.runOne();
                    if (!weakening.same_thread_cell_unconsumed) {
                        self.committed += self.cells;
                        self.cells = 0;
                    }
                    self.wpc = 5;
                },
                // arm the lane-wake READ on the worker's ring
                5 => {
                    if (!weakening.no_lane_read and !self.read_armed and !self.read_done) self.read_armed = true;
                    self.kernelRead();
                    self.wpc = 6;
                },
                // park only with nothing in hand
                6 => {
                    const ready_blocks = !weakening.park_with_ready and self.ready > 0;
                    const cells_block = !weakening.same_thread_cell_unconsumed and self.cells > 0;
                    self.wpc = if (ready_blocks or cells_block) 0 else 7;
                },
                // parked: a completed READ wakes the worker; everything done
                // and drained ⇒ exit
                7 => {
                    self.kernelRead();
                    if (self.read_done) {
                        self.wpc = 0;
                    } else if (self.producersDone() and self.a == .finished and self.b == .finished and self.c == .finished and self.cells == 0) {
                        self.wpc = 9;
                    }
                },
                else => unreachable,
            }
        }
        pub fn invariant(self: *const Self) !void {
            // Parked with work in hand and no wake pending: once the producer
            // is done nothing can ever end the park.
            if (self.wpc != 7 or !self.producersDone()) return;
            if (self.read_done or (self.read_armed and self.efd > 0)) return;
            if (self.ready > 0) return error.LostRunnable;
            if (self.cells > 0) return error.LostReply;
            if (self.inbox_a or self.inbox_c) return error.LostForeignWake;
        }
        pub fn final(self: *const Self) !void {
            if (self.committed != 2) return error.RepliesNotCommitted;
            if (self.a != .finished or self.b != .finished or self.c != .finished) return error.FibersNotFinished;
        }
    };
}

const testing = std.testing;

test "the merged worker pass (decision 0003 §AB.3) holds: reap → cells → lane disarm → inbox → run → the cells they published → arm the lane READ → park only with nothing ready and no cell published" {
    try explorer.expectHolds(PassModel(.{}), .{});
}

test "each weakening of the merged pass fails: parking with a runnable fiber, parking with an unsent same-thread reply, the lane disarmed after its scan, no READ of the lane's eventfd on the worker's ring" {
    try explorer.expectFails(PassModel(.{ .park_with_ready = true }), .{}, error.InvariantViolated);
    try explorer.expectFails(PassModel(.{ .same_thread_cell_unconsumed = true }), .{}, error.InvariantViolated);
    try explorer.expectFails(PassModel(.{ .lane_disarm_after_scan = true }), .{}, error.InvariantViolated);
    try explorer.expectFails(PassModel(.{ .no_lane_read = true }), .{}, error.InvariantViolated);
}

test "worker pass model: exhaustive within the explorer's default limits" {
    var report: explorer.Report = undefined;
    try explorer.explore(PassModel(.{}), testing.allocator, .{}, &report);
    try testing.expect(report.states > 1);
    try testing.expect(report.states < 20_000);
}
