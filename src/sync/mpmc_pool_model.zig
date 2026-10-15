//! Explorer model of a POOL FREE LIST on the bounded MPMC ring
//! (`sync/mpmc.zig`): the ring holds the pool's WHOLE population (the FUSE
//! sideband's two cell pools, decision 0003 §AI.4; the `uring_fs` and
//! journal-lane request cells), several threads take a cell and give it
//! back.
//!
//! Vyukov's push at position `pos` needs the cell at `pos & mask` to read
//! `seq == pos`, which the pop at `pos − cap` stores AFTER its `dequeue_pos`
//! CAS. A consumer preempted between the two leaves that cell reading
//! "filled", while a SECOND consumer's pops and pushes run past it: the push
//! that laps it reads `seq < pos` and answers FULL with the pool's
//! population short of the capacity. On a ring whose population never
//! exceeds its capacity a refusal is never "full" — the owner retries.
//!
//! Two threads, `population` cells all queued at the start in a `cap`-cell
//! ring; each thread pops a cell and pushes it back `rounds` times. Every
//! action on a shared word is one step; the data copy rides the step that
//! publishes or consumes it (sequential consistency: the release/acquire
//! pairs on `seq` order them in the product).
//!
//! Weakening ledger:
//!   `.push_once` — the give-back is ONE `tryPush` whose refusal is
//!     asserted or dropped (the sideband's `recycle`, the `uring_fs` and
//!     journal-lane `freeCell` before their fixes: the cell is LOST): the
//!     explorer finds T0 holding position 0 mid-pop while T1 pops position
//!     1 and its push at position 2 meets cell 0 still filled.
//!   `.push_once` on a ring TWICE the population — oversizing is no fix:
//!     T1 cycles until its push laps T0's stalled cell and is refused.
//!   `.unless_full_d_first` — `pushSpinUnlessFull`'s fullness check reads
//!     `dequeue_pos` BEFORE `enqueue_pos`: T0 cycles between the two loads
//!     and the stale `d` over-reads the occupancy as FULL.
//!   The shipped give-backs hold — `pushSpin` (a refusal re-reads
//!   `enqueue_pos` and retries) and `pushSpinUnlessFull` (retries unless
//!   `enqueue_pos`, then `dequeue_pos`, read FULL): no refusal surfaces, and
//!   at the end the ring holds each cell exactly once.

const std = @import("std");
const explorer = @import("../model/explorer.zig");

const GiveBack = enum { push_once, push_spin, unless_full, unless_full_d_first };

fn PoolModel(comptime cap: u8, comptime population: u8, comptime rounds: u8, comptime give_back: GiveBack) type {
    std.debug.assert(std.math.isPowerOfTwo(cap) and population <= cap);
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const mask: u8 = cap - 1;

        seq: [cap]u8,
        data: [cap]u8,
        enqueue_pos: u8 = population,
        dequeue_pos: u8 = 0,
        pc: [thread_count]u8 = @splat(0),
        pos: [thread_count]u8 = @splat(0),
        held: [thread_count]u8 = @splat(0),
        /// The fullness check's first load (`enqueue_pos`, or `dequeue_pos`
        /// in the weakening).
        first: [thread_count]u8 = @splat(0),
        left: [thread_count]u8 = @splat(rounds),
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
            const full_first = 8;
            const full_second = 9;
            const finished = 10;
        };

        pub fn init() Self {
            var s: Self = .{ .seq = undefined, .data = undefined };
            // Cell i: filled for the consumer at position i below the
            // population, free for the producer at position i above it.
            for (0..cap) |i| {
                s.seq[i] = @intCast(if (i < population) i + 1 else i);
                s.data[i] = @intCast(i);
            }
            return s;
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
                    // one cell, so the pool never runs dry for good).
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
                    } else if (diff > 0) {
                        self.pc[t] = Pc.push_load_pos;
                    } else switch (give_back) {
                        .push_once => {
                            self.refused = true;
                            self.pc[t] = Pc.finished;
                        },
                        .push_spin => self.pc[t] = Pc.push_load_pos,
                        .unless_full, .unless_full_d_first => self.pc[t] = Pc.full_first,
                    }
                },
                Pc.full_first => {
                    self.first[t] = if (give_back == .unless_full_d_first) self.dequeue_pos else self.enqueue_pos;
                    self.pc[t] = Pc.full_second;
                },
                Pc.full_second => {
                    const e = if (give_back == .unless_full_d_first) self.enqueue_pos else self.first[t];
                    const d = if (give_back == .unless_full_d_first) self.first[t] else self.dequeue_pos;
                    if (@as(i8, @bitCast(e -% d)) >= cap) {
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
                    self.left[t] -= 1;
                    self.pc[t] = if (self.left[t] == 0) Pc.finished else Pc.pop_load_pos;
                },
                else => unreachable,
            }
        }

        pub fn invariant(self: *const Self) !void {
            if (self.refused) return error.PoolPushRefused;
        }

        pub fn final(self: *const Self) !void {
            // Every cell queued, each exactly once, all filled.
            if (self.enqueue_pos -% self.dequeue_pos != population) return error.CellLost;
            var seen: [cap]bool = @splat(false);
            var p = self.dequeue_pos;
            while (p != self.enqueue_pos) : (p +%= 1) {
                if (self.seq[p & mask] != p +% 1) return error.CellNotPublished;
                const c = self.data[p & mask];
                if (c >= population or seen[c]) return error.CellDuplicated;
                seen[c] = true;
            }
        }
    };
}

test "a pool free list on the MPMC ring at its population: the retried give-back (pushSpin) holds over every schedule; ONE asserted tryPush is refused when a second consumer runs past a pop between its CAS and its sequence store (the weakening)" {
    try explorer.expectHolds(PoolModel(2, 2, 1, .push_spin), .{});
    try explorer.expectFails(PoolModel(2, 2, 1, .push_once), .{}, error.InvariantViolated);
}

test "oversizing the free list's ring is no fix: ONE tryPush on a ring twice the population is still refused once the second consumer laps the stalled pop's cell" {
    try explorer.expectFails(PoolModel(4, 2, 3, .push_once), .{}, error.InvariantViolated);
}

test "pushSpinUnlessFull on a ring at its population never answers full over every schedule (two rounds each); its weakening — dequeue_pos read before enqueue_pos — over-reads a stale occupancy as FULL" {
    try explorer.expectHolds(PoolModel(2, 2, 2, .unless_full), .{});
    try explorer.expectHolds(PoolModel(4, 2, 3, .unless_full), .{});
    try explorer.expectFails(PoolModel(2, 2, 2, .unless_full_d_first), .{}, error.InvariantViolated);
}
