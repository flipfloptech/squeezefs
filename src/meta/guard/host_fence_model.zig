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
//!   0 M0  host A's manager, frozen past its claim's TTL: its fencing
//!         registrant's death preempt of a host-B member's key (a recovery
//!         it was running when frozen; decision 0003 §DG), a write, its
//!         heartbeat, a write — each at any point of the schedule (the
//!         resume). Its `m` line stands; appender 0's page names it.
//!   1 P1  a successor on host B: the claim probed (no write), the host
//!         registered (`p` enrolled under the registry's lock), the claim
//!         read again and the hold in one step (acquire, or the holder's key
//!         preempted; a fresh claim refuses without a preempt — review 5
//!         major 1, the instant between them abstracted), the claim probed again
//!         (a fresh claim whose writer this open's preempt fenced is taken
//!         over), the replay and the pre-gate writes (held tail, fold,
//!         cover), the gate's claim read and its fencing registrant stood
//!         (registered under the host's key, §DG), the bind (claim committed,
//!         appender 0's page, `m` enrolled, `p` retired); then its clean
//!         unmount (the claim deleted, the fencer unregistered; a hold taken
//!         by a preempt KEPT, else released as the namespace's last
//!         registrant).
//!   2 P2  a successor on host C: the same — paused at any step across
//!         P1's whole takeover; then (`release_cycle` only) the first §DF.11
//!         death preempt of a host-B member.
//!   3 SA  a joiner mounting on host A: the rule judges M0, registers host A,
//!         re-judges.
//!   4 JB  a joiner on host B: the rule judges P1, registers, re-judges;
//!         then departs (B3: it never unregisters a hold that is ours).
//!   5 L   a PTPL-less target power cycle: every registration and the hold
//!         forgotten.
//!   6 R0  M0's recovery thread finishing a death it ran when frozen: the
//!         report (does host A — our key under our host id — hold?) then its
//!         fencer's preempt of a host-B member's key; the two under the
//!         fencer's lock, which the heartbeat's re-register and check take
//!         too (review M1 of decision 0003 §DG).
//!
//! `Member` — the manager MG on host A holds, its fencing registrant F_A (a
//! write-free second identity of host A under host A's key, a NON-holder)
//! registered beside it; members X and Y on host B; W a member co-located
//! with MG on host A (no write: its guarantee row is the flock, T_self and
//! the frame screen, never the device):
//!   0 X   a frozen member (re-joined in place once: its retirement `r`
//!         names appender 1 at term 1; its page is appender 1 at term 2):
//!         one write at any point.
//!   1 Y   a member leaving: its line marked `l`, its leave's write (the
//!         page Free, the identity cleared), the line removed.
//!   2 MG  each member whose page is Live evicted in turn: the death
//!         preempt of its host's key (through F_A; the own-key guard for
//!         W), the page Recovering, the re-fence (§DF.12), the tail scan,
//!         `releaseRecovered` (Free, the identity kept — the pair reusable
//!         from here); then MG's clean unmount (the hold kept when a preempt
//!         landed under it, §DF.6).
//!   3 S   a joiner mounting on host B: the rule, the register, the re-judge.
//!   4 Z   a joiner elsewhere: `JoinAppender` takes the lowest Free pair and
//!         writes its own identity.
//!   Device semantics (nvmet 7.3.1 `pr.c`, a preempt by key): `spec` (a
//!   holder's preempt of another key unregisters every registrant of it) or
//!   `nvmet` 7.3 (a HOLDER's preempt of another key is a no-op; a
//!   NON-holder's — the fencer's — unregisters every registrant of it, the
//!   sender not skipped, the reservation kept). A preempt of the holder's own
//!   key unregisters the holder: unreserved.
//!
//! Invariants: `KilledCurrent` the rule never kills the current manager or a
//! Live member; `PreemptedLive` a hold never preempts a live manager's host
//! (its acked-but-unsynced writes lost for nothing, review 5 major 1); `WroteOverLiveClaim` no open writes while another writer's
//! claim is fresh and that writer can still write (review round 3 major 2);
//! `WroteUnheld` a writer open writes only under its own hold;
//! `OverwroteAcked` a superseded or stale incarnation — a manager whose
//! successor bound, an open whose replay a later bind superseded — never
//! lands a write over that successor's acked metadata (review 4 blocker 2);
//! `FencedWrote` a manager the device superseded never writes;
//! `TwoManagers` at most one bound manager admitted under a standing
//! reservation; `UnreservedUnderManager` never unreserved under the current
//! manager; `FencerHeld` a fencing registrant never holds the reservation;
//! `FencedMemberWrote` an evicted member never writes after its region's
//! tail scan; `FencerFenced` the manager's fencer is never the victim of a
//! member preempt; `ManagerUnreserved` a member preempt never unreserves the
//! manager's namespace; `FencedHostRegistered` once a death preempt landed,
//! no registration of the fenced host (its own, its fencer's) stands until
//! a rule's register re-admits it; `FencerWrote` no write lands under a
//! fencer's identity.
//!
//! The ACCEPTED LIMITS (counted, not violations; `Strict` makes each one a
//! violation and a test shows it reachable): `lapse` — a PTPL-less target
//! power cycle forgets every fence until the manager's heartbeat re-acquires
//! (anything superseded and resumed meanwhile writes); `register_window` —
//! between a rule's register and its re-judge, a sibling that turned stale
//! meanwhile is admitted. Without the fencer, on nvmet 7.3 the holder's
//! death preempt fences nothing: `Member(.{ .semantics = .nvmet, .fencer =
//! false })` FAILS `FencedMemberWrote` (§DF.11's limit, the no-fencer
//! posture); with it the member model HOLDS under both semantics (§DG).
//! Round 4's release window is closed (a hold taken by a preempt is kept):
//! `keep_preempted_hold = false` FAILS `OverwroteAcked` — M0's checkpoint,
//! or an open paused across the takeover.

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
    register_window: bool = false,
};

pub const Hold = enum { wero, wear_registered_holds };

/// How the rule judges a manager's line: the report first, then appender
/// 0's page (the fix of review round 3 major 1); round 3's (`p` stale
/// whenever the rule runs, never retired; `m` by the page); the page alone.
pub const JudgeManager = enum { report_then_page, round3, page_only };

/// The fencing registrant's key (decision 0003 §DG): its host's own (`host`
/// — a preempt of the host's key unregisters the host and its fencer in one
/// command) or a key of its own (`distinct` — it outlives its host's fence).
pub const FencerKey = enum { host, distinct };

