//! MODEL (charter §9 "model-check every lock-free core"; decision 0003
//! §CH): an fsync of one inode against its member's lane — the
//! committer's stamp, the caller's skip on the durable floor, the request
//! with its own cover, the JOIN of a barrier already in flight, the
//! answer. Dependency-free; explored by `src/model/explorer.zig`.
//!
//! The window W of the inode's entry is position 1. Threads: the
//! COMMITTER (reserve W → stamp its mark → return, the op's answer); the
//! DEVICE (lands W once reserved: the write's CQE); the CADENCE (one
//! coalesced barrier started at any moment, its floor the landed prefix
//! then — possibly before W landed); the LANE (admits the request — at
//! once if the durable floor covers its want, JOINED to the barrier in
//! flight if that barrier's floor does, else needing the next — and starts
//! a barrier for a waiter once its want landed and none is in flight);
//! the FLUSH (completes the barrier in flight: the durable floor rises to
//! its floor; a waiter whose ticket it reaches AND whose want its floor
//! covers is answered — `JournalLane.barrierDone`); the FSYNC caller
//! (after the committer returned: reads the mark, then the durable floor,
//! then skips or requests and waits).
//!
//! Holds: an fsync is never declared durable before W is (the durable
//! floor at 1), and is always declared. Weakenings, each failing the
//! invariant: `.join_by_ticket` (a request admitted while a barrier is in
//! flight rides it by its ticket alone, the floor unchecked: the cadence's
//! barrier that started before W landed answers it), `.skip_on_landed`
//! (the caller skips on the LANDED prefix instead of the durable floor),
//! `.stamp_after_return` (the committer stamps after its op returned: the
//! caller reads no mark and skips).

const std = @import("std");
const explorer = @import("../model/explorer.zig");

pub const Weakening = enum { none, join_by_ticket, skip_on_landed, stamp_after_return };

