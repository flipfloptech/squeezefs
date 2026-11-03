//! The MODELS of the manager election and the per-host fence (decision
//! 0003 §DF; charter §5.5 "Fencing"), written against the code's inputs:
//! the stale-sibling rule judges a sibling by its registry lines (`p`/`m`
//! a manager, `j` a joiner, `l` a joiner leaving, `r` a retirement with
//! its term), the reservation report and the appender pages, as
//! `host_fence.judge` does; it reads, registers and re-judges as three steps
//! (review B2). Dependency-free: one atomic action per `step`, explored
//! exhaustively by `src/model/explorer.zig`.
//!
//! `Election` — hosts A, B, C on one metadata namespace, WERO:
//!   0 M0  host A's manager, frozen past its claim's TTL: a write, its
//!         heartbeat, a write — each at any point of the schedule (the
//!         resume). Its `m` line stands; appender 0's page names it.
//!   1 P1  a successor on host B: the claim probed (no write), the host
//!         registered (`p` enrolled under the registry's lock), the hold
//!         (acquire, or the holder's key preempted), the claim probed again,
//!         the pre-gate writes (held tail, fold, cover), the gate's claim
//!         read, the bind (claim committed, appender 0's page, `m` enrolled,
//!         `p` retired); then its clean unmount (the claim deleted, the hold
//!         released when it is the namespace's last registrant).
//!   2 P2  a successor on host C: the same; then (`release_cycle` only) the
//!         first §DF.11 death preempt of a host-B member.
//!   3 SA  a joiner mounting on host A: the rule judges M0, registers host A,
//!         re-judges.
//!   4 JB  a joiner on host B: the rule judges P1, registers, re-judges;
//!         then departs (B3: it never unregisters a hold that is ours).
//!   5 L   a PTPL-less target power cycle: every registration and the hold
//!         forgotten.
//!
//! `Member` — the manager MG on host A holds; members X and Y on host B:
//!   0 X   a frozen member (re-joined in place once: its retirement `r`
//!         names appender 1 at term 1; its page is appender 1 at term 2):
//!         one write at any point.
//!   1 Y   a member leaving: its line marked `l`, its leave's write (the
//!         page Free), the line removed.
//!   2 MG  each member whose page is Live evicted in turn: the death
//!         preempt of host B's key, the page Recovering, the re-fence
//!         (§DF.12), the tail scan (the region recovered, reused from here).
//!   3 S   a joiner mounting on host B: the rule, the register, the re-judge.
//!   Device semantics: `spec` (a holder's preempt of another key unregisters
//!   it) or `nvmet` 7.3 (a holder's preempt of another key is a no-op).
//!
//! Invariants: `KilledCurrent` the rule never kills the current manager or a
//! Live member; `WroteOverLiveClaim` no open writes while another writer's
//! claim is fresh (review round 3 major 2); `WroteUnheld` a writer open
//! writes only under its own hold; `FencedWrote` a superseded manager never
//! writes; `TwoManagers` at most one bound manager admitted under a standing
//! reservation; `UnreservedUnderManager` never unreserved under the current
//! manager; `FencedMemberWrote` an evicted member never writes after its
//! region's tail scan.
//!
//! The ACCEPTED LIMITS (counted, not violations; `Strict` makes each one a
//! violation and a test shows it reachable): `lapse` — a PTPL-less target
//! power cycle forgets every fence until the manager's heartbeat re-acquires
//! (anything superseded and resumed meanwhile writes); `release_window` — a
//! successor's clean unmount as the namespace's last registrant leaves it
//! unreserved, and a superseded manager resumed then writes until its
//! heartbeat fail-stops it (review round 3 major 3 closes the re-acquire,
//! not this window); `register_window` — between a rule's register and its
//! re-judge, a sibling that turned stale meanwhile is admitted. And on
//! nvmet 7.3 the death preempt fences nothing: `Member(.{ .semantics =
//! .nvmet })` FAILS `FencedMemberWrote` (detection only; decision 0003 §DF).

const std = @import("std");

const A: u8 = 0;
const B: u8 = 1;
const C: u8 = 2;
const NONE: u8 = 3;
/// A host's registry lock, free.
const FREE: u8 = 0xff;

