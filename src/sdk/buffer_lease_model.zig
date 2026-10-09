//! Exhaustive model of the buffer-lease word (`buffer_lease.zig`) for
//! `model/explorer.zig` (charter §9 "model-check every lock-free core").
//! Five threads over ONE packed word `{state, inflight}` and the custody
//! facts the invariant needs:
//!
//!   S  the submitter: lease.submit (CAS inflight+1), then publish to the
//!      daemon — or undo (complete) if the session poisoned in between;
//!   D  the daemon: completes the published op, or NEVER if the session
//!      poisoned (the "arbitrarily late" completion, modelled as its worst
//!      case);
//!   R  the reaper: on the daemon's completion, lease.complete (CAS
//!      inflight−1);
//!   F  the freer: one `sqz_buf_free` at an arbitrary time;
//!   P  the poisoner: flags the session, then poisons the lease.
//!
//! Invariant: a RECYCLED range (`free` returned the range to the allocator)
//! is never one the daemon may still serve (published and not completed).
//! WEAKENING: the freer's one CAS split into `load; if inflight == 0:
//! store(FREED)` — the submitter's CAS lands between the two and publishes a
//! range the freer then recycles; `expectFails`.

const std = @import("std");
const explorer = @import("../model/explorer.zig");

const APP_OWNED: u8 = 0;
const FREED: u8 = 1;
const POISONED: u8 = 2;

const W = struct {
    state: u8,
    inflight: u8,

    fn eql(a: W, b: W) bool {
        return a.state == b.state and a.inflight == b.inflight;
    }
};

