//! The MODEL of the manager election and the per-host fence (decision
//! 0003 §DF; charter §5.5 "Fencing"): three hosts A, B, C on one metadata
//! namespace; the writer claim's successors arbitrating through NVMe
//! Persistent Reservations with nvmet 7.3's semantics; a frozen manager and
//! its host's registry; the stale-sibling rule; a co-located joiner's
//! departure. Dependency-free: one atomic action per `step`, explored
//! exhaustively by `src/model/explorer.zig`.
//!
//! Threads:
//!   0 M0  host A's manager, frozen (SIGSTOP) and superseded: its ONE
//!         action is a stale write, possible at any point of the schedule
//!         (the resume). Bound at the start.
//!   1 P1  a successor on host B: register B (enrolled at once under the
//!         registry's lock), arbitrate, bind (refused once its host is not
//!         admitted).
//!   2 P2  a successor on host C: the same; then the death preempt of a
//!         member key it does not hold, with nvmet's holder semantics.
//!   3 S   a fresh mount on host A that re-registers host A (when it is
//!         unregistered): the stale-sibling rule first.
//!   4 J   a joiner on host B departing: it may unregister host B.
//!
//! Device semantics (nvmet 7.3): one registration per host; a writer is
//! admitted iff no reservation stands or its host is registered (WERO: the
//! registrants write); a non-holder's acquire of a held reservation
//! conflicts; a non-holder's preempt of the holder's key takes the
//! reservation and unregisters the holder's host (spec-conformant); a
//! HOLDER's preempt of another key is a no-op; unregistering the holder
//! releases the reservation.
//!
//! Invariants: (1) at most one bound, live manager whose host is admitted;
//! (2) a fenced incarnation (its host's registration preempted while it was
//! superseded) never writes after its fence; (3) no live bound manager while
//! the namespace stands unreserved (every host — fenced ones included — could
//! write).
//!
//! The WEAKENINGS (each must fail): `.wear_registered_holds` (rtype 5, a
//! registered host's conflicting acquire taken as held — review round 3 N1:
//! two managers, invariant 1); `rule = false` (no stale-sibling rule: the
//! superseded manager re-admitted, invariants 1 and 2); `retire_term =
//! false` (the B1 mark without the term spares the superseded incarnation:
//! invariants 1 and 2); `depart_checks_holder = false` with `early_enrol =
//! false` (the departure as before B3: invariant 3); `release_cycle = true`
//! (the first §DF.11 death preempt: release → preempt → re-acquire,
//! invariant 3).

const std = @import("std");

pub const Hold = enum { wero, wear_registered_holds };

pub const Config = struct {
    hold: Hold = .wero,
    rule: bool = true,
    retire_term: bool = true,
    depart_checks_holder: bool = true,
    early_enrol: bool = true,
    release_cycle: bool = false,
};

const A: u8 = 0;
const B: u8 = 1;
const C: u8 = 2;
const NONE: u8 = 3;
const HOST = [5]u8{ A, B, C, A, B };

