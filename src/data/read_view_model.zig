//! The read view's STALENESS model (`read_view.zig`, `meta/ino_witness.zig`):
//! a MUTATOR landing a record of the ino (the leaf apply, then the witness
//! bump), a FILLER caching the ino off the KV path (witness read → lookup
//! → witness re-read → the slot's seqlock'd write) and a READER serving the
//! slot (seq → the words → seq → the witness). Standalone and dependency-
//! free (`model/explorer.zig`), one atomic action per step.
//!
//! Shared words: `kv` (the record as the KV holds it: 0 then 1), `gen` (the
//! witness), the slot `{seq, data, sgen}`. The law: a SERVED read carries
//! the record the KV holds, or lags it only by a mutation whose bump has not
//! run — a commit nobody was acked for, so the read is concurrent with it.
//! Weakenings that fail: `bump_before_apply` (the witness ahead of the
//! visibility: a filler between the two caches the old record under the new
//! generation, forever current); `reader_no_gen_check` (the view served
//! without the witness compare); `reader_no_seq_recheck` (a torn read: the
//! old data under the new fill's generation). The filler's post-lookup
//! re-read of the witness is an economy, not a load-bearing step: without
//! it a fill under an already-moved generation is simply stale at its first
//! read (`filler_no_recheck` holds).

pub const Weakening = enum { none, bump_before_apply, reader_no_gen_check, reader_no_seq_recheck, filler_no_recheck };

pub fn StalenessModel(comptime weak: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        const M = 0;
        const F = 1;
        const R = 2;

        kv: u8 = 0,
        gen: u8 = 0,
        seq: u8 = 0,
        data: u8 = 0,
        sgen: u8 = 0,
        pc: [3]u8 = @splat(0),
        /// The filler's registers.
        f_g0: u8 = 0,
        f_v: u8 = 0,
        /// The reader's registers; `stale` = it served a view the law
        /// forbids (judged at the serve: a later mutation is not its lag).
        r_s0: u8 = 0,
        r_d: u8 = 0,
        r_sg: u8 = 0,
        stale: bool = false,

        pub fn init() Self {
            // The slot starts filled with the ORIGINAL record under the
            // original generation (a warm view), seq even.
            return .{};
        }

        const M_END: u8 = 2;
        const F_END: u8 = 6;
        const R_END: u8 = 5;

        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] >= switch (t) {
                M => M_END,
                F => F_END,
                else => R_END,
            };
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                M => {
                    const apply_first = weak != .bump_before_apply;
                    switch (self.pc[M]) {
                        0 => if (apply_first) {
                            self.kv = 1;
                        } else {
                            self.gen += 1;
                        },
                        1 => if (apply_first) {
                            self.gen += 1;
                        } else {
                            self.kv = 1;
                        },
                        else => unreachable,
                    }
                    self.pc[M] += 1;
                },
                F => {
                    switch (self.pc[F]) {
                        // The witness before the lookups.
                        0 => self.f_g0 = self.gen,
                        // The KV lookup.
                        1 => self.f_v = self.kv,
                        // The witness after: a moved generation skips the fill.
                        2 => if (weak != .filler_no_recheck and self.gen != self.f_g0) {
                            self.pc[F] = F_END;
                            return;
                        },
                        // seq odd; the words; seq even.
                        3 => self.seq += 1,
                        4 => {
                            self.data = self.f_v;
                            self.sgen = self.f_g0;
                        },
                        5 => self.seq += 1,
                        else => unreachable,
                    }
                    self.pc[F] += 1;
                },
                R => {
                    switch (self.pc[R]) {
                        0 => {
                            self.r_s0 = self.seq;
                            if (self.r_s0 & 1 != 0) {
                                self.pc[R] = R_END;
                                return;
                            }
                        },
                        1 => self.r_d = self.data,
                        2 => self.r_sg = self.sgen,
                        3 => if (weak != .reader_no_seq_recheck and self.seq != self.r_s0) {
                            self.pc[R] = R_END;
                            return;
                        },
                        4 => if (weak == .reader_no_gen_check or self.gen == self.r_sg) {
                            // Served: the law judged now. A lag is admissible
                            // only behind a mutation applied and not yet
                            // bumped (pc[M] == 1 with the apply first).
                            const lagging_unacked = weak != .bump_before_apply and self.pc[M] == 1;
                            self.stale = self.r_d != self.kv and !lagging_unacked;
                        },
                        else => unreachable,
                    }
                    self.pc[R] += 1;
                },
                else => unreachable,
            }
        }

        pub fn invariant(self: *const Self) !void {
            if (self.stale) return error.StaleViewServed;
        }

        pub fn final(_: *const Self) !void {}
    };
}

const explorer = @import("../model/explorer.zig");

test "the read view's staleness model holds; the witness bumped before the apply, a read without the witness compare and a read without the seq recheck each serve a stale view (the recorded weakenings); the filler's post-lookup recheck is an economy" {
    try explorer.expectHolds(StalenessModel(.none), .{});
    try explorer.expectHolds(StalenessModel(.filler_no_recheck), .{});
    try explorer.expectFails(StalenessModel(.bump_before_apply), .{}, error.InvariantViolated);
    try explorer.expectFails(StalenessModel(.reader_no_gen_check), .{}, error.InvariantViolated);
    try explorer.expectFails(StalenessModel(.reader_no_seq_recheck), .{}, error.InvariantViolated);
}