pub fn Lease(comptime split_free: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 5;
        const S = 0;
        const D = 1;
        const R = 2;
        const F = 3;
        const P = 4;

        word: W = .{ .state = APP_OWNED, .inflight = 0 },
        /// The descriptor reached the daemon (custody began).
        published: bool = false,
        /// The daemon completed (custody ended).
        daemon_done: bool = false,
        session_poisoned: bool = false,
        /// `free` returned the range to the allocator.
        recycled: bool = false,
        quarantined: bool = false,
        pc: [5]u8 = @splat(0),
        /// Each thread's last loaded word (its CAS expectation).
        tmp: [5]W = @splat(.{ .state = 0, .inflight = 0 }),

        pub fn init() Self {
            return .{};
        }

        /// Explicit key: no padding bytes reach the hash.
        pub fn key(self: *const Self) u64 {
            var h = std.hash.Wyhash.init(0);
            h.update(&[_]u8{ self.word.state, self.word.inflight, @intFromBool(self.published), @intFromBool(self.daemon_done), @intFromBool(self.session_poisoned), @intFromBool(self.recycled), @intFromBool(self.quarantined) });
            h.update(&self.pc);
            for (self.tmp) |t| h.update(&[_]u8{ t.state, t.inflight });
            return h.final();
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == 9;
        }

        fn cas(self: *Self, t: usize, next: W) bool {
            if (!self.word.eql(self.tmp[t])) return false;
            self.word = next;
            return true;
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                S => self.stepSubmitter(),
                D => self.stepDaemon(),
                R => self.stepReaper(),
                F => self.stepFreer(),
                P => self.stepPoisoner(),
                else => unreachable,
            }
        }

        fn stepSubmitter(self: *Self) void {
            switch (self.pc[S]) {
                0 => {
                    self.tmp[S] = self.word;
                    self.pc[S] = 1;
                },
                1 => {
                    // lease.submit: FREED/POISONED refuse; else CAS inflight+1.
                    if (self.tmp[S].state != APP_OWNED) {
                        self.pc[S] = 9;
                    } else if (self.cas(S, .{ .state = APP_OWNED, .inflight = self.tmp[S].inflight + 1 })) {
                        self.pc[S] = 2;
                    } else self.pc[S] = 0;
                },
                2 => {
                    // The session check at claim: poisoned ⇒ undo, else publish.
                    if (self.session_poisoned) {
                        self.pc[S] = 3;
                    } else {
                        self.published = true;
                        self.pc[S] = 9;
                    }
                },
                3 => {
                    self.tmp[S] = self.word;
                    self.pc[S] = 4;
                },
                4 => {
                    // lease.complete (the undo): decrement only while APP_OWNED.
                    if (self.tmp[S].state != APP_OWNED) {
                        self.pc[S] = 9;
                    } else if (self.cas(S, .{ .state = APP_OWNED, .inflight = self.tmp[S].inflight - 1 })) {
                        self.pc[S] = 9;
                    } else self.pc[S] = 3;
                },
                else => unreachable,
            }
        }

        fn stepDaemon(self: *Self) void {
            // A poisoned session's daemon never completes (worst case); a
            // live one completes what was published; nothing published and
            // the submitter finished ⇒ nothing to serve.
            if (self.session_poisoned) {
                self.pc[D] = 9;
            } else if (self.published) {
                self.daemon_done = true;
                self.pc[D] = 9;
            } else if (self.pc[S] == 9) {
                self.pc[D] = 9;
            }
        }

        fn stepReaper(self: *Self) void {
            switch (self.pc[R]) {
                0 => {
                    if (self.daemon_done) {
                        self.pc[R] = 1;
                    } else if (self.pc[D] == 9) {
                        // No CQE will ever come (poisoned, or nothing published).
                        self.pc[R] = 9;
                    }
                },
                1 => {
                    self.tmp[R] = self.word;
                    self.pc[R] = 2;
                },
                2 => {
                    if (self.tmp[R].state != APP_OWNED) {
                        self.pc[R] = 9;
                    } else if (self.cas(R, .{ .state = APP_OWNED, .inflight = self.tmp[R].inflight - 1 })) {
                        self.pc[R] = 9;
                    } else self.pc[R] = 1;
                },
                else => unreachable,
            }
        }

        fn stepFreer(self: *Self) void {
            switch (self.pc[F]) {
                0 => {
                    self.tmp[F] = self.word;
                    self.pc[F] = 1;
                },
                1 => switch (self.tmp[F].state) {
                    FREED => self.pc[F] = 9,
                    POISONED => if (self.cas(F, .{ .state = FREED, .inflight = 0 })) {
                        self.quarantined = true;
                        self.pc[F] = 9;
                    } else {
                        self.pc[F] = 0;
                    },
                    APP_OWNED => {
                        if (self.tmp[F].inflight != 0) {
                            // -EBUSY, never deferred.
                            self.pc[F] = 9;
                        } else if (split_free) {
                            // THE WEAKENING: a plain store after the load.
                            self.word = .{ .state = FREED, .inflight = 0 };
                            self.recycled = true;
                            self.pc[F] = 9;
                        } else if (self.cas(F, .{ .state = FREED, .inflight = 0 })) {
                            self.recycled = true;
                            self.pc[F] = 9;
                        } else self.pc[F] = 0;
                    },
                    else => unreachable,
                },
                else => unreachable,
            }
        }

        fn stepPoisoner(self: *Self) void {
            switch (self.pc[P]) {
                0 => {
                    self.session_poisoned = true;
                    self.pc[P] = 1;
                },
                1 => {
                    self.tmp[P] = self.word;
                    self.pc[P] = 2;
                },
                2 => {
                    if (self.tmp[P].state == APP_OWNED and self.tmp[P].inflight != 0) {
                        if (self.cas(P, .{ .state = POISONED, .inflight = self.tmp[P].inflight })) {
                            self.pc[P] = 9;
                        } else self.pc[P] = 1;
                    } else self.pc[P] = 9;
                },
                else => unreachable,
            }
        }

        pub fn invariant(self: *const Self) !void {
            // A recycled range the daemon may still serve.
            if (self.recycled and self.published and !self.daemon_done) return error.FreedRangeUnderDaemonCustody;
            // Never both outcomes.
            if (self.recycled and self.quarantined) return error.RecycledAndQuarantined;
        }

        pub fn final(self: *const Self) !void {
            // Whatever the freer did, the lease word agrees with it.
            if (self.recycled and self.word.state != FREED) return error.RecycledButNotFreed;
            if (self.quarantined and self.word.state != FREED) return error.QuarantinedButNotFreed;
            // A range the daemon never released is never recycled.
            if (self.published and !self.daemon_done and self.recycled) return error.FreedRangeUnderDaemonCustody;
        }
    };
}

const testing = std.testing;

test "buffer lease model holds: a recycled range is never under daemon custody, in every interleaving of submitter / daemon / reaper / freer / poisoner" {
    try explorer.expectHolds(Lease(false), .{});
}

test "buffer lease weakening fails: the freer's load-then-store admits a freed-while-submitted range" {
    try explorer.expectFails(Lease(true), .{}, error.InvariantViolated);
}
