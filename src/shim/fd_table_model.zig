//! Exhaustive models of the bound-fd table's lock-free cores (`fd_table.zig`),
//! explored with `model/explorer.zig` under SC + explicit per-thread store
//! buffers. Each model ships with its WEAKENING proven to fail:
//!
//!  1. `MirrorDekker`: the offset mirror's op side (`publish` = store offset →
//!     SeqCst fence → load armed) against the demote side (`disarmIfCurrent`
//!     = swap armed → SeqCst fence → load offset). Holds: the final offset
//!     reaches at least one consumer (the op writes it through when it saw
//!     the disarm; the demoter flushes it when it loaded it). WEAKENING:
//!     drop the op side's fence (a Release store leaves the offset in the
//!     store buffer while the arm is loaded) — both miss it: FinalViolated.
//!  2. `RefcountDrop`: two closers of dup siblings racing a pinning reader
//!     on one cell. Holds: exactly one release reports `last`; no acquire
//!     succeeds on a zero count; the count ends at zero. WEAKENING: an
//!     `acquire` that increments without refusing zero (the plain
//!     `fetch_add` shape) resurrects a cell whose last reference already
//!     reported — a second `last` for one binding: InvariantViolated.
//!  3. `ForkEpochBind`: a bind racing the fork PREPARE's epoch bump. Holds:
//!     a description the child shares (its `open` returned before the
//!     bump) is never under an authoritative parent mirror — either the
//!     walk flushed it or the stale snapshot refused the arm. WEAKENING:
//!     the snapshot taken at BIND time instead of before the `open` — an
//!     fd opened before the bump and bound after it arms authoritative
//!     while the child shares its `f_pos`: FinalViolated.

const std = @import("std");
const explorer = @import("../model/explorer.zig");

// ---------------------------------------------------------------------------
// 1. The Dekker pair with store buffering.
// ---------------------------------------------------------------------------

pub fn MirrorDekker(comptime op_fence: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const OP = 0;
        const DEMOTE = 1;
        const FINAL: u8 = 7;

        // Shared memory.
        offset: u8 = 1,
        armed: bool = true,
        // The op thread's pending store (TSO store buffer).
        op_pending: ?u8 = null,
        // Outcomes.
        op_saw_armed: ?bool = null,
        demote_loaded: ?u8 = null,
        pc: [2]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return if (t == OP) self.pc[t] == 4 else self.pc[t] == 3;
        }
        pub fn step(self: *Self, t: usize) void {
            if (t == OP) {
                switch (self.pc[OP]) {
                    0 => {
                        // store offset (buffered)
                        self.op_pending = FINAL;
                        self.pc[OP] = 1;
                    },
                    1 => {
                        // the SeqCst fence drains the buffer (weakened: no drain)
                        if (op_fence) self.drainOp();
                        self.pc[OP] = 2;
                    },
                    2 => {
                        // load armed
                        self.op_saw_armed = self.armed;
                        self.pc[OP] = 3;
                    },
                    3 => {
                        // the buffer drains eventually (after the load)
                        self.drainOp();
                        self.pc[OP] = 4;
                    },
                    else => {},
                }
            } else {
                switch (self.pc[DEMOTE]) {
                    0 => {
                        // swap armed (an RMW is a full barrier)
                        self.armed = false;
                        self.pc[DEMOTE] = 1;
                    },
                    1 => {
                        // the SeqCst fence
                        self.pc[DEMOTE] = 2;
                    },
                    2 => {
                        // load offset
                        self.demote_loaded = self.offset;
                        self.pc[DEMOTE] = 3;
                    },
                    else => {},
                }
            }
        }
        fn drainOp(self: *Self) void {
            if (self.op_pending) |v| {
                self.offset = v;
                self.op_pending = null;
            }
        }
        pub fn invariant(_: *const Self) !void {}
        pub fn final(self: *const Self) !void {
            // The op writes through iff it saw the disarm; the demoter
            // flushes what it loaded. The final offset must reach one.
            const op_writes_through = self.op_saw_armed.? == false;
            const demote_has_final = self.demote_loaded.? == FINAL;
            if (!op_writes_through and !demote_has_final) return error.FinalOffsetLost;
        }
    };
}

// ---------------------------------------------------------------------------
// 2. The refcount law.
// ---------------------------------------------------------------------------

