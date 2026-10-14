//! Explorer model of a POOL FREE LIST on the bounded MPMC ring
//! (`sync/mpmc.zig`): the ring holds the pool's WHOLE population (its
//! capacity, a power of two, IS the cell count — the FUSE sideband's two
//! cell pools, decision 0003 §AI.4), several threads take a cell and give
//! it back.
//!
//! Vyukov's push at position `pos` needs the cell at `pos & mask` to read
//! `seq == pos`, which the pop at `pos − cap` stores AFTER its `dequeue_pos`
//! CAS. A consumer preempted between the two leaves that cell reading
//! "filled", while a SECOND consumer's pop and push run past it: the push
//! reads `seq < pos` and answers FULL with the pool's population one short
//! of the capacity. On a ring that holds every slot a refusal is never
//! "full" — the owner retries (`pushSpin`); an asserted `tryPush` aborts.
//!
//! Two threads, two cells (the smallest ring that distinguishes full from
//! empty), both queued at the start; each thread pops a cell and pushes it
//! back once. Every action on a shared word is one step; the data copy
//! rides the step that publishes or consumes it (sequential consistency:
//! the release/acquire pairs on `seq` order them in the product).
//!
//! Weakening ledger:
//!   `.push_once` — the give-back is ONE `tryPush` whose refusal is
//!     asserted (the sideband's `recycle` before the fix): the explorer
//!     finds T0 holding position 0 mid-pop while T1 pops position 1 and its
//!     push at position 2 meets cell 0 still filled — the invariant fails.
//!   The shipped give-back (`pushSpin`: a refusal re-reads `enqueue_pos`
//!   and retries) holds: no refusal surfaces, and at the end the ring holds
//!   each cell exactly once.

const std = @import("std");
const explorer = @import("../model/explorer.zig");

fn PoolModel(comptime push_once: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const cap: u8 = 2;
        const mask: u8 = cap - 1;

        seq: [cap]u8 = .{ 1, 2 },
        data: [cap]u8 = .{ 0, 1 },
        enqueue_pos: u8 = 2,
        dequeue_pos: u8 = 0,
        pc: [thread_count]u8 = @splat(0),
        pos: [thread_count]u8 = @splat(0),
        held: [thread_count]u8 = @splat(0),
        refused: bool = false,

        const Pc = struct {
            const pop_load_pos = 0;
            const pop_load_seq = 1;
            const pop_cas = 2;
            const pop_release = 3;
            const push_load_pos = 4;
            const push_load_seq = 5;
            const push_cas = 6;
            const push_publish = 7;
            const finished = 8;
        };

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == Pc.finished;
        }

        pub fn step(self: *Self, t: usize) void {
            switch (self.pc[t]) {
                Pc.pop_load_pos => {
                    self.pos[t] = self.dequeue_pos;
                    self.pc[t] = Pc.pop_load_seq;
                },
                Pc.pop_load_seq => {
                    const p = self.pos[t];
                    const s = self.seq[p & mask];
                    // `seq == pos + 1`: filled for this consumer; anything
                    // else re-reads the position (each thread holds at most
                    // one cell, so the pool never runs dry).
                    self.pc[t] = if (s == p +% 1) Pc.pop_cas else Pc.pop_load_pos;
                },
                Pc.pop_cas => {
                    if (self.dequeue_pos == self.pos[t]) {
                        self.dequeue_pos +%= 1;
                        self.pc[t] = Pc.pop_release;
                    } else self.pc[t] = Pc.pop_load_pos;
                },
                Pc.pop_release => {
                    const p = self.pos[t];
                    self.held[t] = self.data[p & mask];
                    self.seq[p & mask] = p +% cap;
                    self.pc[t] = Pc.push_load_pos;
                },
                Pc.push_load_pos => {
                    self.pos[t] = self.enqueue_pos;
                    self.pc[t] = Pc.push_load_seq;
                },
                Pc.push_load_seq => {
                    const p = self.pos[t];
                    const diff = @as(i8, @bitCast(self.seq[p & mask] -% p));
                    if (diff == 0) {
                        self.pc[t] = Pc.push_cas;
                    } else if (diff < 0 and push_once) {
                        self.refused = true;
                        self.pc[t] = Pc.finished;
                    } else self.pc[t] = Pc.push_load_pos;
                },
                Pc.push_cas => {
                    if (self.enqueue_pos == self.pos[t]) {
                        self.enqueue_pos +%= 1;
                        self.pc[t] = Pc.push_publish;
                    } else self.pc[t] = Pc.push_load_pos;
                },
                Pc.push_publish => {
                    const p = self.pos[t];
                    self.data[p & mask] = self.held[t];
                    self.seq[p & mask] = p +% 1;
                    self.pc[t] = Pc.finished;
                },
                else => unreachable,
            }
        }

        pub fn invariant(self: *const Self) !void {
            if (self.refused) return error.PoolPushRefused;
        }

        pub fn final(self: *const Self) !void {
            // Both cells queued, each exactly once, both filled.
            if (self.enqueue_pos -% self.dequeue_pos != cap) return error.CellLost;
            var seen: [cap]bool = @splat(false);
            var p = self.dequeue_pos;
            while (p != self.enqueue_pos) : (p +%= 1) {
                if (self.seq[p & mask] != p +% 1) return error.CellNotPublished;
                const c = self.data[p & mask];
                if (c >= cap or seen[c]) return error.CellDuplicated;
                seen[c] = true;
            }
        }
    };
}

test "a pool free list on the MPMC ring at its population: the retried give-back (pushSpin) holds over every schedule; ONE asserted tryPush is refused when a second consumer runs past a pop between its CAS and its sequence store (the weakening)" {
    try explorer.expectHolds(PoolModel(false), .{});
    try explorer.expectFails(PoolModel(true), .{}, error.InvariantViolated);
}
