//! The device-overlay models (charter §9; `docs/porting/data_write_path.md`
//! §3.2, §3.3): the `begin_store / inflight_empty` Dekker pair under store
//! buffering, the validated serve that is never torn, and the §3.3
//! stale-DMA counterexample made unrepresentable by the range claims. Each
//! with the weakening that fails it.
//!
//! Weakenings (recorded):
//! * `writer_fence` / `freezer_fence` — either side's SeqCst point weakened
//!   to release/acquire (its store may stay buffered past its cross-word
//!   load): the SB interleaving admits a store that neither backed out on
//!   the freeze nor was visible to the freezer's drain (`FinalViolated`:
//!   `stored ≠ covered_at_drain` after `inflight_empty` was observed).
//! * `no_revalidate` — a reader that skips the generation-equality
//!   re-check serves a mix of two passes' bytes (`InvariantViolated`).
//! * `no_claims` — overlapping stores admitted concurrently: B (gen g+1)
//!   lands first, A (gen g) lands LAST, the device holds A's older bytes
//!   under a coverage and generation that say B (`FinalViolated`).

const std = @import("std");
const explorer = @import("../model/explorer.zig");

// ---------------------------------------------------------------------------
// Model 1: overlay_freeze_drain_observes_final_coverage.
// ---------------------------------------------------------------------------

pub const PairWeakening = enum { none, writer_fence, freezer_fence };

pub fn FreezeDrainModel(comptime weakening: PairWeakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const W = 0;
        const F = 1;

        inflight: u8 = 0,
        frozen: u8 = 0,
        covered: u8 = 0,
        stored: u8 = 0,
        /// Per-thread store buffers (TSO).
        w_pending: u8 = 0,
        f_pending: u8 = 0,
        w_pc: u8 = 0,
        f_pc: u8 = 0,
        backed_out: u8 = 0,
        observed_empty: u8 = 0,
        covered_at_drain: u8 = 0,

        pub fn init() Self {
            return .{};
        }

        fn drainW(self: *Self) void {
            if (self.w_pending != 0) {
                self.inflight = 1;
                self.w_pending = 0;
            }
        }

        fn drainF(self: *Self) void {
            if (self.f_pending != 0) {
                self.frozen = 1;
                self.f_pending = 0;
            }
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                W => self.w_pc == 5,
                F => self.f_pc == 4,
                else => unreachable,
            };
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                W => switch (self.w_pc) {
                    0 => {
                        // inflight += 1 (buffered).
                        self.w_pending = 1;
                        self.w_pc = 1;
                    },
                    1 => {
                        // The SeqCst point: drains the buffer before the cross-word load.
                        if (weakening != .writer_fence) self.drainW();
                        self.w_pc = 2;
                    },
                    2 => {
                        // load state: frozen ⇒ back out.
                        if (self.frozen != 0) {
                            self.drainW();
                            self.inflight = 0;
                            self.backed_out = 1;
                            self.w_pc = 5;
                        } else {
                            self.w_pc = 3;
                        }
                    },
                    3 => {
                        // The DMA lands (an RMW/syscall drains the buffer).
                        self.drainW();
                        self.stored = 1;
                        self.w_pc = 4;
                    },
                    4 => {
                        // complete_store: covered, then inflight −= 1.
                        self.covered = 1;
                        self.inflight = 0;
                        self.w_pc = 5;
                    },
                    else => unreachable,
                },
                F => switch (self.f_pc) {
                    0 => {
                        // state = Frozen (buffered).
                        self.f_pending = 1;
                        self.f_pc = 1;
                    },
                    1 => {
                        if (weakening != .freezer_fence) self.drainF();
                        self.f_pc = 2;
                    },
                    2 => {
                        // inflight_empty: load inflight; 0 ⇒ drain observes coverage; else wait (re-load later).
                        if (self.inflight == 0 and self.w_pending == 0) {
                            self.observed_empty = 1;
                            self.covered_at_drain = self.covered;
                            self.f_pc = 3;
                        } else if (self.inflight == 0 and self.w_pending != 0) {
                            // The buffered increment is invisible: the freezer reads 0.
                            self.observed_empty = 1;
                            self.covered_at_drain = self.covered;
                            self.f_pc = 3;
                        }
                        // inflight == 1 ⇒ stay at pc 2 (the Notify wait).
                    },
                    3 => {
                        // A late landing of the buffered freeze.
                        self.drainF();
                        self.f_pc = 4;
                    },
                    else => unreachable,
                },
                else => unreachable,
            }
        }

        pub fn invariant(_: *const Self) !void {}

        /// If the freezer observed an empty in-flight set, every store that
        /// did not back out had its coverage visible to the drain.
        pub fn final(self: *const Self) !void {
            if (self.observed_empty == 1 and self.backed_out == 0 and self.stored != self.covered_at_drain) return error.StoreInvisibleToDrain;
            if (self.observed_empty == 0 and self.f_pc == 4) return error.FreezerNeverObserved;
        }
    };
}

// ---------------------------------------------------------------------------
// Model 2: overlay_validated_serve_is_never_torn.
// ---------------------------------------------------------------------------

pub const ServeWeakening = enum { none, no_revalidate };