/// The accepted limits made violations.
pub const Strict = struct {
    lapse: bool = false,
    release_window: bool = false,
    register_window: bool = false,
};

pub const Hold = enum { wero, wear_registered_holds };

/// How the rule judges a manager's line: the report first, then appender
/// 0's page (the fix of review round 3 major 1); round 3's (`p` stale
/// whenever the rule runs, never retired; `m` by the page); the page alone.
pub const JudgeManager = enum { report_then_page, round3, page_only };

pub const ElectionConfig = struct {
    hold: Hold = .wero,
    rule: bool = true,
    judge: JudgeManager = .report_then_page,
    /// The claim probed before the registration (false: round 3 — register,
    /// pre-gate writes, the gate's claim read, the hold).
    claim_first: bool = true,
    /// The claim probed again once the hold stands.
    reprobe: bool = true,
    /// After a lapse the hold re-acquired only while the device's claim
    /// names this incarnation (review round 3 major 3).
    lapse_claim_check: bool = true,
    depart_checks_holder: bool = true,
    early_enrol: bool = true,
    release_cycle: bool = false,
    strict: Strict = .{},
};

const Line = enum(u8) { none, p, m, pm };

const PStep = enum(u8) { probe, register, hold, reprobe, write, gate, bind, unmount, death_release, death_preempt, death_reacquire };

const Order = struct {
    steps: [12]PStep = undefined,
    len: u8 = 0,

    fn add(o: *Order, s: PStep) void {
        o.steps[o.len] = s;
        o.len += 1;
    }
};

fn successorOrder(cfg: ElectionConfig, t: usize) Order {
    var o: Order = .{};
    if (cfg.claim_first) {
        o.add(.probe);
        o.add(.register);
        o.add(.hold);
        if (cfg.reprobe) o.add(.reprobe);
        o.add(.write);
        o.add(.gate);
    } else {
        o.add(.register);
        o.add(.write);
        o.add(.gate);
        o.add(.hold);
    }
    o.add(.bind);
    if (t == 1) o.add(.unmount);
    if (t == 2 and cfg.release_cycle) {
        o.add(.death_release);
        o.add(.death_preempt);
        o.add(.death_reacquire);
    }
    return o;
}

