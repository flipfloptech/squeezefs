//! The exhaustive model of the write-lease recall (`write_lease_core.zig`)
//! with EXPLICIT store buffers (TSO, as `doorbell_model.zig`): a thread's
//! store lands in its own buffer, is forwarded to its own loads and reaches
//! main memory at its FENCE step; a SeqCst RMW is a drained store. Two
//! threads: the CLIENT (count in → read recall → device write → count out)
//! and the DAEMON (recall := 1 → read inflight → wait for 0 → MUTATE the
//! layout). The invariant after every step: never (the daemon mutating ∧ a
//! client write in flight on the device). The final check: the client's
//! write either landed before the mutation or was refused — never lost.
//! Weakenings proven red: drop the client's fence (its count reaches main
//! memory after the daemon read 0), drop the daemon's fence (its recall
//! reaches the client after the client read LIVE); each is the Dekker
//! store-buffering failure.

const std = @import("std");
const explorer = @import("../model/explorer.zig");

pub const Variant = enum { shipped, no_client_fence, no_daemon_fence };

pub fn WriteLease(comptime variant: Variant) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const client = 0;
        const daemon = 1;

        // ---- main memory ----
        inflight: u8 = 0,
        recall: u8 = 0,
        // ---- store buffers ----
        pend_inflight: u8 = 0,
        pend_recall: bool = false,
        // ---- the world ----
        /// The client's device write is in flight (issued, not completed).
        writing: bool = false,
        /// The client's write landed on the device (before any mutation?).
        landed: bool = false,
        landed_before_mutation: bool = false,
        refused: bool = false,
        mutated: bool = false,
        // ---- registers ----
        c_recall: u8 = 0,
        d_inflight: u8 = 0,
        pc: [2]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == 9;
        }
        pub fn step(self: *Self, t: usize) void {
            if (t == client) self.stepClient() else self.stepDaemon();
        }

        fn drainInflight(self: *Self) void {
            self.inflight +%= self.pend_inflight;
            self.pend_inflight = 0;
        }
        fn drainRecall(self: *Self) void {
            if (self.pend_recall) {
                self.recall = 1;
                self.pend_recall = false;
            }
        }
        /// The client's view of `recall`: main memory (nothing of its own
        /// is buffered there); the daemon's view of `inflight` likewise.
        fn stepClient(self: *Self) void {
            switch (self.pc[client]) {
                0 => {
                    // inflight += 1: buffered (forwarded to our own reads).
                    self.pend_inflight = 1;
                    self.pc[client] = 1;
                },
                1 => {
                    // The fence: the count reaches main memory before the load.
                    if (variant != .no_client_fence) self.drainInflight();
                    self.pc[client] = 2;
                },
                2 => {
                    self.c_recall = self.recall;
                    self.pc[client] = if (self.c_recall != 0) 6 else 3;
                },
                3 => {
                    // The device write is issued.
                    self.writing = true;
                    self.pc[client] = 4;
                },
                4 => {
                    // It lands.
                    self.writing = false;
                    self.landed = true;
                    self.landed_before_mutation = !self.mutated;
                    self.pc[client] = 5;
                },
                5 => {
                    // inflight -= 1 (a drained RMW), with any late buffer.
                    self.drainInflight();
                    self.inflight -%= 1;
                    self.pc[client] = 9;
                },
                6 => {
                    // Refused: undo the count (drained RMW).
                    self.drainInflight();
                    self.inflight -%= 1;
                    self.refused = true;
                    self.pc[client] = 9;
                },
                else => unreachable,
            }
        }
        fn stepDaemon(self: *Self) void {
            switch (self.pc[daemon]) {
                0 => {
                    // recall := 1: buffered.
                    self.pend_recall = true;
                    self.pc[daemon] = 1;
                },
                1 => {
                    if (variant != .no_daemon_fence) self.drainRecall();
                    self.pc[daemon] = 2;
                },
                2 => {
                    self.d_inflight = self.inflight;
                    self.pc[daemon] = if (self.d_inflight == 0) 4 else 3;
                },
                3 => {
                    // Waiting: re-read (the futex wait's wake is the client's
                    // decrement; the model polls).
                    self.drainRecall();
                    self.d_inflight = self.inflight;
                    if (self.d_inflight == 0) self.pc[daemon] = 4;
                },
                4 => {
                    // The mutation.
                    self.drainRecall();
                    self.mutated = true;
                    self.pc[daemon] = 9;
                },
                else => unreachable,
            }
        }
        pub fn invariant(self: *const Self) !void {
            if (self.mutated and self.writing) return error.WriteInFlightUnderMutation;
        }
        pub fn final(self: *const Self) !void {
            if (!self.refused and !(self.landed and self.landed_before_mutation)) return error.WriteLost;
            if (self.refused and self.landed) return error.RefusedYetLanded;
        }
    };
}

test "the write-lease recall: the daemon never mutates with a client write in flight and a client write is landed-before or refused — and dropping either side's fence (store buffering) breaks it" {
    try explorer.expectHolds(WriteLease(.shipped), .{});
    try explorer.expectFails(WriteLease(.no_client_fence), .{}, error.InvariantViolated);
    try explorer.expectFails(WriteLease(.no_daemon_fence), .{}, error.InvariantViolated);
}