/// The "device" is two words (page A, page B); a writer runs two passes
/// (bytes 1 then 2) over both pages under a claim; a reader snapshots the
/// generation, waits out the claim, reads A then B, revalidates.
pub fn ValidatedServeModel(comptime weakening: ServeWeakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const W = 0;
        const R = 1;

        a: u8 = 0,
        b: u8 = 0,
        claim: u8 = 0,
        gen: u8 = 0,
        w_pass: u8 = 1,
        w_pc: u8 = 0,
        r_pc: u8 = 0,
        r_snap: u8 = 0,
        r_a: u8 = 0,
        r_b: u8 = 0,
        served: u8 = 0,
        served_a: u8 = 0,
        served_b: u8 = 0,

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                W => self.w_pc == 4 and self.w_pass == 3,
                R => self.r_pc == 5,
                else => unreachable,
            };
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                W => switch (self.w_pc) {
                    0 => {
                        // begin_store: claim + generation.
                        self.claim = 1;
                        self.gen += 1;
                        self.w_pc = 1;
                    },
                    1 => {
                        self.a = self.w_pass;
                        self.w_pc = 2;
                    },
                    2 => {
                        self.b = self.w_pass;
                        self.w_pc = 3;
                    },
                    3 => {
                        // CQE: release the claim; next pass or done.
                        self.claim = 0;
                        self.w_pass += 1;
                        self.w_pc = if (self.w_pass <= 2) 0 else 4;
                    },
                    else => unreachable,
                },
                R => switch (self.r_pc) {
                    0 => {
                        self.r_snap = self.gen;
                        self.r_pc = 1;
                    },
                    1 => {
                        // await pages: no claim.
                        if (self.claim == 0) self.r_pc = 2;
                    },
                    2 => {
                        self.r_a = self.a;
                        self.r_pc = 3;
                    },
                    3 => {
                        self.r_b = self.b;
                        self.r_pc = 4;
                    },
                    4 => {
                        // REVALIDATE: generation equality ∧ no claim ⇒ serve; else loop.
                        const valid = switch (weakening) {
                            .none => self.gen == self.r_snap and self.claim == 0,
                            .no_revalidate => true,
                        };
                        if (valid and (self.r_a != 0 or self.r_b != 0)) {
                            self.served = 1;
                            self.served_a = self.r_a;
                            self.served_b = self.r_b;
                            self.r_pc = 5;
                        } else if (valid) {
                            // Nothing written yet: not an overlay serve.
                            self.r_pc = 5;
                        } else {
                            self.r_pc = 0;
                        }
                    },
                    else => unreachable,
                },
                else => unreachable,
            }
        }

        /// A served window is never a mix of two passes' bytes.
        pub fn invariant(self: *const Self) !void {
            if (self.served == 1 and self.served_a != self.served_b) return error.TornServe;
        }

        pub fn final(_: *const Self) !void {}

        pub fn key(self: *const Self) u64 {
            return std.hash.XxHash3.hash(0, std.mem.asBytes(self));
        }
    };
}

// ---------------------------------------------------------------------------
// Model 3: overlapping_inflight_stores_are_unrepresentable (§3.3).
// ---------------------------------------------------------------------------

pub const ClaimWeakening = enum { none, no_claims };

/// Two stores over ONE range: A (older) and B (newer). With claims B cannot
/// begin while A is in flight; without, B may land first and A last.
pub fn OverlapModel(comptime weakening: ClaimWeakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;

        device: u8 = 0,
        claim: u8 = 0,
        gen: u8 = 0,
        covered: u8 = 0,
        /// The generation each store was accepted at (0 = not yet).
        accepted: [2]u8 = @splat(0),
        pc: [2]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == 3;
        }

        pub fn step(self: *Self, t: usize) void {
            switch (self.pc[t]) {
                0 => {
                    // begin_store: the claim refuses an in-flight overlap.
                    if (weakening == .none and self.claim != 0) return; // wait (settle, retry)
                    self.claim = 1;
                    self.gen += 1;
                    self.accepted[t] = self.gen;
                    self.pc[t] = 1;
                },
                1 => {
                    // The DMA lands: the device holds THIS store's bytes.
                    self.device = @intCast(t + 1);
                    self.pc[t] = 2;
                },
                2 => {
                    // CQE: coverage joins; the claim releases.
                    self.covered = 1;
                    self.claim = 0;
                    self.pc[t] = 3;
                },
                else => unreachable,
            }
        }

        pub fn invariant(_: *const Self) !void {}

        /// The device holds the bytes of the store accepted LAST (the
        /// generation and the coverage say so).
        pub fn final(self: *const Self) !void {
            const newest: u8 = if (self.accepted[0] > self.accepted[1]) 1 else 2;
            if (self.device != newest) return error.OlderBytesUnderNewerGeneration;
        }
    };
}

test "overlay_freeze_drain_observes_final_coverage — holds; weakening EITHER SeqCst point admits a store invisible to the drain" {
    try explorer.expectHolds(FreezeDrainModel(.none), .{});
    try explorer.expectFails(FreezeDrainModel(.writer_fence), .{}, error.FinalViolated);
    try explorer.expectFails(FreezeDrainModel(.freezer_fence), .{}, error.FinalViolated);
}

test "overlay_validated_serve_is_never_torn — holds; skipping the generation revalidation serves a mix of two passes" {
    try explorer.expectHolds(ValidatedServeModel(.none), .{});
    try explorer.expectFails(ValidatedServeModel(.no_revalidate), .{}, error.InvariantViolated);
}

test "overlapping_inflight_stores_are_unrepresentable — holds under the claims; without them the §3.3 counterexample (older bytes under a newer generation) is reachable" {
    try explorer.expectHolds(OverlapModel(.none), .{});
    try explorer.expectFails(OverlapModel(.no_claims), .{}, error.FinalViolated);
}

test "state counts: small enough for every gate" {
    var report: explorer.Report = undefined;
    try explorer.explore(FreezeDrainModel(.none), std.testing.allocator, .{}, &report);
    try std.testing.expect(report.states > 10 and report.states < 500);
    try explorer.explore(ValidatedServeModel(.none), std.testing.allocator, .{}, &report);
    try std.testing.expect(report.states > 10 and report.states < 2000);
}