pub fn Election(comptime cfg: ElectionConfig) type {
    return extern struct {
        const Self = @This();
        pub const thread_count: usize = 6;
        const order = [3]Order{ .{}, successorOrder(cfg, 1), successorOrder(cfg, 2) };
        const HOST = [3]u8{ A, B, C };
        const SA_END = 3;
        const JB_DEPART = 3;
        const JB_END = 4;

        reg: [3]bool = .{ true, false, false },
        /// The holder's host (NONE: unreserved).
        holder: u8 = A,
        /// WEAR's conflicting acquire taken as held (the N1 weakening).
        wear_held: [3]bool = @splat(false),
        /// The claim's incarnation (NONE: deleted); M0's is stale.
        claim: u8 = 0,
        /// Appender 0's Live page names this manager (NONE: Free).
        page0: u8 = 0,
        pc: [6]u8 = @splat(0),
        alive: [3]bool = @splat(true),
        bound: [3]bool = .{ true, false, false },
        failed: [3]bool = @splat(false),
        preempted: [3]bool = @splat(false),
        line: [3]Line = .{ .m, .none, .none },
        jb_enrolled: bool = false,
        /// Each host's registry lock: the thread holding it (the rule holds
        /// it from its read to its re-judge; a register, a release, a
        /// departure waits for it).
        lock: [3]u8 = @splat(FREE),
        /// A rule's register stands, its re-judge pending.
        window: [3]bool = @splat(false),
        role_passed: bool = false,
        lapse_happened: bool = false,
        /// A clean unmount (or a refused open) released the hold.
        release_happened: bool = false,
        killed_current: bool = false,
        wrote_over_live: bool = false,
        wrote_unheld: bool = false,
        fenced_wrote: bool = false,
        hit_lapse: bool = false,
        hit_release: bool = false,
        hit_register: bool = false,

        pub fn init() Self {
            return .{};
        }

        fn admitted(self: *const Self, h: u8) bool {
            return self.holder == NONE or self.reg[h];
        }

        fn holds(self: *const Self, h: u8) bool {
            return self.holder == h or self.wear_held[h];
        }

        fn otherRegistered(self: *const Self, h: u8) bool {
            for (0..3) |x| if (x != h and self.reg[x]) return true;
            return false;
        }

        fn unregisterHost(self: *Self, h: u8) void {
            self.reg[h] = false;
            self.wear_held[h] = false;
            if (self.holder == h) self.holder = NONE;
        }

        /// Another writer's claim is fresh (M0's aged past the TTL).
        fn claimFreshForeign(self: *const Self, m: u8) bool {
            const c = self.claim;
            return c != NONE and c != m and c != 0 and self.alive[c] and self.bound[c] and !self.failed[c];
        }

        fn isCurrent(self: *const Self, i: u8) bool {
            return self.bound[i] and self.alive[i] and !self.failed[i] and self.claim == i and (self.holder == HOST[i] or self.holder == NONE);
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                0 => self.pc[0] >= 3 or !self.alive[0] or self.failed[0],
                1, 2 => self.pc[t] >= order[t].len or !self.alive[t] or self.failed[t],
                3 => self.pc[3] >= SA_END,
                4 => self.pc[4] >= JB_END,
                5 => self.pc[5] >= 1,
                else => unreachable,
            };
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                0 => {
                    if (self.pc[0] == 1) {
                        if (!self.m0Beat()) return;
                    } else self.m0Write();
                    self.pc[0] += 1;
                },
                1, 2 => self.successor(@intCast(t)),
                3 => self.ruleRunner(3, A, SA_END),
                4 => if (self.pc[4] == JB_DEPART) {
                    if (self.busy(B, 4)) return;
                    self.depart();
                    self.pc[4] += 1;
                } else self.ruleRunner(4, B, JB_DEPART),
                5 => {
                    self.reg = @splat(false);
                    self.wear_held = @splat(false);
                    self.holder = NONE;
                    self.lapse_happened = true;
                    self.pc[5] = 1;
                },
                else => unreachable,
            }
        }

        /// M0 resumed: a write. Refused by the device → the volume latched.
        fn m0Write(self: *Self) void {
            if (!self.admitted(A)) {
                self.failed[0] = true;
                return;
            }
            const superseded = self.role_passed or (self.holder != NONE and self.holder != A);
            if (!superseded) return;
            if (self.lapse_happened) {
                self.hit_lapse = true;
            } else if (self.holder == NONE) {
                self.hit_release = true;
            } else if (self.window[A]) {
                self.hit_register = true;
            } else self.fenced_wrote = true;
        }

        /// The heartbeat's PR re-check (`gate.prRecheck`). False = waiting
        /// for host A's registry lock (the re-register).
        fn m0Beat(self: *Self) bool {
            if (self.holder == A) return true;
            if (self.holder != NONE) {
                self.failed[0] = true; // usurped
                return true;
            }
            if (cfg.lapse_claim_check and self.claim != 0) {
                self.failed[0] = true; // the lapse: the device's claim is not ours
                return true;
            }
            if (self.busy(A, 0)) return false;
            self.reg[A] = true;
            self.holder = A;
            return true;
        }

        /// Host `h`'s registry lock is held by a thread other than `t`.
        fn busy(self: *const Self, h: u8, t: u8) bool {
            return self.lock[h] != FREE and self.lock[h] != t;
        }

        /// A writer open's write (pre-gate, bind, unmount). False = refused.
        fn writerWrite(self: *Self, m: u8, h: u8) bool {
            if (!self.admitted(h)) return false;
            const over_live = self.claimFreshForeign(m);
            if (self.holds(h) and !over_live) return true;
            if (self.lapse_happened) {
                self.hit_lapse = true;
                return true;
            }
            if (self.release_happened and self.holder == NONE) {
                self.hit_release = true;
                return true;
            }
            if (over_live) self.wrote_over_live = true;
            if (!self.holds(h) and self.holder != NONE) {
                if (self.window[h]) self.hit_register = true else self.wrote_unheld = true;
            }
            return true;
        }

        /// A refused open: its line out; the registration given back as
        /// `host_fence.leaveRegistration` says (a preempting hold kept).
        fn discard(self: *Self, m: u8, h: u8) void {
            self.alive[m] = false;
            self.line[m] = .none;
            if (!self.reg[h]) return;
            if (h == B and self.jb_enrolled) return;
            if (self.holder == h) {
                if (self.preempted[m] or self.otherRegistered(h)) return;
                self.release_happened = true;
            }
            self.unregisterHost(h);
        }

        /// The step touches its host's registry (a register, a release, a
        /// refusal's take-back).
        fn needsLock(self: *const Self, m: u8, s: PStep) bool {
            const h = HOST[m];
            return switch (s) {
                .register, .unmount => true,
                .hold => !self.reg[h],
                .reprobe, .gate => self.claimFreshForeign(m),
                .write, .bind => !self.admitted(h),
                else => false,
            };
        }

        fn successor(self: *Self, m: u8) void {
            const h = HOST[m];
            const s = order[m].steps[self.pc[m]];
            if (self.needsLock(m, s) and self.busy(h, m)) return;
            switch (s) {
                .probe => if (self.claimFreshForeign(m)) {
                    self.alive[m] = false;
                },
                .register => {
                    self.reg[h] = true;
                    if (cfg.early_enrol) self.line[m] = .p;
                },
                .hold => self.hold(m, h),
                .reprobe, .gate => if (self.claimFreshForeign(m)) self.discard(m, h),
                .write => if (!self.writerWrite(m, h)) self.discard(m, h),
                .bind => if (self.writerWrite(m, h)) {
                    self.claim = m;
                    self.page0 = m;
                    self.bound[m] = true;
                    self.role_passed = true;
                    self.line[m] = if (cfg.judge == .round3) .pm else .m;
                } else self.discard(m, h),
                .unmount => {
                    if (self.writerWrite(m, h)) {
                        if (self.claim == m) self.claim = NONE;
                        if (self.page0 == m) self.page0 = NONE;
                        if (self.reg[h] and !(h == B and self.jb_enrolled)) {
                            if (!self.holds(h)) {
                                self.unregisterHost(h);
                            } else if (!self.otherRegistered(h)) {
                                self.release_happened = true;
                                self.unregisterHost(h);
                            }
                        }
                    }
                    self.bound[m] = false;
                    self.line[m] = .none;
                    self.alive[m] = false;
                },
                .death_release => if (self.holder == h) {
                    self.holder = NONE;
                },
                .death_preempt => if (self.holder == NONE and self.reg[B]) self.unregisterHost(B),
                .death_reacquire => if (self.holder == NONE and self.reg[h]) {
                    self.holder = h;
                },
            }
            self.pc[m] += 1;
        }

        fn hold(self: *Self, m: u8, h: u8) void {
            if (!self.reg[h]) return self.discard(m, h);
            if (self.holder == NONE) {
                self.holder = h;
            } else if (self.holder != h) switch (cfg.hold) {
                // A non-holder's preempt of the holder's key: the hold
                // passes, the holder's host unregistered.
                .wero => {
                    self.reg[self.holder] = false;
                    self.holder = h;
                    self.preempted[m] = true;
                },
                // rtype 5: the conflicting acquire taken as held; the stale
                // claim's key (host A) preempted while it holds.
                .wear_registered_holds => if (self.holder == A) {
                    self.reg[A] = false;
                    self.holder = h;
                    self.preempted[m] = true;
                } else {
                    self.wear_held[h] = true;
                },
            };
        }

        /// The manager line of incarnation `i` judged stale (`judgeRegion`).
        fn managerStale(self: *const Self, i: u8) bool {
            const ln = self.line[i];
            if (ln == .none) return false;
            return switch (cfg.judge) {
                .report_then_page => (self.holder != NONE and self.holder != HOST[i]) or self.page0 != i,
                .round3 => ln == .p or ln == .pm or self.page0 != i,
                .page_only => self.page0 != i,
            };
        }

        /// The manager of host `h`'s set (M0 on A, P1 on B; P2's host C has
        /// no rule runner).
        fn managerOf(h: u8) u8 {
            return if (h == A) 0 else 1;
        }

        fn staleAlive(self: *const Self, h: u8) bool {
            const i = managerOf(h);
            return cfg.rule and self.alive[i] and self.managerStale(i);
        }

        fn kill(self: *Self, i: u8) void {
            if (self.isCurrent(i)) self.killed_current = true;
            self.alive[i] = false;
            self.line[i] = .none;
        }

        /// SA and JB: the rule (read, judge, kill), the register, the
        /// re-judge (a stale sibling there: the registration withdrawn, the
        /// sibling killed, the register retried).
        fn ruleRunner(self: *Self, t: u8, h: u8, end: u8) void {
            switch (self.pc[t]) {
                0 => {
                    if (self.busy(h, t)) return;
                    if (self.reg[h]) {
                        if (h == B) self.jb_enrolled = true;
                        self.pc[t] = end;
                        return;
                    }
                    self.lock[h] = t;
                    if (self.staleAlive(h)) self.kill(managerOf(h));
                },
                1 => {
                    self.reg[h] = true;
                    if (h == B) self.jb_enrolled = true;
                    self.window[h] = true;
                },
                2 => {
                    self.window[h] = false;
                    if (self.staleAlive(h)) {
                        self.unregisterHost(h);
                        self.kill(managerOf(h));
                        self.pc[t] = 1;
                        return;
                    }
                    self.lock[h] = FREE;
                },
                else => unreachable,
            }
            self.pc[t] += 1;
        }

        /// JB's clean departure (B3).
        fn depart(self: *Self) void {
            const siblings = self.alive[1] and self.line[1] != .none;
            self.jb_enrolled = false;
            if (!siblings and (!cfg.depart_checks_holder or !self.holds(B)) and self.reg[B]) self.unregisterHost(B);
        }

        pub fn invariant(self: *const Self) !void {
            if (self.killed_current) return error.KilledCurrent;
            if (self.wrote_over_live) return error.WroteOverLiveClaim;
            if (self.wrote_unheld) return error.WroteUnheld;
            if (self.fenced_wrote) return error.FencedWrote;
            const exempt = (self.lapse_happened and !cfg.strict.lapse) or (self.release_happened and !cfg.strict.release_window);
            if (!exempt) {
                var managers: u8 = 0;
                for (0..3) |i| {
                    if (!self.bound[i] or !self.alive[i] or self.failed[i]) continue;
                    const h = HOST[i];
                    if (self.window[h] and !cfg.strict.register_window) continue;
                    if (self.holds(h) or (self.holder != NONE and self.reg[h])) managers += 1;
                }
                if (managers > 1) return error.TwoManagers;
                if (self.holder == NONE) for (0..3) |i| {
                    if (self.bound[i] and self.alive[i] and !self.failed[i] and self.claim == i) return error.UnreservedUnderManager;
                };
            }
            if (cfg.strict.lapse and self.hit_lapse) return error.LapseWindow;
            if (cfg.strict.release_window and self.hit_release) return error.ReleaseWindow;
            if (cfg.strict.register_window and self.hit_register) return error.RegisterWindow;
        }

        pub fn final(_: *const Self) !void {}
    };
}