pub fn Model(comptime cfg: Config) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 5;

        /// Per-host registration (host A's is M0's at the start).
        reg: [3]bool = .{ true, false, false },
        /// The reservation's holder (host), NONE = unreserved.
        holder: u8 = A,
        pc: [5]u8 = @splat(0),
        alive: [5]bool = @splat(true),
        /// Managers 0..2 (M0, P1, P2) bound to the role.
        bound: [3]bool = .{ true, false, false },
        /// Manager i's host was preempted while it was superseded.
        fenced: [3]bool = @splat(false),
        /// P1 entered host B's registry.
        p1_enrolled: bool = false,
        wrote_after_fence: bool = false,

        pub fn init() Self {
            return .{};
        }

        pub fn key(self: *const Self) u64 {
            var h = std.hash.Wyhash.init(0);
            h.update(std.mem.asBytes(&self.reg));
            h.update(&[_]u8{self.holder});
            h.update(&self.pc);
            h.update(std.mem.asBytes(&self.alive));
            h.update(std.mem.asBytes(&self.bound));
            h.update(std.mem.asBytes(&self.fenced));
            h.update(&[_]u8{ @intFromBool(self.p1_enrolled), @intFromBool(self.wrote_after_fence) });
            return h.final();
        }

        fn admitted(self: *const Self, host: u8) bool {
            return self.holder == NONE or self.reg[host];
        }

        /// Host `victim`'s registration preempted: its superseded managers
        /// are fenced; the reservation passes to `to`.
        fn preemptHost(self: *Self, victim: u8, to: u8) void {
            self.reg[victim] = false;
            for (0..3) |m| if (HOST[m] == victim and self.bound[m]) {
                self.fenced[m] = true;
            };
            self.holder = to;
        }

        fn unregister(self: *Self, host: u8) void {
            self.reg[host] = false;
            if (self.holder == host) self.holder = NONE;
        }

        pub fn done(self: *const Self, t: usize) bool {
            const last: u8 = switch (t) {
                0 => 1,
                1 => 3,
                2 => if (cfg.release_cycle) 6 else 4,
                3, 4 => 1,
                else => unreachable,
            };
            return self.pc[t] >= last or !self.alive[t];
        }

        pub fn step(self: *Self, t: usize) void {
            const h = HOST[t];
            switch (t) {
                // M0 resumes: one stale write.
                0 => {
                    if (self.admitted(A) and self.fenced[0]) self.wrote_after_fence = true;
                    self.pc[0] = 1;
                },
                1, 2 => self.successor(t, h),
                3 => {
                    if (!self.reg[A]) {
                        // The rule: M0 is stale once the role passed (the
                        // holder is another host's); the B1 mark without the
                        // term spares it.
                        const stale = self.holder != A and self.holder != NONE;
                        if (cfg.rule and stale and cfg.retire_term) self.alive[0] = false;
                        self.reg[A] = true;
                    }
                    self.pc[3] = 1;
                },
                4 => {
                    const siblings = self.alive[1] and self.p1_enrolled;
                    const holds = self.holder == B;
                    const leave = !siblings and (!cfg.depart_checks_holder or !holds);
                    if (leave and self.reg[B]) self.unregister(B);
                    self.pc[4] = 1;
                },
                else => unreachable,
            }
        }

        fn successor(self: *Self, t: usize, h: u8) void {
            switch (self.pc[t]) {
                // Register the host before the first write; enrol (P1).
                0 => {
                    self.reg[h] = true;
                    if (t == 1 and cfg.early_enrol) self.p1_enrolled = true;
                },
                // Arbitrate.
                1 => {
                    if (self.holder == NONE) {
                        self.holder = h;
                    } else if (self.holder != h) switch (cfg.hold) {
                        // A non-holder preempts the holder's key: spec arm.
                        .wero => if (self.reg[h]) self.preemptHost(self.holder, h),
                        // rtype 5: the conflicting acquire taken as held; the
                        // stale claim's key (host A) preempted if it stands.
                        .wear_registered_holds => if (self.reg[A] and h != A) {
                            self.reg[A] = false;
                            self.fenced[0] = self.bound[0];
                        },
                    };
                },
                // Bind: refused when the host is not admitted.
                2 => {
                    if (self.admitted(h)) self.bound[t] = true else self.alive[t] = false;
                    if (t == 1 and !cfg.early_enrol) self.p1_enrolled = true;
                },
                // P2's death preempt of host B's key (a dead member there).
                3 => {
                    if (cfg.release_cycle) {
                        if (self.holder == h) self.holder = NONE;
                    } else {
                        // nvmet: a HOLDER's preempt of another key is a
                        // no-op (detected by the report: `.failed`).
                    }
                },
                4 => if (self.holder == NONE and self.reg[B]) self.unregister(B),
                5 => if (self.holder == NONE and self.reg[h]) {
                    self.holder = h;
                },
                else => unreachable,
            }
            self.pc[t] += 1;
        }

        pub fn invariant(self: *const Self) !void {
            if (self.holder == NONE) for (0..3) |m| {
                if (self.bound[m] and self.alive[m]) return error.UnreservedUnderManager;
            };
            var managers: u8 = 0;
            for (0..3) |m| {
                if (self.bound[m] and self.alive[m] and self.admitted(HOST[m])) managers += 1;
            }
            if (managers > 1) return error.TwoManagers;
            if (self.wrote_after_fence) return error.FencedWrote;
        }

        pub fn final(_: *const Self) !void {}
    };
}