pub const ElectionConfig = struct {
    hold: Hold = .wero,
    rule: bool = true,
    judge: JudgeManager = .report_then_page,
    /// The claim probed before the registration (false: round 3 — register,
    /// pre-gate writes, the gate's claim read, the hold).
    claim_first: bool = true,
    /// The claim read again right before the hold (review 5 major 1; false:
    /// round 5 — the first probe's reading, from before the rule).
    probe_before_hold: bool = true,
    /// The second probe under the hold takes over a fresh foreign claim
    /// whose writer this open's preempt fenced (review 4 minor 4; false:
    /// round 4 — the open refuses, the set managerless until the claim ages).
    reprobe_proceeds: bool = true,
    /// After a lapse the hold re-acquired only while the device's claim
    /// names this incarnation (review round 3 major 3).
    lapse_claim_check: bool = true,
    /// A holder that took its hold by a preempt keeps it at its last clean
    /// unmount, the next mount preempts it (review 4 blocker 2; false: round
    /// 4 — released as the namespace's last registrant).
    keep_preempted_hold: bool = true,
    depart_checks_holder: bool = true,
    early_enrol: bool = true,
    release_cycle: bool = false,
    strict: Strict = .{},
    /// `TwoManagers` and `UnreservedUnderManager` checked (off only to show
    /// which write a weakening's first structural breach leads to).
    structural: bool = true,
    /// The second successor runs (off only to show M0's own write).
    p2: bool = true,
    fencer_key: FencerKey = .host,
    /// M0, resumed, finishes a recovery it ran when frozen: its fencer's
    /// death preempt of a host-B member's key (thread R0).
    m0_fences: bool = true,
    /// Review M1: a fencer preempts only after a report shows its host
    /// holding, and a re-registered fencer is taken back when the host does
    /// not hold (false: neither check).
    fencer_hold_check: bool = true,
    /// The fencer's lock: the recovery's check and preempt, the heartbeat's
    /// re-register and check, each one critical section (false: separate
    /// steps nothing orders).
    fencer_lock: bool = true,
    /// The lapse's claim read and its re-acquire as separate steps (true:
    /// the code's order — the claim probe, then the register and acquire —
    /// with a successor's bind between them; shown reachable, an accepted
    /// instant of the lapse class, §DF.13(k), §DG.13(h)).
    lapse_acquire_split: bool = false,
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
        o.add(.reprobe);
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
        pub const thread_count: usize = 7;
        const order = [3]Order{ .{}, successorOrder(cfg, 1), successorOrder(cfg, 2) };
        const HOST = [3]u8{ A, B, C };
        const SA_END = 3;
        const JB_DEPART = 3;
        const JB_END = 4;
        /// Write, the beat (report; after a lapse the host's register), the
        /// acquire, the fencer's re-register, the check, write.
        const M0_STEPS: u8 = 6;
        const R0 = 6;

        reg: [3]bool = .{ true, false, false },
        /// Each host's fencing registrant registered (M0 stood host A's at
        /// its hold).
        reg_f: [3]bool = .{ true, false, false },
        /// M0's beat found the lapse and registered host A (the re-acquire's
        /// remaining steps run).
        m0_lapsed: bool = false,
        /// The fencer's lock (FREE, M0 or R0).
        flock: u8 = FREE,
        /// R0's report showed host A holding.
        r0_ok: bool = false,
        /// The holder's host (NONE: unreserved).
        holder: u8 = A,
        /// The holder is that host's FENCER, not the host (nvmet's preempt of
        /// the holder's key by a non-holder, `nvmet_pr_set_new_holder`).
        holder_fencer: bool = false,
        /// WEAR's conflicting acquire taken as held (the N1 weakening).
        wear_held: [3]bool = @splat(false),
        /// The claim's incarnation (NONE: deleted); M0's is stale.
        claim: u8 = 0,
        /// Appender 0's Live page names this manager (NONE: Free).
        page0: u8 = 0,
        pc: [7]u8 = @splat(0),
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
        /// M0's heartbeat re-acquired the hold after a lapse.
        m0_reacquired: bool = false,
        /// +1 at every bind: a manager's acked metadata from there on.
        epoch: u8 = 0,
        /// The epoch each incarnation's view (its replay) reads.
        view: [3]u8 = @splat(0),
        /// The claim a successor's second probe found fresh and took over
        /// (its writer fenced by this open's preempt).
        fenced_claim: [3]u8 = @splat(NONE),
        killed_current: bool = false,
        wrote_over_live: bool = false,
        wrote_unheld: bool = false,
        fenced_wrote: bool = false,
        overwrote_acked: bool = false,
        preempted_live: bool = false,
        fencer_held: bool = false,
        fencer_preempt_unheld: bool = false,
        hit_lapse: bool = false,
        hit_register: bool = false,

        pub fn init() Self {
            return .{};
        }

        fn admitted(self: *const Self, h: u8) bool {
            return self.holder == NONE or self.reg[h];
        }

        fn holds(self: *const Self, h: u8) bool {
            return (self.holder == h and !self.holder_fencer) or self.wear_held[h];
        }

        /// The report names host `h`'s KEY the holder: its own registration,
        /// or its fencer's when that carries the same key.
        fn keyHolds(self: *const Self, h: u8) bool {
            return self.holder == h and (!self.holder_fencer or cfg.fencer_key == .host);
        }

        fn otherRegistered(self: *const Self, h: u8) bool {
            for (0..3) |x| if (x != h and self.reg[x]) return true;
            return false;
        }

        /// The host's own unregister (a departure, a release): by its host
        /// id, so its fencer's registration stays.
        fn unregisterHost(self: *Self, h: u8) void {
            self.reg[h] = false;
            self.wear_held[h] = false;
            if (self.holder == h and !self.holder_fencer) self.holder = NONE;
        }

        /// A preempt of host `h`'s key by another registrant: every
        /// registrant of the key goes — the host, and its fencer when that
        /// carries the same key (nvmet `nvmet_pr_unreg_all_*_by_prkey`).
        fn preemptKeyOf(self: *Self, h: u8) void {
            self.reg[h] = false;
            self.wear_held[h] = false;
            if (cfg.fencer_key == .host) self.reg_f[h] = false;
        }

        /// Another writer's claim is fresh (M0's aged past the TTL).
        fn claimFreshForeign(self: *const Self, m: u8) bool {
            const c = self.claim;
            return c != NONE and c != m and c != 0 and self.alive[c] and self.bound[c] and !self.failed[c];
        }

        /// ... and its writer can still write (not fenced at the device).
        fn claimLiveForeign(self: *const Self, m: u8) bool {
            return self.claimFreshForeign(m) and self.admitted(HOST[self.claim]);
        }

        fn isCurrent(self: *const Self, i: u8) bool {
            return self.bound[i] and self.alive[i] and !self.failed[i] and self.claim == i and ((self.holder == HOST[i] and !self.holder_fencer) or self.holder == NONE);
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                0 => self.pc[0] >= M0_STEPS or !self.alive[0] or self.failed[0],
                1 => self.pc[1] >= order[1].len or !self.alive[1] or self.failed[1],
                2 => !cfg.p2 or self.pc[2] >= order[2].len or !self.alive[2] or self.failed[2],
                3 => self.pc[3] >= SA_END,
                4 => self.pc[4] >= JB_END,
                5 => self.pc[5] >= 1,
                R0 => !cfg.m0_fences or self.pc[R0] >= 2 or !self.alive[0],
                else => unreachable,
            };
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                0 => {
                    switch (self.pc[0]) {
                        0, 5 => self.m0Write(),
                        1 => if (!self.m0Beat()) return,
                        2 => self.m0Acquire(),
                        3 => if (!self.m0FencerRegister()) return,
                        4 => self.m0FencerCheck(),
                        else => unreachable,
                    }
                    self.pc[0] += 1;
                },
                R0 => {
                    if (self.pc[R0] == 0) {
                        if (cfg.fencer_lock) {
                            if (self.flock != FREE) return;
                            self.flock = R0;
                        }
                        self.r0_ok = !cfg.fencer_hold_check or self.holds(A);
                        if (!self.r0_ok) {
                            if (self.flock == R0) self.flock = FREE;
                            self.pc[R0] = 2;
                            return;
                        }
                    } else {
                        self.m0FencerPreempt();
                        if (self.flock == R0) self.flock = FREE;
                    }
                    self.pc[R0] += 1;
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
                    self.reg_f = @splat(false);
                    self.wear_held = @splat(false);
                    self.holder = NONE;
                    self.holder_fencer = false;
                    self.lapse_happened = true;
                    self.pc[5] = 1;
                },
                else => unreachable,
            }
        }

        /// M0 resumed: a write (its conveyor's, or a checkpoint's nodes,
        /// bitmap pages and ledger record). Refused by the device → the
        /// volume latched.
        fn m0Write(self: *Self) void {
            if (!self.admitted(A)) {
                self.failed[0] = true;
                return;
            }
            const stale = self.view[0] < self.epoch;
            const fenced = self.holder != NONE and (self.holder != A or self.holder_fencer);
            if (!stale and !fenced) return;
            // Past its own re-acquire the write is the re-acquire's, not the
            // lapse window's (review round 3 major 3).
            if (self.lapse_happened and !self.m0_reacquired) {
                self.hit_lapse = true;
            } else if (self.window[A]) {
                self.hit_register = true;
            } else if (stale) {
                self.overwrote_acked = true;
            } else self.fenced_wrote = true;
        }

        /// The heartbeat's PR re-check (`gate.prRecheck`). False = waiting
        /// for host A's registry lock (the re-register).
        fn m0Beat(self: *Self) bool {
            // The report: held by host A (our key under our host id).
            if (self.holds(A)) return true;
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
            self.m0_lapsed = true;
            if (!cfg.lapse_acquire_split) self.m0Acquire();
            return true;
        }

        /// The re-acquire's hold: an acquire on an unreserved namespace; a
        /// reservation standing (a successor's, since the register) or our
        /// registration gone conflicts — `reacquire_failed`, fail-stop.
        fn m0Acquire(self: *Self) void {
            if (!self.m0_lapsed or self.m0_reacquired or self.failed[0]) return;
            if (self.holder == NONE and self.reg[A]) {
                self.holder = A;
                self.m0_reacquired = true;
            } else if (!self.holds(A)) self.failed[0] = true;
        }

        /// The fencer's re-register (the lapse forgot it), under its lock.
        /// False = waiting for the lock.
        fn m0FencerRegister(self: *Self) bool {
            if (!self.m0_lapsed) return true;
            if (cfg.fencer_lock) {
                if (self.flock != FREE and self.flock != 0) return false;
                self.flock = 0;
            }
            self.reg_f[A] = true;
            return true;
        }

        /// Review M1: the host must still hold, else the fencer is taken back
        /// and the volume usurped; the lock released.
        fn m0FencerCheck(self: *Self) void {
            if (self.m0_lapsed and cfg.fencer_hold_check and !self.holds(A)) {
                self.reg_f[A] = false;
                self.failed[0] = true;
            }
            if (self.flock == 0) self.flock = FREE;
        }

        /// M0's fencer preempts a host-B member's key. Its registration gone,
        /// the command conflicts (nvmet finds no registrant under its host
        /// id and key); the victim's key holding, the fencer TAKES the
        /// reservation and every other registrant of the key goes
        /// (`nvmet_pr_set_new_holder` + `nvmet_pr_unreg_all_others_by_prkey`);
        /// else every registrant of the key goes, the reservation kept.
        fn m0FencerPreempt(self: *Self) void {
            if (!self.reg_f[A]) return;
            if (!self.holds(A)) self.fencer_preempt_unheld = true;
            if (self.keyHolds(B)) {
                self.holder = A;
                self.holder_fencer = true;
                self.fencer_held = true;
            }
            self.preemptKeyOf(B);
        }

        /// Host `h`'s registry lock is held by a thread other than `t`.
        fn busy(self: *const Self, h: u8, t: u8) bool {
            return self.lock[h] != FREE and self.lock[h] != t;
        }

        /// A writer open's write (pre-gate, bind, unmount). False = refused.
        fn writerWrite(self: *Self, m: u8, h: u8) bool {
            if (!self.admitted(h)) return false;
            const stale = self.view[m] < self.epoch;
            const over_live = self.claimLiveForeign(m);
            if (self.holds(h) and !stale and !over_live) return true;
            if (self.lapse_happened) {
                self.hit_lapse = true;
                return true;
            }
            if (!self.holds(h) and self.window[h]) {
                self.hit_register = true;
                return true;
            }
            if (stale) self.overwrote_acked = true;
            if (over_live) self.wrote_over_live = true;
            if (!self.holds(h) and self.holder != NONE) self.wrote_unheld = true;
            return true;
        }

        /// A refused open: its line out; the registration given back as
        /// `host_fence.leaveRegistration` says (a preempting hold kept).
        fn discard(self: *Self, m: u8, h: u8) void {
            self.alive[m] = false;
            self.line[m] = .none;
            // Its fencer's last user: unregistered.
            self.reg_f[h] = false;
            if (!self.reg[h]) return;
            if (h == B and self.jb_enrolled) return;
            if (self.holder == h) {
                if (self.preempted[m] or self.otherRegistered(h)) return;
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
                .reprobe => if (self.claimFreshForeign(m)) {
                    if (cfg.reprobe_proceeds and self.preempted[m] and self.holds(h)) self.fenced_claim[m] = self.claim else self.discard(m, h);
                },
                // The gate's claim read; its fencer stood (registered under
                // the host's key whatever the host's registration reads).
                .gate => if (self.claimFreshForeign(m) and self.fenced_claim[m] != self.claim) self.discard(m, h) else {
                    self.reg_f[h] = true;
                },
                // The replay (the view) and the pre-gate writes.
                .write => {
                    self.view[m] = self.epoch;
                    if (!self.writerWrite(m, h)) self.discard(m, h);
                },
                .bind => if (self.writerWrite(m, h)) {
                    self.claim = m;
                    self.page0 = m;
                    self.bound[m] = true;
                    self.role_passed = true;
                    self.epoch += 1;
                    self.view[m] = self.epoch;
                    self.line[m] = if (cfg.judge == .round3) .pm else .m;
                } else self.discard(m, h),
                .unmount => {
                    self.reg_f[h] = false;
                    if (self.writerWrite(m, h)) {
                        if (self.claim == m) self.claim = NONE;
                        if (self.page0 == m) self.page0 = NONE;
                        if (self.reg[h] and !(h == B and self.jb_enrolled)) {
                            if (!self.holds(h)) {
                                self.unregisterHost(h);
                            } else if (!(cfg.keep_preempted_hold and self.preempted[m]) and !self.otherRegistered(h)) {
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
                .death_preempt => if (self.holder == NONE and self.reg[B]) self.preemptKeyOf(B),
                .death_reacquire => if (self.holder == NONE and self.reg[h]) {
                    self.holder = h;
                },
            }
            self.pc[m] += 1;
        }

        fn hold(self: *Self, m: u8, h: u8) void {
            if (!self.reg[h]) return self.discard(m, h);
            // The claim read right before the hold (review 5 major 1), one
            // step with it: the instant between them is one report (the
            // rule's tens of seconds are not).
            if (cfg.probe_before_hold and self.claimFreshForeign(m)) return self.discard(m, h);
            if (self.holder == NONE) {
                self.holder = h;
            } else if (self.holder != h) switch (cfg.hold) {
                // A non-holder's preempt of the holder's key: the hold
                // passes, the holder's host unregistered.
                .wero => {
                    const v = self.holder;
                    if (self.holder_fencer) {
                        // The holder is host v's fencer: its key goes (the
                        // host's registration with it when they share it).
                        self.reg_f[v] = false;
                        if (cfg.fencer_key == .host) self.reg[v] = false;
                        self.holder_fencer = false;
                    } else {
                        self.notePreemptOf(v);
                        self.preemptKeyOf(v);
                    }
                    self.holder = h;
                    self.preempted[m] = true;
                },
                // rtype 5: the conflicting acquire taken as held; the stale
                // claim's key (host A) preempted while it holds.
                .wear_registered_holds => if (self.holder == A) {
                    self.preemptKeyOf(A);
                    self.holder = h;
                    self.preempted[m] = true;
                } else {
                    self.wear_held[h] = true;
                },
            };
        }

        /// A preempt of host `victim`'s key: a live manager there (its claim
        /// fresh) loses its acked-but-unsynced writes for nothing.
        fn notePreemptOf(self: *Self, victim: u8) void {
            const c = self.claim;
            if (c != NONE and c != 0 and HOST[c] == victim and self.alive[c] and self.bound[c] and !self.failed[c]) self.preempted_live = true;
        }

        /// The manager line of incarnation `i` judged stale (`judgeRegion`).
        fn managerStale(self: *const Self, i: u8) bool {
            const ln = self.line[i];
            if (ln == .none) return false;
            return switch (cfg.judge) {
                .report_then_page => (self.holder != NONE and !self.keyHolds(HOST[i])) or self.page0 != i,
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
            if (self.preempted_live) return error.PreemptedLive;
            if (self.wrote_over_live) return error.WroteOverLiveClaim;
            if (self.wrote_unheld) return error.WroteUnheld;
            if (self.overwrote_acked) return error.OverwroteAcked;
            if (self.fenced_wrote) return error.FencedWrote;
            if (self.fencer_preempt_unheld) return error.FencerPreemptUnheld;
            if (self.fencer_held) return error.FencerHeld;
            if (cfg.structural and (!self.lapse_happened or cfg.strict.lapse)) {
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
            if (cfg.strict.register_window and self.hit_register) return error.RegisterWindow;
        }

        pub fn final(_: *const Self) !void {}
    };
}

pub const Semantics = enum { spec, nvmet };

pub const MemberConfig = struct {
    semantics: Semantics = .spec,
    /// The death preempt and the re-fence issued by the manager's FENCING
    /// REGISTRANT — a write-free second identity of its host, a non-holder
    /// (decision 0003 §DG); false: the holder's own preempt (the no-fencer
    /// posture: multipath=Y, a failed connect — §DF.11's limit on nvmet).
    fencer: bool = true,
    fencer_key: FencerKey = .host,
    /// multipath=Y: a member's write leaves through any controller of its
    /// host's head — its host's fencer's included (the worst routing).
    merged_path: bool = false,
    /// The death preempt of the manager's OWN key withheld (a co-located
    /// member: the device cannot fence inside one host).
    own_key_guard: bool = true,
    /// A preempt the fencer landed keeps the manager's hold at its clean
    /// unmount like its own client's (§DF.6; false: only the holder's
    /// client's preempts counted).
    fencer_preempts_keep_hold: bool = true,
    /// The retirement covers a page only at its term (B1, round 3 major 1).
    retire_term: bool = true,
    /// A leaving member's line marked `l` until its leave's last write
    /// (review round 3 major 4; false: dropped before the leave).
    leaving_line: bool = true,
    /// A joined member's line carries its appender id and term (`j <slot>
    /// <wid> <aid> <term> <vol>`): stale unless that page is Live under the
    /// same writer id and term or retired (review 4 blocker 1; false: round
    /// 4 — judged by the pages that name its writer id, none → joining).
    aid_on_line: bool = true,
    /// Round 4's `l`-line exception: a Free page naming the leaver spared
    /// (review 4 blocker 1b — that page is a released recovery's).
    leave_free_exception: bool = false,
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
        pub const thread_count: usize = 5;
        const X: u8 = 0;
        const Y: u8 = 1;
        /// The member co-located with the manager on host A (pair 2).
        const W: u8 = 2;
        /// The new joiner's identity (never a pair index).
        const Z: u8 = 4;
        const PAIRS = 3;
        /// Five recovery steps per pair, then the manager's unmount.
        const MG_UNMOUNT = 5 * PAIRS;
        const MG_STEPS = MG_UNMOUNT + 1;
        const S_END = 3;

        /// Host B registered (host A's — the manager's — holds).
        reg_b: bool = true,
        reg_a: bool = true,
        /// The manager's fencer registered (stood at its hold).
        reg_fa: bool = cfg.fencer,
        /// Host B's fencer registered: B was the manager's host once; A's
        /// takeover preempted host B's key, which took a fencer under that
        /// key with it — a key of its own outlived it.
        reg_fb: bool = cfg.fencer_key == .distinct,
        /// The manager's reservation stands (WERO under host A's key).
        reserved: bool = true,
        /// A death preempt landed on host B's key; cleared by the rule's
        /// register (the host re-admitted).
        fenced_b: bool = false,
        /// A preempt landed that keeps the hold at the manager's unmount.
        keep_hold: bool = false,
        /// Three page pairs: pair 0 is X's region (appender 1), pair 1 Y's
        /// (appender 2), pair 2 W's (appender 3). Each names an owner (X, Y,
        /// W, Z or NONE: the identity cleared) in a state.
        owner: [PAIRS]u8 = .{ X, Y, W },
        state: [PAIRS]Page = @splat(.live),
        line: [2]MLine = .{ .j, .j },
        alive: [2]bool = .{ true, true },
        failed: [2]bool = .{ false, false },
        scanned: [PAIRS]bool = @splat(false),
        /// MG passed over this pair (its page not Live at the eviction).
        spared: [PAIRS]bool = @splat(false),
        pc: [5]u8 = @splat(0),
        /// S holds host B's registry lock (from its read to its re-judge;
        /// Y's line changes wait for it).
        locked: bool = false,
        window: bool = false,
        killed_live: bool = false,
        wrote_after_scan: bool = false,
        fencer_wrote: bool = false,
        fencer_fenced: bool = false,
        manager_unreserved: bool = false,
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
                4 => self.pc[4] >= 1,
                else => unreachable,
            };
        }

        fn ownLive(self: *const Self, v: u8) bool {
            return self.owner[v] == v and self.state[v] == .live;
        }

        /// A member's write. False = refused (the member latched). Under
        /// multipath=Y the head routes it through host B's fencer's path.
        fn memberWrite(self: *Self, v: u8) bool {
            const via_fencer = cfg.merged_path;
            const registered = if (via_fencer) self.reg_fb else self.reg_b;
            if (self.reserved and !registered) {
                self.failed[v] = true;
                return false;
            }
            if (via_fencer and self.reserved) self.fencer_wrote = true;
            if (self.scanned[v]) {
                if (self.window) self.hit_register = true else self.wrote_after_scan = true;
            }
            return true;
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                0 => {
                    _ = self.memberWrite(X);
                    self.pc[0] = 1;
                },
                1 => {
                    if (self.pc[1] != 1 and self.locked) return;
                    switch (self.pc[1]) {
                        0 => self.line[Y] = if (cfg.leaving_line) .l else .none,
                        // The leave's writes; its page goes Free, the
                        // identity cleared (`LeaveAppender`).
                        1 => if (self.memberWrite(Y) and self.ownLive(Y)) {
                            self.state[Y] = .free;
                            self.owner[Y] = NONE;
                        },
                        2 => self.line[Y] = .none,
                        else => unreachable,
                    }
                    self.pc[1] += 1;
                },
                2 => {
                    if (self.pc[2] == MG_UNMOUNT) {
                        self.unmount();
                    } else self.recovery(@intCast(self.pc[2] / 5), self.pc[2] % 5);
                    self.pc[2] += 1;
                },
                3 => self.rule(),
                // A new joiner elsewhere: `JoinAppender` takes the lowest
                // Free pair and writes its own identity.
                4 => {
                    for (0..PAIRS) |i| if (self.state[i] == .free) {
                        self.owner[i] = Z;
                        self.state[i] = .live;
                        break;
                    };
                    self.pc[4] = 1;
                },
                else => unreachable,
            }
        }

        /// The death preempt of pair `v`'s host key (host A's for W). By the
        /// fencer: a non-holder — every registrant of the key goes, the
        /// sender not skipped (nvmet `nvmet_pr_unreg_all_host_by_prkey`, the
        /// spec's rule); its registration gone, its command conflicts. By the
        /// holder: spec semantics unregister the key's registrants, nvmet's
        /// only re-type the holder's own hold.
        fn preempt(self: *Self, v: u8) void {
            const own = v == W;
            if (own and cfg.own_key_guard) return;
            if (cfg.fencer) {
                if (!self.reg_fa) return;
            } else if (cfg.semantics == .nvmet or own) return;
            if (own) {
                if (!self.reg_a) return;
                // Host A's key: the holder unregistered — unreserved — and
                // the fencer under that key with it.
                self.reg_a = false;
                self.reserved = false;
                self.manager_unreserved = true;
                if (cfg.fencer_key == .host and self.reg_fa) {
                    self.reg_fa = false;
                    self.fencer_fenced = true;
                }
                return;
            }
            if (!self.reg_b) return;
            self.reg_b = false;
            if (cfg.fencer_key == .host) self.reg_fb = false;
            self.fenced_b = true;
            if (!cfg.fencer or cfg.fencer_preempts_keep_hold) self.keep_hold = true;
        }

        /// The manager's §5.9 recovery of pair `v`, step `s`: the death
        /// preempt, Recovering, the re-fence, the tail scan,
        /// `releaseRecovered` (Free, the identity KEPT).
        fn recovery(self: *Self, v: u8, s: u8) void {
            if (s == 0) {
                if (!self.ownLive(v)) {
                    self.spared[v] = true;
                    return;
                }
                self.preempt(v);
                return;
            }
            if (self.spared[v]) return;
            switch (s) {
                1 => self.state[v] = .recovering,
                // The re-fence: the key registered again is preempted again
                // (never after an ignored holder preempt on nvmet).
                2 => if (cfg.refence and v != W and self.reg_b) self.preempt(v),
                3 => {
                    self.scanned[v] = true;
                    self.state[v] = .recovered;
                },
                4 => self.state[v] = .free,
                else => unreachable,
            }
        }

        /// The manager's clean unmount: its fencer unregistered (the last
        /// user); the hold kept when a preempt landed under it, else released
        /// (`host_fence.leaveRegistration`).
        fn unmount(self: *Self) void {
            self.reg_fa = false;
            if (!self.keep_hold) self.reserved = false;
        }

        /// X's retirement covers its page pair at term 1; its page is at
        /// term 2 (the term-less weakening covers the pair at any term).
        fn retired(v: u8, pair: u8) bool {
            return v == X and pair == X and !cfg.retire_term;
        }

        /// `judgeRegion` for member `v`'s line (host B's registry: X, Y).
        fn stale(self: *const Self, v: u8) bool {
            const ln = self.line[v];
            if (ln == .none) return false;
            if (cfg.aid_on_line) {
                if (self.ownLive(v)) return false;
                if (cfg.leave_free_exception and ln == .l and self.owner[v] == v and self.state[v] == .free) return false;
                return !retired(v, v);
            }
            // Round 4: the pages naming its writer id (none → joining).
            var uncovered = false;
            for (0..PAIRS) |i| {
                if (self.owner[i] != v) continue;
                if (self.state[i] == .live) return false;
                if (cfg.leave_free_exception and ln == .l and self.state[i] == .free) continue;
                if (!retired(v, @intCast(i))) uncovered = true;
            }
            return uncovered;
        }

        fn anyStaleAlive(self: *const Self) bool {
            for (0..2) |v| if (self.alive[v] and self.stale(@intCast(v))) return true;
            return false;
        }

        fn killStale(self: *Self) void {
            for (0..2) |v| if (self.alive[v] and self.stale(@intCast(v))) {
                if (self.ownLive(@intCast(v))) self.killed_live = true;
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
                    self.fenced_b = false;
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
            if (self.fencer_fenced) return error.FencerFenced;
            if (self.manager_unreserved) return error.ManagerUnreserved;
            if (self.fenced_b and (self.reg_b or self.reg_fb)) return error.FencedHostRegistered;
            if (self.wrote_after_scan) return error.FencedMemberWrote;
            if (self.fencer_wrote) return error.FencerWrote;
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
        for (report.schedule, 0..) |t, i| {
            m.step(t);
            m.invariant() catch |e| {
                std.debug.print("  schedule (threads): {any}\n", .{report.schedule[0 .. i + 1]});
                return e;
            };
        }
        return error.NoInvariantNamed;
    };
    return null;
}

test "model: the election as built (WERO, the claim probed before the registration, again right before the hold and again under it — a writer this open's preempt fenced taken over —, the rule judging a manager by the report then appender 0's page with `p` retired at enrolment, the re-judge, the lapse's claim check, a hold taken by a preempt kept at the last clean unmount, the departure that keeps our hold; every manager's fencing registrant under its host's key, the superseded manager's resumed fencer preempt under the fencer's lock after a report shows its host holding, the lapse's re-acquire as separate commands with the fencer taken back when its host does not hold) holds — no current manager killed or preempted live, no write over a live claim, outside its own hold or from a view a later manager's acked metadata superseded, no fenced write, no fencer preempt while its host does not hold, no fencer holding, one manager, never unreserved under the current manager" {
    try explorer.expectHolds(Election(.{}), limits);
    const r = try statesOf(Election(.{}));
    std.debug.print("\nhost fence model (election): {d} states explored, depth {d}\n", .{ r.states, r.max_depth_seen });
}

test "model: a member's fence as built under spec preempt semantics (the death preempt and the re-fence through the manager's fencing registrant under its host's key, the own-key guard, a joined member's line naming its appender id and term, the term-keyed retirement, the `l` line until the leave's last write, the re-judge, the §DF.12 re-fence, the hold kept at the manager's unmount; recovered pages released and reused) holds — no Live member killed, no fencer fenced, the manager's namespace never unreserved by a member preempt, no fenced host registered, no evicted member writes after its tail scan, no write under a fencer's identity" {
    try explorer.expectHolds(Member(.{}), limits);
    const r = try statesOf(Member(.{}));
    std.debug.print("\nhost fence model (member, spec): {d} states explored, depth {d}\n", .{ r.states, r.max_depth_seen });
}

test "model: a member's fence as built under nvmet 7.3 semantics (decision 0003 §DG): the manager's fencing registrant — a non-holder under its host's key — preempts the dead member's host key, which the HOLDER's preempt cannot (nvmet_pr_preempt's holder == reg arm); a fenced member never writes" {
    try explorer.expectHolds(Member(.{ .semantics = .nvmet }), limits);
    const r = try statesOf(Member(.{ .semantics = .nvmet }));
    std.debug.print("\nhost fence model (member, nvmet 7.3 + fencer): {d} states explored, depth {d}\n", .{ r.states, r.max_depth_seen });
}

test "model weakening (§DF.11, the no-fencer posture): on nvmet 7.3 the holder's death preempt fences nothing — an evicted member resumed after its tail scan writes; on a spec-conformant target the holder's preempt alone holds" {
    try explorer.expectFails(Member(.{ .semantics = .nvmet, .fencer = false }), limits, error.InvariantViolated);
    try testing.expectEqual(@as(?anyerror, error.FencedMemberWrote), firstError(Member(.{ .semantics = .nvmet, .fencer = false })));
    try explorer.expectHolds(Member(.{ .fencer = false }), limits);
}

test "model weakening (§DG.2): a fencer under a key of its own outlives its host's fence — the death preempt lands and the fenced host's fencer stays registered; under multipath=Y its path carries the fenced host's writes; a superseded manager's fencer, resumed, takes the reservation from the successor that preempted it" {
    try explorer.expectFails(Member(.{ .semantics = .nvmet, .fencer_key = .distinct }), limits, error.InvariantViolated);
    try explorer.expectFails(Member(.{ .semantics = .nvmet, .fencer_key = .distinct, .merged_path = true }), limits, error.InvariantViolated);
    try explorer.expectFails(Election(.{ .fencer_key = .distinct }), limits, error.InvariantViolated);
}

test "model, an accepted instant (§DG.13(h)): the lapse's claim probe and its re-acquire are separate commands — a successor binding between them leaves the resumed manager re-acquiring on a stale view (OverwroteAcked); the claim's TTL and the lapse class bound it" {
    try explorer.expectFails(Election(.{ .lapse_acquire_split = true }), limits, error.InvariantViolated);
}

test "model weakening (review M1 of §DG): without the hold checks a fencer re-registered after a successor preempted its host's key (the lapse's re-acquire, the fencer's re-register as separate commands) preempts while its host does not hold — and takes the successor's reservation; without the fencer's lock the recovery's check and preempt straddle the takeover and the re-register" {
    try explorer.expectFails(Election(.{ .fencer_hold_check = false }), limits, error.InvariantViolated);
    try testing.expectEqual(@as(?anyerror, error.FencerPreemptUnheld), firstError(Election(.{ .fencer_hold_check = false })));
    try explorer.expectFails(Election(.{ .fencer_lock = false }), limits, error.InvariantViolated);
    try testing.expectEqual(@as(?anyerror, error.FencerPreemptUnheld), firstError(Election(.{ .fencer_lock = false })));
}

test "model: under multipath=Y a fencer under its host's key is fenced with the host — a write routed through its path is refused (an availability cost, never a breach)" {
    try explorer.expectHolds(Member(.{ .semantics = .nvmet, .merged_path = true }), limits);
}

test "model weakening (§DG.4): no own-key guard — the fencer's preempt of its own host's key unregisters the holder (unreserved) and the fencer itself (nvmet does not skip the sender)" {
    try explorer.expectFails(Member(.{ .semantics = .nvmet, .own_key_guard = false }), limits, error.InvariantViolated);
    try testing.expectEqual(@as(?anyerror, error.FencerFenced), firstError(Member(.{ .semantics = .nvmet, .own_key_guard = false })));
}

test "model weakening (§DG.5): the manager's unmount counts only its own client's preempts — after the fencer's death preempt the hold is released, the namespace unreserved, and the evicted member writes" {
    try explorer.expectFails(Member(.{ .semantics = .nvmet, .fencer_preempts_keep_hold = false }), limits, error.InvariantViolated);
    try testing.expectEqual(@as(?anyerror, error.FencedMemberWrote), firstError(Member(.{ .semantics = .nvmet, .fencer_preempts_keep_hold = false })));
}

test "model, the accepted windows are reachable: a PTPL-less lapse, a rule's register before its re-judge" {
    try explorer.expectFails(Election(.{ .strict = .{ .lapse = true } }), limits, error.InvariantViolated);
    try testing.expectEqual(@as(?anyerror, error.RegisterWindow), firstError(Member(.{ .strict = .{ .register_window = true } })));
}

test "model, today's code (review 4 blocker 2): the hold released at a takeover successor's clean last unmount — a frozen superseded manager's checkpoint, or an open paused across the takeover, overwrites the successor's acked metadata" {
    try explorer.expectFails(Election(.{ .keep_preempted_hold = false }), limits, error.InvariantViolated);
    try testing.expectEqual(@as(?anyerror, error.OverwroteAcked), firstError(Election(.{ .keep_preempted_hold = false, .structural = false })));
    try testing.expectEqual(@as(?anyerror, error.OverwroteAcked), firstError(Election(.{ .keep_preempted_hold = false, .structural = false, .p2 = false })));
}

test "model, today's code (review 4 blockers 1 and 1b): a joined member's line without its appender id (a released recovered pair reused by a new joiner names no one: joining, spared) and the `l`-line Free exception (a released recovery's Free page spared)" {
    try explorer.expectFails(Member(.{ .aid_on_line = false, .leave_free_exception = true }), limits, error.InvariantViolated);
    try explorer.expectFails(Member(.{ .aid_on_line = false }), limits, error.InvariantViolated);
    try explorer.expectFails(Member(.{ .leave_free_exception = true }), limits, error.InvariantViolated);
}

test "model, today's code (review 5 major 1): the hold on the first probe's reading — a manager that bound during the rule is preempted live" {
    try explorer.expectFails(Election(.{ .probe_before_hold = false }), limits, error.InvariantViolated);
    try testing.expectEqual(@as(?anyerror, error.PreemptedLive), firstError(Election(.{ .probe_before_hold = false })));
}

test "model: the second probe that refuses a writer this open's preempt fenced (round 4) is safe too — review 4 minor 4 is availability, not safety" {
    try explorer.expectHolds(Election(.{ .reprobe_proceeds = false }), limits);
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

test "model weakening (review round 3 major 3): the lapse re-acquired without the device's claim check — the superseded manager wins the hold back" {
    try explorer.expectFails(Election(.{ .lapse_claim_check = false }), limits, error.InvariantViolated);
}

test "model weakening: the departure that unregisters the host when no OTHER entry stands, holder or not, the manager enrolled only at its bind (review B3)" {
    try explorer.expectFails(Election(.{ .depart_checks_holder = false, .early_enrol = false }), limits, error.InvariantViolated);
}

test "model weakening: the release → preempt → re-acquire death preempt (the first §DF.11 wiring)" {
    try explorer.expectFails(Election(.{ .release_cycle = true }), limits, error.InvariantViolated);
}

test "model weakening (review B1, round 3 major 1): the retirement mark without the page term spares an evicted re-joined member (spec and nvmet + fencer)" {
    try explorer.expectFails(Member(.{ .retire_term = false }), limits, error.InvariantViolated);
    try explorer.expectFails(Member(.{ .semantics = .nvmet, .retire_term = false }), limits, error.InvariantViolated);
}

test "model weakening (review round 3 major 4): a leaving member's line dropped before its leave's writes — the rule cannot see it (spec and nvmet + fencer)" {
    try explorer.expectFails(Member(.{ .leaving_line = false }), limits, error.InvariantViolated);
    try explorer.expectFails(Member(.{ .semantics = .nvmet, .leaving_line = false }), limits, error.InvariantViolated);
}

test "model weakening: no §DF.12 re-fence once the page is Recovering (spec and nvmet + fencer)" {
    try explorer.expectFails(Member(.{ .refence = false }), limits, error.InvariantViolated);
    try explorer.expectFails(Member(.{ .semantics = .nvmet, .refence = false }), limits, error.InvariantViolated);
}

test "model weakening (review B2): no re-judge after the rule's register (spec and nvmet + fencer)" {
    try explorer.expectFails(Member(.{ .rejudge = false }), limits, error.InvariantViolated);
    try explorer.expectFails(Member(.{ .semantics = .nvmet, .rejudge = false }), limits, error.InvariantViolated);
}

test "model weakening (review 4 blockers 1 and 1b) under nvmet + fencer: a joined line without its appender id, the `l`-line Free exception" {
    try explorer.expectFails(Member(.{ .semantics = .nvmet, .aid_on_line = false }), limits, error.InvariantViolated);
    try explorer.expectFails(Member(.{ .semantics = .nvmet, .leave_free_exception = true }), limits, error.InvariantViolated);
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
    try testing.expectEqual(@as(?anyerror, null), firstError(Member(.{ .semantics = .nvmet })));
    try expectBreaks("the hold released at a takeover's last unmount (blocker 2)", firstError(Election(.{ .keep_preempted_hold = false })), &.{ error.UnreservedUnderManager, error.OverwroteAcked });
    try expectBreaks("the hold released at a takeover's last unmount: the paused open (blocker 2)", firstError(Election(.{ .keep_preempted_hold = false, .structural = false })), &.{error.OverwroteAcked});
    try expectBreaks("the hold released at a takeover's last unmount: M0's checkpoint (blocker 2)", firstError(Election(.{ .keep_preempted_hold = false, .structural = false, .p2 = false })), &.{error.OverwroteAcked});
    try expectBreaks("no appender id on the line (blocker 1)", firstError(Member(.{ .aid_on_line = false })), &.{error.FencedMemberWrote});
    try expectBreaks("the l-line Free exception (blocker 1b)", firstError(Member(.{ .leave_free_exception = true })), &.{error.FencedMemberWrote});
    try expectBreaks("the hold on the first probe's reading (review 5 major 1)", firstError(Election(.{ .probe_before_hold = false })), &.{error.PreemptedLive});
    try expectBreaks("rtype 5 (N1)", firstError(Election(.{ .hold = .wear_registered_holds })), &.{ error.TwoManagers, error.WroteOverLiveClaim, error.OverwroteAcked });
    try expectBreaks("no rule", firstError(Election(.{ .rule = false })), &.{ error.FencedWrote, error.TwoManagers, error.WroteUnheld, error.OverwroteAcked, error.WroteOverLiveClaim });
    try expectBreaks("round 3's judgement (major 1)", firstError(Election(.{ .judge = .round3 })), &.{error.KilledCurrent});
    try expectBreaks("the page alone (major 1)", firstError(Election(.{ .judge = .page_only })), &.{ error.FencedWrote, error.TwoManagers, error.WroteUnheld, error.OverwroteAcked, error.WroteOverLiveClaim });
    try expectBreaks("register before the claim (major 2)", firstError(Election(.{ .claim_first = false })), &.{ error.WroteOverLiveClaim, error.WroteUnheld });
    try expectBreaks("no lapse claim check (major 3)", firstError(Election(.{ .lapse_claim_check = false })), &.{ error.FencedWrote, error.OverwroteAcked });
    try expectBreaks("B3 departure", firstError(Election(.{ .depart_checks_holder = false, .early_enrol = false })), &.{ error.UnreservedUnderManager, error.WroteOverLiveClaim, error.OverwroteAcked });
    try expectBreaks("release-cycle death preempt", firstError(Election(.{ .release_cycle = true })), &.{ error.UnreservedUnderManager, error.WroteOverLiveClaim, error.OverwroteAcked });
    try expectBreaks("term-less retirement (B1)", firstError(Member(.{ .retire_term = false })), &.{error.FencedMemberWrote});
    try expectBreaks("no leaving line (major 4)", firstError(Member(.{ .leaving_line = false })), &.{error.FencedMemberWrote});
    try expectBreaks("no re-fence (§DF.12)", firstError(Member(.{ .refence = false })), &.{error.FencedMemberWrote});
    try expectBreaks("no re-judge (B2)", firstError(Member(.{ .rejudge = false })), &.{error.FencedMemberWrote});
    try expectBreaks("nvmet 7.3 without the fencer (§DF.11)", firstError(Member(.{ .semantics = .nvmet, .fencer = false })), &.{error.FencedMemberWrote});
    try expectBreaks("a fencer key of its own (§DG.2)", firstError(Member(.{ .semantics = .nvmet, .fencer_key = .distinct })), &.{error.FencedHostRegistered});
    try expectBreaks("a fencer key of its own under multipath=Y (§DG.2)", firstError(Member(.{ .semantics = .nvmet, .fencer_key = .distinct, .merged_path = true })), &.{ error.FencerWrote, error.FencedHostRegistered, error.FencedMemberWrote });
    try expectBreaks("a fencer key of its own: the superseded manager's fencer (§DG.2)", firstError(Election(.{ .fencer_key = .distinct })), &.{ error.FencerPreemptUnheld, error.FencerHeld });
    try expectBreaks("no hold checks around the fencer (review M1)", firstError(Election(.{ .fencer_hold_check = false })), &.{error.FencerPreemptUnheld});
    try expectBreaks("no fencer lock (review M1)", firstError(Election(.{ .fencer_lock = false })), &.{error.FencerPreemptUnheld});
    try expectBreaks("no own-key guard (§DG.4)", firstError(Member(.{ .semantics = .nvmet, .own_key_guard = false })), &.{ error.FencerFenced, error.ManagerUnreserved });
    try expectBreaks("the fencer's preempts not keeping the hold (§DG.5)", firstError(Member(.{ .semantics = .nvmet, .fencer_preempts_keep_hold = false })), &.{error.FencedMemberWrote});
    try expectBreaks("no re-fence under nvmet + fencer", firstError(Member(.{ .semantics = .nvmet, .refence = false })), &.{error.FencedMemberWrote});
    try expectBreaks("no re-judge under nvmet + fencer", firstError(Member(.{ .semantics = .nvmet, .rejudge = false })), &.{error.FencedMemberWrote});
    try expectBreaks("term-less retirement under nvmet + fencer", firstError(Member(.{ .semantics = .nvmet, .retire_term = false })), &.{error.FencedMemberWrote});
    try expectBreaks("no leaving line under nvmet + fencer", firstError(Member(.{ .semantics = .nvmet, .leaving_line = false })), &.{error.FencedMemberWrote});
}