pub const Semantics = enum { spec, nvmet };

pub const MemberConfig = struct {
    semantics: Semantics = .spec,
    /// The retirement covers a page only at its term (B1, round 3 major 1).
    retire_term: bool = true,
    /// A leaving member's line marked `l` until its leave's last write
    /// (review round 3 major 4; false: dropped before the leave).
    leaving_line: bool = true,
    /// The recovery's re-fence once the page is Recovering (§DF.12).
    refence: bool = true,
    /// The rule's re-judge after its register (B2).
    rejudge: bool = true,
    strict: Strict = .{},
};

const Page = enum(u8) { live, recovering, recovered, free };
const MLine = enum(u8) { none, j, l };

pub fn Member(comptime cfg: MemberConfig) type {
    return extern struct {
        const Self = @This();
        pub const thread_count: usize = 4;
        const MG_STEPS = 8;
        const S_END = 3;

        /// Host B registered (host A's — the manager's — always is).
        reg_b: bool = true,
        page: [2]Page = .{ .live, .live },
        line: [2]MLine = .{ .j, .j },
        alive: [2]bool = .{ true, true },
        failed: [2]bool = .{ false, false },
        scanned: [2]bool = .{ false, false },
        /// MG passed over this member (its page not Live at the eviction).
        spared: [2]bool = .{ false, false },
        pc: [4]u8 = @splat(0),
        /// S holds host B's registry lock (from its read to its re-judge;
        /// Y's line changes wait for it).
        locked: bool = false,
        window: bool = false,
        killed_live: bool = false,
        wrote_after_scan: bool = false,
        hit_register: bool = false,

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                0 => self.pc[0] >= 1 or !self.alive[0] or self.failed[0],
                1 => self.pc[1] >= 3 or !self.alive[1] or self.failed[1],
                2 => self.pc[2] >= MG_STEPS,
                3 => self.pc[3] >= S_END,
                else => unreachable,
            };
        }

        /// A member's write. False = refused (the member latched).
        fn memberWrite(self: *Self, v: u8) bool {
            if (!self.reg_b) {
                self.failed[v] = true;
                return false;
            }
            if (self.scanned[v]) {
                if (self.window) self.hit_register = true else self.wrote_after_scan = true;
            }
            return true;
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                0 => {
                    _ = self.memberWrite(0);
                    self.pc[0] = 1;
                },
                1 => {
                    if (self.pc[1] != 1 and self.locked) return;
                    switch (self.pc[1]) {
                        0 => self.line[1] = if (cfg.leaving_line) .l else .none,
                        1 => if (self.memberWrite(1) and self.page[1] == .live) {
                            self.page[1] = .free;
                        },
                        2 => self.line[1] = .none,
                        else => unreachable,
                    }
                    self.pc[1] += 1;
                },
                2 => {
                    self.recovery(@intCast(self.pc[2] / 4), self.pc[2] % 4);
                    self.pc[2] += 1;
                },
                3 => self.rule(),
                else => unreachable,
            }
        }

        /// The manager's §5.9 recovery of member `v`, step `s`.
        fn recovery(self: *Self, v: u8, s: u8) void {
            if (s == 0) {
                if (self.page[v] != .live) {
                    self.spared[v] = true;
                    return;
                }
                if (cfg.semantics == .spec) self.reg_b = false;
                return;
            }
            if (self.spared[v]) return;
            switch (s) {
                1 => self.page[v] = .recovering,
                2 => if (cfg.refence and self.reg_b and cfg.semantics == .spec) {
                    self.reg_b = false;
                },
                3 => {
                    self.scanned[v] = true;
                    self.page[v] = .recovered;
                },
                else => unreachable,
            }
        }

        /// `judgeRegion` for a joiner (`j`) or a leaving joiner (`l`): Live
        /// current; Free spared on an `l` line; Recovering or Recovered stale
        /// unless the retirement covers it — X's `r` names appender 1 at
        /// term 1, its page is appender 1 at term 2.
        fn stale(self: *const Self, v: u8) bool {
            const ln = self.line[v];
            if (ln == .none) return false;
            return switch (self.page[v]) {
                .live => false,
                .free => ln == .j,
                .recovering, .recovered => !(v == 0 and !cfg.retire_term),
            };
        }

        fn anyStaleAlive(self: *const Self) bool {
            for (0..2) |v| if (self.alive[v] and self.stale(@intCast(v))) return true;
            return false;
        }

        fn killStale(self: *Self) void {
            for (0..2) |v| if (self.alive[v] and self.stale(@intCast(v))) {
                if (self.page[v] == .live) self.killed_live = true;
                self.alive[v] = false;
            };
        }

        fn rule(self: *Self) void {
            switch (self.pc[3]) {
                0 => if (self.reg_b) {
                    self.pc[3] = S_END;
                    return;
                } else {
                    self.locked = true;
                    self.killStale();
                },
                1 => {
                    self.reg_b = true;
                    if (cfg.rejudge) self.window = true else {
                        self.locked = false;
                        self.pc[3] = S_END;
                        return;
                    }
                },
                2 => {
                    self.window = false;
                    if (self.anyStaleAlive()) {
                        self.reg_b = false;
                        self.killStale();
                        self.pc[3] = 1;
                        return;
                    }
                    self.locked = false;
                },
                else => unreachable,
            }
            self.pc[3] += 1;
        }

        pub fn invariant(self: *const Self) !void {
            if (self.killed_live) return error.KilledCurrent;
            if (self.wrote_after_scan) return error.FencedMemberWrote;
            if (cfg.strict.register_window and self.hit_register) return error.RegisterWindow;
        }

        pub fn final(_: *const Self) !void {}
    };
}

