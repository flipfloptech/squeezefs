//! The exhaustive models of the limiter's two lock-free handshakes
//! (decision 0003 §CE; `host/inval.zig`), with EXPLICIT store buffers (TSO,
//! as `write_lease_model.zig`): a thread's store lands in its own buffer
//! and reaches main memory at its FENCE step; a SeqCst RMW is a drained
//! store. The property of both: every ring write the window suppressed is
//! COVERED — a whole invalidation lands after its RAM apply — or still
//! marked for the next sweep; never neither.
//!
//! `Unbind`: a suppressed write races its binding's UNBIND. The WRITER
//! applies, marks its row dirty and reads its binding (dead ⇒ it swaps the
//! mark off and fires whole); the UNBINDER stores the row dead and swaps the dirty
//! mark off (dirty ⇒ it fires whole). Weakenings proven red: drop the
//! writer's fence (its dirty mark reaches memory after the unbinder's swap
//! read it clean, while it read its row still live), drop the unbinder's
//! fence (its dead row reaches the writer after the writer read it live,
//! while its swap ran before the dirty mark landed).
//!
//! `Sweep`: a suppressed write races the window's trailing sweep. The
//! WRITER applies, swaps its row dirty and, on the row's clean → dirty
//! edge, raises its session's sweep flag; the SWEEPER swaps the flag off
//! and, when it was up, swaps the row's mark off (dirty ⇒ it fires whole).
//! At the end the write is covered, or its row is dirty with the flag up
//! (the next sweep's). Explored from a clean row and from a row an earlier
//! write already marked (its flag up: this write raises nothing). Weakening
//! proven red: raise the flag BEFORE marking the row (the sweep consumes
//! the flag and reads the row clean; the mark lands with no flag to find
//! it).

const std = @import("std");
const explorer = @import("../model/explorer.zig");

pub const UnbindVariant = enum { shipped, no_writer_fence, no_unbinder_fence };

pub fn Unbind(comptime variant: UnbindVariant) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const writer = 0;
        const unbinder = 1;

        // ---- main memory ----
        live: bool = true,
        dirty: bool = false,
        // ---- store buffers ----
        pend_dirty: bool = false,
        pend_dead: bool = false,
        // ---- the world: the step index of each event (0 = not yet) ----
        clock: u8 = 0,
        applied_at: u8 = 0,
        last_fire_at: u8 = 0,
        // ---- registers ----
        u_dirty: bool = false,
        pc: [2]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == 9;
        }
        pub fn step(self: *Self, t: usize) void {
            self.clock += 1;
            if (t == writer) self.stepWriter() else self.stepUnbinder();
        }
        fn drainDirty(self: *Self) void {
            if (self.pend_dirty) {
                self.dirty = true;
                self.pend_dirty = false;
            }
        }
        fn drainDead(self: *Self) void {
            if (self.pend_dead) {
                self.live = false;
                self.pend_dead = false;
            }
        }
        fn stepWriter(self: *Self) void {
            switch (self.pc[writer]) {
                0 => {
                    // The RAM apply.
                    self.applied_at = self.clock;
                    self.pc[writer] = 1;
                },
                1 => {
                    // The row's dirty mark: buffered.
                    self.pend_dirty = true;
                    self.pc[writer] = 2;
                },
                2 => {
                    // The SeqCst RMW drains it before the binding is read.
                    if (variant != .no_writer_fence) self.drainDirty();
                    self.pc[writer] = 3;
                },
                3 => {
                    // Main memory: the unbinder's buffered store is not
                    // the writer's view.
                    self.pc[writer] = if (self.live) 5 else 4;
                },
                4 => {
                    // A dead binding: the write swaps its mark off and
                    // fires whole (an unbinder that swapped it off first
                    // fired for it).
                    if (self.dirty) {
                        self.dirty = false;
                        self.last_fire_at = self.clock;
                    }
                    self.pc[writer] = 5;
                },
                5 => {
                    self.drainDirty();
                    self.pc[writer] = 9;
                },
                else => unreachable,
            }
        }
        fn stepUnbinder(self: *Self) void {
            switch (self.pc[unbinder]) {
                0 => {
                    // The row stored dead (`live.swap(false)`): buffered.
                    self.pend_dead = true;
                    self.pc[unbinder] = 1;
                },
                1 => {
                    // The SeqCst RMW drains it before the mark is read.
                    if (variant != .no_unbinder_fence) self.drainDead();
                    self.pc[unbinder] = 2;
                },
                2 => {
                    // The dirty swap over main memory.
                    self.u_dirty = self.dirty;
                    self.dirty = false;
                    self.pc[unbinder] = if (self.u_dirty) 3 else 4;
                },
                3 => {
                    self.last_fire_at = self.clock;
                    self.pc[unbinder] = 4;
                },
                4 => {
                    self.drainDead();
                    self.pc[unbinder] = 9;
                },
                else => unreachable,
            }
        }
        pub fn invariant(_: *const Self) !void {}
        pub fn final(self: *const Self) !void {
            if (self.last_fire_at <= self.applied_at) return error.WriteUncovered;
        }
    };
}