pub fn RefcountDrop(comptime acquire_refuses_zero: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        const READER = 2;

        /// Two references (two dup'd fds).
        refs: u8 = 2,
        lasts: u8 = 0,
        /// Per closer: the count it loaded for its CAS.
        seen: [2]u8 = @splat(0),
        /// The reader: acquire (load + CAS) then release.
        reader_seen: u8 = 0,
        reader_holds: bool = false,
        resurrected: bool = false,
        pc: [3]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return if (t == READER) self.pc[t] == 4 else self.pc[t] == 2;
        }
        pub fn step(self: *Self, t: usize) void {
            if (t == READER) {
                switch (self.pc[READER]) {
                    0 => {
                        self.reader_seen = self.refs;
                        self.pc[READER] = 1;
                    },
                    1 => {
                        // acquire CAS: refuses 0 (weakened: increments anyway)
                        if (acquire_refuses_zero and self.reader_seen == 0) {
                            self.pc[READER] = 4;
                        } else if (self.refs == self.reader_seen) {
                            self.refs += 1;
                            self.reader_holds = true;
                            if (self.reader_seen == 0) self.resurrected = true;
                            self.pc[READER] = 2;
                        } else {
                            self.pc[READER] = 0;
                        }
                    },
                    2 => {
                        self.reader_seen = self.refs;
                        self.pc[READER] = 3;
                    },
                    3 => {
                        // release CAS
                        if (self.refs == self.reader_seen) {
                            self.releaseFrom(self.reader_seen);
                            self.reader_holds = false;
                            self.pc[READER] = 4;
                        } else {
                            self.pc[READER] = 2;
                        }
                    },
                    else => {},
                }
            } else {
                switch (self.pc[t]) {
                    0 => {
                        self.seen[t] = self.refs;
                        self.pc[t] = 1;
                    },
                    1 => {
                        // release CAS: saturates at 0
                        if (self.seen[t] == 0) {
                            self.pc[t] = 2;
                        } else if (self.refs == self.seen[t]) {
                            self.releaseFrom(self.seen[t]);
                            self.pc[t] = 2;
                        } else {
                            self.pc[t] = 0;
                        }
                    },
                    else => {},
                }
            }
        }
        fn releaseFrom(self: *Self, seen: u8) void {
            self.refs = seen - 1;
            if (seen == 1) self.lasts += 1;
        }
        pub fn invariant(self: *const Self) !void {
            if (self.resurrected) return error.ZeroCountResurrected;
            if (self.lasts > 1) return error.LastReportedTwice;
        }
        pub fn final(self: *const Self) !void {
            if (self.lasts != 1) return error.LastNotReportedOnce;
            if (self.refs != 0) return error.CountNotZero;
        }
    };
}

// ---------------------------------------------------------------------------
// 3. The fork epoch vs a racing bind.
// ---------------------------------------------------------------------------

pub fn ForkEpochBind(comptime snapshot_before_open: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const BINDER = 0;
        const FORKER = 1;

        epoch: u8 = 0,
        /// The cell.
        armed: bool = false,
        cell_epoch: u8 = 0,
        /// The binder's snapshot and whether its `open` has returned (an
        /// fd that exists at the fork point is one the child SHARES).
        snapshot: u8 = 0,
        opened: bool = false,
        /// The child's view, taken at the PREPARE (the epoch bump): does it
        /// share the description?
        child_has_fd: bool = false,
        flushed: bool = false,
        pc: [2]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == 2;
        }
        pub fn step(self: *Self, t: usize) void {
            if (t == BINDER) {
                switch (self.pc[BINDER]) {
                    0 => {
                        // the snapshot BEFORE the open (the law), then the open
                        if (snapshot_before_open) self.snapshot = self.epoch;
                        self.opened = true;
                        self.pc[BINDER] = 1;
                    },
                    1 => {
                        // bind: the weakened shape snapshots here instead
                        if (!snapshot_before_open) self.snapshot = self.epoch;
                        self.cell_epoch = self.snapshot;
                        self.armed = self.snapshot == self.epoch;
                        self.pc[BINDER] = 2;
                    },
                    else => {},
                }
            } else {
                switch (self.pc[FORKER]) {
                    0 => {
                        // PREPARE: the epoch bump = the fork point for the
                        // child's fd table
                        self.epoch += 1;
                        self.child_has_fd = self.opened;
                        self.pc[FORKER] = 1;
                    },
                    1 => {
                        // the walk: flush cells armed under the previous epoch
                        if (self.armed and self.cell_epoch == self.epoch - 1) {
                            self.armed = false;
                            self.flushed = true;
                        }
                        self.pc[FORKER] = 2;
                    },
                    else => {},
                }
            }
        }
        pub fn invariant(_: *const Self) !void {}
        pub fn final(self: *const Self) !void {
            // A description the child shares must never be under an
            // authoritative parent mirror (the parent's offsets would never
            // reach the kernel `f_pos` the child consumes: the lost update).
            const authoritative = self.armed and self.cell_epoch == self.epoch;
            if (authoritative and self.child_has_fd) return error.SharedDescriptionArmed;
        }
    };
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the mirror Dekker pair holds with both SeqCst fences; the op side's fence removed loses the final offset" {
    try explorer.expectHolds(MirrorDekker(true), .{});
    try explorer.expectFails(MirrorDekker(false), .{}, error.FinalViolated);
}

test "the refcount law: two closers + a pinning reader report last exactly once; the acquire's zero refusal removed resurrects a dead cell" {
    try explorer.expectHolds(RefcountDrop(true), .{});
    try explorer.expectFails(RefcountDrop(false), .{}, error.InvariantViolated);
}

test "a bind racing the fork's epoch bump never arms a description the child shares; the snapshot moved to bind time arms one" {
    try explorer.expectHolds(ForkEpochBind(true), .{});
    try explorer.expectFails(ForkEpochBind(false), .{}, error.FinalViolated);
}