// ---------------------------------------------------------------------------
// The checks.
// ---------------------------------------------------------------------------

const explorer = @import("../../model/explorer.zig");
const limits: explorer.Limits = .{};
const testing = std.testing;

fn statesOf(comptime M: type) !explorer.Report {
    var report: explorer.Report = undefined;
    try explorer.explore(M, testing.allocator, limits, &report);
    return report;
}

/// The invariant a failing schedule breaks first (null = holds).
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

test "model: the election as built (WERO, the claim probed before the registration and again under the hold, the rule judging a manager by the report then appender 0's page with `p` retired at enrolment, the re-judge, the lapse's claim check, the departure that keeps our hold) holds — no current manager killed, no write over a live claim or outside its own hold, no superseded write, one manager, never unreserved under the current manager" {
    try explorer.expectHolds(Election(.{}), limits);
    const r = try statesOf(Election(.{}));
    std.debug.print("\nhost fence model (election): {d} states explored, depth {d}\n", .{ r.states, r.max_depth_seen });
}

test "model: a member's fence as built under spec preempt semantics (the term-keyed retirement, the `l` line until the leave's last write, the re-judge, the §DF.12 re-fence) holds — no Live member killed, no evicted member writes after its tail scan" {
    try explorer.expectHolds(Member(.{}), limits);
    const r = try statesOf(Member(.{}));
    std.debug.print("\nhost fence model (member): {d} states explored, depth {d}\n", .{ r.states, r.max_depth_seen });
}

