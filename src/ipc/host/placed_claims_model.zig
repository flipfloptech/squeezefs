//! The placed-sever claims core as a MODEL (charter §9 "model-check every
//! lock-free core"; spec `docs/porting/il_daemon.md` §3.2 "the Dekker"):
//! a claimer that publishes `writers` THEN checks `sealed` and copies, an
//! adopter that publishes `sealed` THEN reads `writers` and adopts; both
//! sides SeqCst. The model carries store buffering explicitly (a per-thread
//! pending store the fence step drains), so dropping EITHER side's drain —
//! the release/acquire program — lets both read the pre-race value: an
//! adoption over a copy in flight (the adopter hands a cell to the table
//! while bytes are still landing in it) — the invariant this core exists
//! to forbid. A third thread models a sibling claimer whose claim must be
//! refused once the seal landed (`claim_after_seal_never_lands`).
//!
//! `placed.Claims.beginWrite / endWrite / sealForAdoption` is this program.

const std = @import("std");
const explorer = @import("../../model/explorer.zig");

pub const Variant = enum { shipped, no_claimer_fence, no_adopter_fence, seal_check_before_publish };

pub fn Dekker(comptime variant: Variant) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        const claimer = 0;
        const adopter = 1;
        const sibling = 2;

        // ---- main memory ----
        writers: u8 = 0,
        sealed: bool = false,
        /// Bytes the claimer landed in the cell.
        copied: bool = false,
        // ---- store buffers (TSO) ----
        pend_writers: u8 = 0,
        pend_sealed: bool = false,
        // ---- registers / outcomes ----
        claimer_in_window: bool = false,
        claimer_wrote: bool = false,
        adopted: bool = false,
        sibling_wrote: bool = false,
        sibling_in_window: bool = false,
        pc: [3]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == 9;
        }
        fn drainWriters(self: *Self) void {
            self.writers +%= self.pend_writers;
            self.pend_writers = 0;
        }
        fn drainSealed(self: *Self) void {
            if (self.pend_sealed) {
                self.sealed = true;
                self.pend_sealed = false;
            }
        }
        fn stepClaimer(self: *Self, t: usize, in_window: *bool, wrote: *bool) void {
            switch (self.pc[t]) {
                0 => {
                    // writers += 1 (buffered store).
                    self.pend_writers +%= 1;
                    self.pc[t] = 1;
                },
                1 => {
                    // The SeqCst drain (the fence the weakening drops).
                    if (variant != .no_claimer_fence) self.drainWriters();
                    self.pc[t] = 2;
                },
                2 => {
                    // "Did you seal?" — a load of main memory.
                    if (self.sealed) {
                        // Refused: undo the writer, pooled path.
                        self.drainWriters();
                        self.writers -%= 1;
                        self.pc[t] = 9;
                    } else {
                        in_window.* = true;
                        self.pc[t] = 3;
                    }
                },
                3 => {
                    // The copy lands.
                    self.copied = true;
                    wrote.* = true;
                    self.pc[t] = 4;
                },
                4 => {
                    // endWrite: the window closes.
                    self.drainWriters();
                    self.writers -%= 1;
                    in_window.* = false;
                    self.pc[t] = 9;
                },
                else => unreachable,
            }
        }
        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                claimer => self.stepClaimer(t, &self.claimer_in_window, &self.claimer_wrote),
                sibling => {
                    // The sibling claims only after the adopter has published
                    // its seal (the "later chunk" of the cohort).
                    if (self.pc[sibling] == 0 and self.pc[adopter] < 2) return;
                    self.stepClaimer(t, &self.sibling_in_window, &self.sibling_wrote);
                },
                adopter => switch (self.pc[adopter]) {
                    0 => {
                        if (variant == .seal_check_before_publish) {
                            // The broken order: read writers first.
                            if (self.writers == 0) self.pc[adopter] = 1 else self.pc[adopter] = 9;
                        } else {
                            // sealed = true (buffered store).
                            self.pend_sealed = true;
                            self.pc[adopter] = 1;
                        }
                    },
                    1 => {
                        if (variant == .seal_check_before_publish) {
                            self.pend_sealed = true;
                            self.adopted = true;
                            self.pc[adopter] = 3;
                        } else {
                            // The SeqCst drain (the fence the weakening drops).
                            if (variant != .no_adopter_fence) self.drainSealed();
                            self.pc[adopter] = 2;
                        }
                    },
                    2 => {
                        // "Any writer?" — a load of main memory.
                        if (self.writers == 0) self.adopted = true;
                        self.pc[adopter] = 3;
                    },
                    3 => {
                        // The store buffer drains eventually.
                        self.drainSealed();
                        self.pc[adopter] = 9;
                    },
                    else => unreachable,
                },
                else => unreachable,
            }
        }
        pub fn invariant(self: *const Self) !void {
            // An adoption while a claimer is inside its writer window: the
            // table's cell is still receiving bytes.
            if (self.adopted and (self.claimer_in_window or self.sibling_in_window)) return error.AdoptedOverAWriter;
        }
        pub fn final(self: *const Self) !void {
            // Every write that landed did so before the seal, or the
            // adoption was refused: `adopted ⇒ copies landed before the seal`.
            // A sibling that claimed after the seal was published must have
            // been refused.
            if (self.pc[adopter] == 9 and self.sealed and self.sibling_wrote and self.adopted) return error.ClaimAfterSealLanded;
        }
    };
}

test "placed claims Dekker: an adoption never lands over a writer in its window, a claim after the seal never lands — and dropping either side's fence or sealing after the check breaks it" {
    try explorer.expectHolds(Dekker(.shipped), .{});
    try explorer.expectFails(Dekker(.no_claimer_fence), .{}, error.InvariantViolated);
    try explorer.expectFails(Dekker(.no_adopter_fence), .{}, error.InvariantViolated);
    try explorer.expectFails(Dekker(.seal_check_before_publish), .{}, error.InvariantViolated);
}