pub const SweepVariant = enum { shipped, flag_before_mark };

pub fn Sweep(comptime variant: SweepVariant, comptime pre_marked: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const writer = 0;
        const sweeper = 1;

        dirty: bool = pre_marked,
        flag: bool = pre_marked,
        clock: u8 = 0,
        applied_at: u8 = 0,
        last_fire_at: u8 = 0,
        w_old: bool = false,
        s_flag: bool = false,
        s_dirty: bool = false,
        pc: [2]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == 9;
        }
        pub fn step(self: *Self, t: usize) void {
            self.clock += 1;
            if (t == writer) self.stepWriter() else self.stepSweeper();
        }
        fn stepWriter(self: *Self) void {
            switch (self.pc[writer]) {
                0 => {
                    self.applied_at = self.clock;
                    self.pc[writer] = 1;
                },
                1 => {
                    if (variant == .flag_before_mark) {
                        self.flag = true;
                    } else {
                        // `dirty.swap(true)`: the clean → dirty edge raises.
                        self.w_old = self.dirty;
                        self.dirty = true;
                    }
                    self.pc[writer] = 2;
                },
                2 => {
                    if (variant == .flag_before_mark) {
                        self.dirty = true;
                    } else if (!self.w_old) self.flag = true;
                    self.pc[writer] = 9;
                },
                else => unreachable,
            }
        }
        fn stepSweeper(self: *Self) void {
            switch (self.pc[sweeper]) {
                0 => {
                    self.s_flag = self.flag;
                    self.flag = false;
                    self.pc[sweeper] = if (self.s_flag) 1 else 9;
                },
                1 => {
                    self.s_dirty = self.dirty;
                    self.dirty = false;
                    self.pc[sweeper] = if (self.s_dirty) 2 else 9;
                },
                2 => {
                    self.last_fire_at = self.clock;
                    self.pc[sweeper] = 9;
                },
                else => unreachable,
            }
        }
        pub fn invariant(_: *const Self) !void {}
        pub fn final(self: *const Self) !void {
            if (self.last_fire_at > self.applied_at) return;
            if (!(self.dirty and self.flag)) return error.WriteUncovered;
        }
    };
}

test "the limiter's handshakes (decision 0003 §CE): a suppressed write racing its unbind, or the window's trailing sweep, is covered by a whole invalidation after its apply or left marked for the next sweep — dropping the writer's or the unbinder's fence (store buffering) breaks the unbind; raising the sweep flag before the row's mark breaks the sweep" {
    try explorer.expectHolds(Unbind(.shipped), .{});
    try explorer.expectFails(Unbind(.no_writer_fence), .{}, error.FinalViolated);
    try explorer.expectFails(Unbind(.no_unbinder_fence), .{}, error.FinalViolated);
    try explorer.expectHolds(Sweep(.shipped, false), .{});
    try explorer.expectHolds(Sweep(.shipped, true), .{});
    try explorer.expectFails(Sweep(.flag_before_mark, false), .{}, error.FinalViolated);
}