test "model, the accepted limit: on nvmet 7.3 the holder's death preempt fences nothing — an evicted member resumed after its tail scan writes (detection only, decision 0003 §DF)" {
    try explorer.expectFails(Member(.{ .semantics = .nvmet }), limits, error.InvariantViolated);
    try testing.expectEqual(@as(?anyerror, error.FencedMemberWrote), firstError(Member(.{ .semantics = .nvmet })));
}

test "model, the accepted windows are reachable: a PTPL-less lapse, a successor's clean last release before the superseded manager's heartbeat, a rule's register before its re-judge" {
    try explorer.expectFails(Election(.{ .strict = .{ .lapse = true } }), limits, error.InvariantViolated);
    try explorer.expectFails(Election(.{ .strict = .{ .release_window = true } }), limits, error.InvariantViolated);
    try testing.expectEqual(@as(?anyerror, error.RegisterWindow), firstError(Member(.{ .strict = .{ .register_window = true } })));
}

test "model weakening: rtype 5 with a registered host's conflicting acquire taken as held — two successors both bind (review round 3 N1)" {
    try explorer.expectFails(Election(.{ .hold = .wear_registered_holds }), limits, error.InvariantViolated);
}

test "model weakening: no stale-sibling rule — a re-registration of the superseded manager's host re-admits it" {
    try explorer.expectFails(Election(.{ .rule = false }), limits, error.InvariantViolated);
}