// ---------------------------------------------------------------------------
// The checks.
// ---------------------------------------------------------------------------

const explorer = @import("../../model/explorer.zig");
const limits: explorer.Limits = .{};

test "model: the chosen design (WERO, the stale-sibling rule with term-keyed retirements, enrolment under the register's lock, a departure that never drops a hold, nvmet's no-op holder preempt) holds — one manager, no fenced write, never unreserved under a manager" {
    try explorer.expectHolds(Model(.{}), limits);
    var report: explorer.Report = undefined;
    try explorer.explore(Model(.{}), testing.allocator, limits, &report);
    std.debug.print("\nhost fence model: {d} states explored, depth {d}\n", .{ report.states, report.max_depth_seen });
}

test "model weakening: rtype 5 with a registered host's conflicting acquire taken as held — two successors both bind (review round 3 N1)" {
    try explorer.expectFails(Model(.{ .hold = .wear_registered_holds }), limits, error.InvariantViolated);
}

test "model weakening: no stale-sibling rule — a re-registration of the superseded manager's host re-admits it" {
    try explorer.expectFails(Model(.{ .rule = false }), limits, error.InvariantViolated);
}

test "model weakening: the retirement mark without the page term spares the superseded incarnation (review B1)" {
    try explorer.expectFails(Model(.{ .retire_term = false }), limits, error.InvariantViolated);
}

test "model weakening: the departure that unregisters the host when no OTHER entry stands, holder or not, the manager enrolled only at its bind (review B3) — the namespace unreserved under a live manager" {
    try explorer.expectFails(Model(.{ .depart_checks_holder = false, .early_enrol = false }), limits, error.InvariantViolated);
}

test "model weakening: the release → preempt → re-acquire death preempt (the first §DF.11 wiring) — unreserved under a live manager" {
    try explorer.expectFails(Model(.{ .release_cycle = true }), limits, error.InvariantViolated);
}

test "model: which invariant each weakening breaks" {
    const Probe = struct {
        /// The invariant the failing schedule breaks first (null = holds).
        fn firstError(comptime M: type) ?anyerror {
            var report: explorer.Report = undefined;
            explorer.explore(M, testing.allocator, limits, &report) catch {
                defer testing.allocator.free(report.schedule);
                var m = M.init();
                for (report.schedule) |t| {
                    m.step(t);
                    m.invariant() catch |e| return e;
                }
                return error.NoInvariantNamed;
            };
            return null;
        }
    };
    try testing.expectEqual(@as(?anyerror, error.TwoManagers), Probe.firstError(Model(.{ .hold = .wear_registered_holds })));
    const no_rule = Probe.firstError(Model(.{ .rule = false })).?;
    try testing.expect(no_rule == error.TwoManagers or no_rule == error.FencedWrote);
    const no_term = Probe.firstError(Model(.{ .retire_term = false })).?;
    try testing.expect(no_term == error.TwoManagers or no_term == error.FencedWrote);
    try testing.expectEqual(@as(?anyerror, error.UnreservedUnderManager), Probe.firstError(Model(.{ .depart_checks_holder = false, .early_enrol = false })));
    try testing.expectEqual(@as(?anyerror, error.UnreservedUnderManager), Probe.firstError(Model(.{ .release_cycle = true })));
    try testing.expectEqual(@as(?anyerror, null), Probe.firstError(Model(.{})));
}

const testing = std.testing;
