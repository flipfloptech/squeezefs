//! The slot-move epoch's park (`moves.zig`, decision 0003 §X.1) as an
//! exhaustive model, with the two weakenings that lose the wake.
//!
//! Shared words: `moved` (the lease table's transition — the op's next
//! attempt succeeds once it is set), `epoch` (the mount's move epoch),
//! `registered`/`woken` (the waiter's `sync.Notify` node; `notifyWaiters`
//! wakes a REGISTERED waiter only — the Notify's own law, proven in
//! `sync/notify_model.zig`). Each step is one atomic action.
//!
//!   MOVER  = mutate (moved) → bump (epoch += 1) → notify (registered ⇒ woken)
//!   CALLER = observe (seen = epoch) → attempt (moved ⇒ done)
//!            → register → recheck (epoch ≠ seen ⇒ re-attempt)
//!            → park (woken ⇒ re-attempt)
//!
//! The park can last forever only if nothing will ever wake it, so the
//! park step is enabled once the mover finished (every wake it could send
//! has been sent); a caller parked unwoken then is a LOST WAKE. Weakenings:
//! (a) `recheck_before_register` — the epoch read before the registration
//! (the transition and its notify land between the two: the caller
//! registers after the only wake); (b) `observe_after_attempt` — the epoch
//! sampled after the failed attempt (the whole transition lands between
//! the attempt and the sample: the recheck sees no change, the notify
//! found nobody registered).

const std = @import("std");
const explorer = @import("../../../model/explorer.zig");

pub const Weakening = enum { none, recheck_before_register, observe_after_attempt };

pub fn MoveWait(comptime weak: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const MOVER: usize = 0;
        const CALLER: usize = 1;
        /// The caller's terminal pcs.
        const DONE: u8 = 10;
        const LOST: u8 = 11;

        moved: bool = false,
        epoch: u8 = 0,
        registered: bool = false,
        woken: bool = false,
        seen: u8 = 0,
        mover_pc: u8 = 0,
        caller_pc: u8 = 0,
        /// A re-attempt that still found the slot unmoved (must not happen:
        /// every wake and every epoch change follows the transition).
        stale_reattempt: bool = false,

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return if (t == MOVER) self.mover_pc == 3 else self.caller_pc >= DONE;
        }

        fn reattempt(self: *Self) void {
            if (!self.moved) self.stale_reattempt = true;
            self.caller_pc = DONE;
        }

        pub fn step(self: *Self, t: usize) void {
            if (t == MOVER) {
                switch (self.mover_pc) {
                    0 => self.moved = true,
                    1 => self.epoch += 1,
                    2 => if (self.registered) {
                        self.woken = true;
                    },
                    else => unreachable,
                }
                self.mover_pc += 1;
                return;
            }
            switch (weak) {
                .none => switch (self.caller_pc) {
                    0 => self.seen = self.epoch,
                    1 => {
                        if (self.moved) self.caller_pc = DONE - 1;
                    },
                    2 => self.registered = true,
                    3 => if (self.epoch != self.seen) return self.reattempt(),
                    4 => {
                        if (self.mover_pc != 3) return;
                        if (self.woken) return self.reattempt();
                        self.caller_pc = LOST;
                        return;
                    },
                    else => unreachable,
                },
                .recheck_before_register => switch (self.caller_pc) {
                    0 => self.seen = self.epoch,
                    1 => {
                        if (self.moved) self.caller_pc = DONE - 1;
                    },
                    2 => if (self.epoch != self.seen) return self.reattempt(),
                    3 => self.registered = true,
                    4 => {
                        if (self.mover_pc != 3) return;
                        if (self.woken) return self.reattempt();
                        self.caller_pc = LOST;
                        return;
                    },
                    else => unreachable,
                },
                .observe_after_attempt => switch (self.caller_pc) {
                    0 => {
                        if (self.moved) self.caller_pc = DONE - 1;
                    },
                    1 => self.seen = self.epoch,
                    2 => self.registered = true,
                    3 => if (self.epoch != self.seen) return self.reattempt(),
                    4 => {
                        if (self.mover_pc != 3) return;
                        if (self.woken) return self.reattempt();
                        self.caller_pc = LOST;
                        return;
                    },
                    else => unreachable,
                },
            }
            self.caller_pc += 1;
        }

        pub fn invariant(self: *const Self) !void {
            if (self.stale_reattempt) return error.ReattemptBeforeTheMove;
        }

        pub fn final(self: *const Self) !void {
            if (self.caller_pc == LOST) return error.LostWake;
        }
    };
}

test "the move park: observe → attempt → register → recheck → park never loses the transition's wake; the recheck before the registration, or the epoch sampled after the failed attempt, parks the caller past the only wake" {
    try explorer.expectHolds(MoveWait(.none), .{});
    try explorer.expectFails(MoveWait(.recheck_before_register), .{}, error.FinalViolated);
    try explorer.expectFails(MoveWait(.observe_after_attempt), .{}, error.FinalViolated);
}