test "model weakening (review round 3 major 1): round 3's judgement (`p` never retired, stale whenever the rule runs) kills the live manager at a lapse" {
    try explorer.expectFails(Election(.{ .judge = .round3 }), limits, error.InvariantViolated);
}

test "model weakening (review round 3 major 1): a manager judged by appender 0's page alone spares the superseded manager while its page is still Live" {
    try explorer.expectFails(Election(.{ .judge = .page_only }), limits, error.InvariantViolated);
}

test "model weakening (review round 3 major 2): round 3's order — the host registered and the pre-gate writes before the claim is read" {
    try explorer.expectFails(Election(.{ .claim_first = false }), limits, error.InvariantViolated);
}

test "model weakening: no second probe under the hold — a writer that bound between the probe and the hold is written over" {
    try explorer.expectFails(Election(.{ .reprobe = false }), limits, error.InvariantViolated);
}

test "model weakening (review round 3 major 3): the lapse re-acquired without the device's claim check — the superseded manager wins the hold back after its successor's clean last unmount" {
    try explorer.expectFails(Election(.{ .lapse_claim_check = false }), limits, error.InvariantViolated);
}

test "model weakening: the departure that unregisters the host when no OTHER entry stands, holder or not, the manager enrolled only at its bind (review B3)" {
    try explorer.expectFails(Election(.{ .depart_checks_holder = false, .early_enrol = false }), limits, error.InvariantViolated);
}