pub fn FsyncJoin(comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 6;
        const committer_t = 0;
        const device_t = 1;
        const cadence_t = 2;
        const lane_t = 3;
        const flush_t = 4;
        const fsync_t = 5;

        const Cpc = enum(u8) { reserve, stamp, ret, done };
        const Spc = enum(u8) { wait_return, read_mark, read_floor, decide, wait_answer, done };

        reserved: bool = false,
        landed: bool = false,
        cpc: Cpc = .reserve,
        mark: u8 = 0,
        returned: bool = false,
        cadence_done: bool = false,
        in_flight: bool = false,
        /// The in-flight (then the last) barrier's floor.
        floor: u8 = 0,
        started: u8 = 0,
        completed: u8 = 0,
        durable: u8 = 0,
        pushed: bool = false,
        admitted: bool = false,
        want: u8 = 0,
        need: u8 = 0,
        answered: bool = false,
        spc: Spc = .wait_return,
        seen_mark: u8 = 0,
        seen_floor: u8 = 0,
        declared: bool = false,

        pub fn init() Self {
            return .{};
        }

        fn landedFloor(self: *const Self) u8 {
            return if (self.landed) 1 else 0;
        }

        fn startBarrier(self: *Self) void {
            self.in_flight = true;
            self.floor = self.landedFloor();
            self.started += 1;
        }

        fn laneCanMove(self: *const Self) bool {
            return (self.pushed and !self.admitted) or (self.admitted and !self.answered and !self.in_flight and self.want <= self.landedFloor());
        }

        /// Nothing left to move and the caller still waits: an fsync
        /// stranded (the lane and the flush end so `final` judges it).
        fn stalled(self: *const Self) bool {
            return !self.declared and self.cpc == .done and self.landed and self.cadence_done and !self.in_flight and !self.laneCanMove() and self.spc == .wait_answer;
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                committer_t => self.cpc == .done,
                device_t => self.landed,
                cadence_t => self.cadence_done,
                lane_t, flush_t => self.declared or self.stalled(),
                fsync_t => self.spc == .done,
                else => unreachable,
            };
        }

        fn stepCommitter(self: *Self) void {
            const late = weakening == .stamp_after_return;
            switch (self.cpc) {
                .reserve => {
                    self.reserved = true;
                    self.cpc = if (late) .ret else .stamp;
                },
                .stamp => {
                    self.mark = 1;
                    self.cpc = if (late) .done else .ret;
                },
                .ret => {
                    self.returned = true;
                    self.cpc = if (late) .stamp else .done;
                },
                .done => unreachable,
            }
        }

        fn stepLane(self: *Self) void {
            if (self.pushed and !self.admitted) {
                self.admitted = true;
                if (self.want <= self.durable) {
                    self.answered = true;
                } else if (self.in_flight and (weakening == .join_by_ticket or self.floor >= self.want)) {
                    self.need = self.started;
                } else self.need = self.started + 1;
                return;
            }
            // A waiter left after a barrier pulls the next once its want
            // landed (`needs_flush` + the §BB.4 hold).
            if (self.admitted and !self.answered and !self.in_flight and self.want <= self.landedFloor()) self.startBarrier();
        }

        fn stepFlush(self: *Self) void {
            if (!self.in_flight) return;
            self.in_flight = false;
            self.completed += 1;
            self.durable = @max(self.durable, self.floor);
            if (self.admitted and !self.answered and self.need <= self.completed and (weakening == .join_by_ticket or self.want <= self.floor)) self.answered = true;
        }

        fn stepFsync(self: *Self) void {
            switch (self.spc) {
                .wait_return => if (self.returned) {
                    self.spc = .read_mark;
                },
                .read_mark => {
                    self.seen_mark = self.mark;
                    self.spc = .read_floor;
                },
                .read_floor => {
                    self.seen_floor = if (weakening == .skip_on_landed) self.landedFloor() else self.durable;
                    self.spc = .decide;
                },
                .decide => if (self.seen_mark <= self.seen_floor) {
                    self.declared = true;
                    self.spc = .done;
                } else {
                    self.want = self.seen_mark;
                    self.pushed = true;
                    self.spc = .wait_answer;
                },
                .wait_answer => if (self.answered) {
                    self.declared = true;
                    self.spc = .done;
                },
                .done => unreachable,
            }
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                committer_t => self.stepCommitter(),
                device_t => if (self.reserved and !self.landed) {
                    self.landed = true;
                },
                cadence_t => {
                    if (!self.in_flight) self.startBarrier();
                    self.cadence_done = true;
                },
                lane_t => self.stepLane(),
                flush_t => self.stepFlush(),
                fsync_t => self.stepFsync(),
                else => unreachable,
            }
        }

        pub fn invariant(self: *const Self) !void {
            if (self.declared and self.durable < 1) return error.FsyncDeclaredBeforeItsEntryDurable;
        }

        pub fn final(self: *const Self) !void {
            if (!self.declared) return error.FsyncStranded;
        }
    };
}

test "the covering fsync's join: never answered before its entry is durable, never stranded; joining by the ticket alone, skipping on the landed prefix and stamping after the op's answer each fail (decision 0003 §CH)" {
    try explorer.expectHolds(FsyncJoin(.none), .{});
    try explorer.expectFails(FsyncJoin(.join_by_ticket), .{}, error.InvariantViolated);
    try explorer.expectFails(FsyncJoin(.skip_on_landed), .{}, error.InvariantViolated);
    try explorer.expectFails(FsyncJoin(.stamp_after_return), .{}, error.InvariantViolated);
}

test "the model's state count (the evidence note's row)" {
    var report: explorer.Report = undefined;
    try explorer.explore(FsyncJoin(.none), std.testing.allocator, .{}, &report);
    std.debug.print("\nmodel FsyncJoin: {d} states, depth {d}\n", .{ report.states, report.max_depth_seen });
    try std.testing.expect(report.states > 10);
}