test "model weakening: the release → preempt → re-acquire death preempt (the first §DF.11 wiring)" {
    try explorer.expectFails(Election(.{ .release_cycle = true }), limits, error.InvariantViolated);
}

test "model weakening (review B1, round 3 major 1): the retirement mark without the page term spares an evicted re-joined member" {
    try explorer.expectFails(Member(.{ .retire_term = false }), limits, error.InvariantViolated);
}

test "model weakening (review round 3 major 4): a leaving member's line dropped before its leave's writes — the rule cannot see it" {
    try explorer.expectFails(Member(.{ .leaving_line = false }), limits, error.InvariantViolated);
}

test "model weakening: no §DF.12 re-fence once the page is Recovering" {
    try explorer.expectFails(Member(.{ .refence = false }), limits, error.InvariantViolated);
}

test "model weakening (review B2): no re-judge after the rule's register" {
    try explorer.expectFails(Member(.{ .rejudge = false }), limits, error.InvariantViolated);
}

fn expectBreaks(name: []const u8, found: ?anyerror, allowed: []const anyerror) !void {
    const e = found orelse return error.TestExpectedFailure;
    std.debug.print("model weakening {s}: {s}\n", .{ name, @errorName(e) });
    for (allowed) |a| if (e == a) return;
    return error.TestUnexpectedError;
}

test "model: which invariant each weakening breaks" {
    try testing.expectEqual(@as(?anyerror, null), firstError(Election(.{})));
    try testing.expectEqual(@as(?anyerror, null), firstError(Member(.{})));
    try expectBreaks("rtype 5 (N1)", firstError(Election(.{ .hold = .wear_registered_holds })), &.{ error.TwoManagers, error.WroteOverLiveClaim });
    try expectBreaks("no rule", firstError(Election(.{ .rule = false })), &.{ error.FencedWrote, error.TwoManagers, error.WroteUnheld });
    try expectBreaks("round 3's judgement (major 1)", firstError(Election(.{ .judge = .round3 })), &.{error.KilledCurrent});
    try expectBreaks("the page alone (major 1)", firstError(Election(.{ .judge = .page_only })), &.{ error.FencedWrote, error.TwoManagers, error.WroteUnheld });
    try expectBreaks("register before the claim (major 2)", firstError(Election(.{ .claim_first = false })), &.{ error.WroteOverLiveClaim, error.WroteUnheld });
    try expectBreaks("no second probe", firstError(Election(.{ .reprobe = false })), &.{error.WroteOverLiveClaim});
    try expectBreaks("no lapse claim check (major 3)", firstError(Election(.{ .lapse_claim_check = false })), &.{error.FencedWrote});
    try expectBreaks("B3 departure", firstError(Election(.{ .depart_checks_holder = false, .early_enrol = false })), &.{ error.UnreservedUnderManager, error.WroteOverLiveClaim });
    try expectBreaks("release-cycle death preempt", firstError(Election(.{ .release_cycle = true })), &.{ error.UnreservedUnderManager, error.WroteOverLiveClaim });
    try expectBreaks("term-less retirement (B1)", firstError(Member(.{ .retire_term = false })), &.{error.FencedMemberWrote});
    try expectBreaks("no leaving line (major 4)", firstError(Member(.{ .leaving_line = false })), &.{error.FencedMemberWrote});
    try expectBreaks("no re-fence (§DF.12)", firstError(Member(.{ .refence = false })), &.{error.FencedMemberWrote});
    try expectBreaks("no re-judge (B2)", firstError(Member(.{ .rejudge = false })), &.{error.FencedMemberWrote});
    try expectBreaks("nvmet 7.3 (the accepted limit)", firstError(Member(.{ .semantics = .nvmet })), &.{error.FencedMemberWrote});
}
