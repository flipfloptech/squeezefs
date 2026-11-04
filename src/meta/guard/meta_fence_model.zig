//! The MODELS of metadata-namespace PR fencing under Write Exclusive – All
//! Registrants, with single-manager exclusion by the successor's
//! preempt-all (decision 0003 §DO; decision 0004, the design, §7). Written
//! against the design's device facts (nvmet 7.3.1 `pr.c`, P1–P17) and its
//! mechanisms (§4); dependency-free, one atomic action per `step`, explored
//! exhaustively by `src/model/explorer.zig` with its opt-in quiescence and
//! cycle checks (every model here declares `stuck()`).
//!
//! Device semantics, pure functions of the state: `register` (P1), `acquire`
//! (P3, P6), `preemptAll` = Preempt-and-Abort PRKEY 0 RTYPE 5 (P4 + P9: per
//! victim the ref kill and the unregister are two steps, the drain an
//! enabling condition of its completion), `fence(k)` = Preempt-and-Abort
//! PRKEY k (P5, P8, P9), `unregister` (P10: the reservation ends with its
//! last registrant), a target restart (P11), `drainBarrier` (P15, P15a) and
//! the generation (P16). A victim's write after its unregistration is
//! refused (EBADE, P9a), its read during the drain gets INTERNAL → EIO (P9b),
//! a write between its kill and its unregister gets INTERNAL (P9c). Write
//! EIOs are transient (§7.1): a write meets EIO only at a P9c instant. In
//! `Succession` (revision 16): the reservation's holder (set by an acquire
//! or a preempt-all, handed to the first remaining registrant when the
//! holder leaves, cleared by a restart; only an acquire's answer shows it)
//! and host A's registration lock (DC20, revision 17: the manager's page
//! check, `admittedHere` and command, and every non-blind rule from its
//! register to its re-judge; a rule's read that finds the host registered
//! adopts, and its register re-judges M0 and kills it if stale, DC24; a
//! stale verdict releases the lock before the drain, revision 18). The
//! register's re-read is atomic with it except in `.split_register`, the
//! product's two commands with DC24's mark clause, M0's grace report (G4)
//! and its acquire's mark under the lock.
//!
//! Invariants, checked in §7.5.2's order (`checkRows`): `KilledCurrent`,
//! `PreemptedLive`, `WroteOverLiveClaim`, `TwoManagers`,
//! `UnreservedUnderManager`, `OverwroteAcked`, `FencedWrote`,
//! `FencedHostRegistered`, `FencedMemberWrote`, `ReallocatedUnfenced`,
//! `StaleDataWrite`, `ReconcileFencedLive`, `LiveFenced` (step part),
//! `ClearedAfterEstablish`, `ManagerFailStoppedUnproven`,
//! `RegisteredButPending`, `PendingPastDeadline`, then the accepted windows'
//! hits under `Strict`. `final`: `LiveFenced` (final part), `RecoveryStuck`.
//! `stuck`: `StrandedAtQuiescence`. Flags are sticky: each is set at the step
//! that commits its violation. Accepted-window hits replace rows 4, 6, 7, 9
//! and 11 (A3, A4, A5, A6, A9; A2 for data), and rows 4, 6 and 7 count
//! metadata writes only.
//!
//! Rule L's environment (§4.5.7): `tick` (a budgeted clock event, then a
//! quiescence tick that runs only where no other thread's step changes the
//! state), `elapsed(w)` (one-shot, bound to the pending generation it fired
//! in) and the per-word unreadable read (at most one per word per run) are
//! threads whose steps no weakening removes; a weakening removes only a
//! reaction. A nondeterministic outcome (an unreadable read, a barrier
//! answering unknown) is an ALTERNATIVE thread, done unless the step it
//! replaces is the next step of its thread, so the branch exists only where
//! it can be taken.
//!
//! Families: `Death`, `Succession`, `Window` (with `WindowLateClear`,
//! `WindowEvictedAfterSibling`, `WindowLapseEvicted`, `WindowDataStall` for
//! the `Window` configurations whose threads differ) and `Debt`.
//!
//! Definition pins (decision 0004 §7.5.2's M1–M8, adopted in revision 16):
//!   - M1: A3 is per incarnation: a target restart happened and that
//!     incarnation has not acquired since (as `host_fence_model.Election`).
//!   - M2: row 4's state part skips a host inside a rule's register window
//!     (A5).
//!   - M3: row 15's proof is a successor's takeover reaching the manager's
//!     host: its preempt-all, its acquire of a namespace a restart left
//!     without that host, or the acquire the manager's lapse re-acquire
//!     conflicts with (DC21).
//!   - M4: "blocked" is asleep in the wait with a false predicate; a
//!     readable tick anchors `RegisteredButPending` only while no
//!     resolution step of the word is in flight (an establishing arm
//!     between its register and completion; a `pending` store before its
//!     store-then-recheck; a kicked beat with work).
//!   - M5: an establishing arm stores `cleared` once it sees its host
//!     registered (§4.5.4); a registration removed meanwhile leaves the
//!     word to the next arm.
//!   - M6: a host's page reads fail during a drain of that host (P9b).
//!   - M7: the retry budget is one retry per takeover (by successor) and
//!     per restart that unregistered the member's host (§4.5.3).
//!   - M8: a latch caused by a death fence of the member's own host (its
//!     own eviction, a co-located sibling's death, Q4) is that fail-stop,
//!     not `LiveFenced`.
//! A withdrawal of a process's fresh registration waits for that process's
//! own admitted commands, up to DC22's bound (the one-shot
//! `WITHDRAW_TIMEOUT` alternative: the registration stays and the
//! incarnation latches): `WindowLateClear`'s J.beat. Elsewhere no metadata
//! withdrawal runs beside an admitted write of its own process (one
//! thread, or a terminal word latching first), and the data arms never
//! withdraw (DC23).

const std = @import("std");
const explorer = @import("../../model/explorer.zig");

const A: u8 = 0;
const B: u8 = 1;
const C: u8 = 2;
const D: u8 = 3;
const NONE: u8 = 0xff;

/// The accepted windows made violations (each shown reachable).
pub const Strict = struct {
    /// A3: a write in the namespace a target restart left unreserved, before
    /// the lapse's re-acquire.
    lapse: bool = false,
    /// A5: a write admitted between a rule's register and its re-judge, by
    /// the incarnation the re-judge then kills or withdraws for.
    register_window: bool = false,
    /// A4: a write admitted under a registration a rule made after reading
    /// the dead member's page Live before step 2, landing before C4's
    /// re-fence.
    readmit_window: bool = false,
    /// A6: an open's preempt-all of a manager whose claim became fresh after
    /// the open's probe 2.
    probe_instant: bool = false,
    /// A2: the superseded manager's data write landing before the debt is
    /// paid.
    debt_window: bool = false,
    /// A9: a superseded manager's write after a blind rule re-admitted its
    /// host.
    blind_window: bool = false,
};

/// The reservation type of the metadata namespace.
pub const MetaRtype = enum {
    /// Write Exclusive – All Registrants (this design).
    wear,
    /// WERO held by the manager, its own death preempt — nvmet 7.3's holder
    /// arm re-types its own hold and fences nothing (no fencer; W14).
    wero_holder,
};

const Wr = enum(u8) { idle, queued, admitted, landed, refused, none };

/// Every invariant row's verdict at one state, in §7.5.2's order.
const Rows = struct {
    killed_current: bool = false,
    preempted_live: bool = false,
    wrote_over_live: bool = false,
    two_managers: bool = false,
    unreserved_under_manager: bool = false,
    overwrote_acked: bool = false,
    fenced_wrote: bool = false,
    fenced_host_registered: bool = false,
    fenced_member_wrote: bool = false,
    realloc_unfenced: bool = false,
    stale_data_write: bool = false,
    reconcile_fenced_live: bool = false,
    live_fenced: bool = false,
    cleared_after_establish: bool = false,
    mgr_failstop_unproven: bool = false,
    registered_but_pending: bool = false,
    pending_past_deadline: bool = false,
    hit_lapse: bool = false,
    hit_register: bool = false,
    hit_readmit: bool = false,
    hit_probe: bool = false,
    hit_debt: bool = false,
    hit_blind: bool = false,
};

fn checkRows(r: Rows, strict: Strict) !void {
    if (r.killed_current) return error.KilledCurrent;
    if (r.preempted_live) return error.PreemptedLive;
    if (r.wrote_over_live) return error.WroteOverLiveClaim;
    if (r.two_managers) return error.TwoManagers;
    if (r.unreserved_under_manager) return error.UnreservedUnderManager;
    if (r.overwrote_acked) return error.OverwroteAcked;
    if (r.fenced_wrote) return error.FencedWrote;
    if (r.fenced_host_registered) return error.FencedHostRegistered;
    if (r.fenced_member_wrote) return error.FencedMemberWrote;
    if (r.realloc_unfenced) return error.ReallocatedUnfenced;
    if (r.stale_data_write) return error.StaleDataWrite;
    if (r.reconcile_fenced_live) return error.ReconcileFencedLive;
    if (r.live_fenced) return error.LiveFenced;
    if (r.cleared_after_establish) return error.ClearedAfterEstablish;
    if (r.mgr_failstop_unproven) return error.ManagerFailStoppedUnproven;
    if (r.registered_but_pending) return error.RegisteredButPending;
    if (r.pending_past_deadline) return error.PendingPastDeadline;
    if (strict.lapse and r.hit_lapse) return error.LapseWindow;
    if (strict.register_window and r.hit_register) return error.RegisterWindow;
    if (strict.readmit_window and r.hit_readmit) return error.ReadmitWindow;
    if (strict.probe_instant and r.hit_probe) return error.ProbeInstant;
    if (strict.debt_window and r.hit_debt) return error.DebtWindow;
    if (strict.blind_window and r.hit_blind) return error.BlindWindow;
}

// ---------------------------------------------------------------------------
// `Death` — a member's death under WEAR, the manager's own fence(k), no
// fencer (replaces `host_fence_model.Member`).
// ---------------------------------------------------------------------------

pub const DeathConfig = struct {
    /// W, the member co-located with MG on host A, dies with a metadata write
    /// in flight: the own-key guard withholds the preempt, its recoverer on
    /// the same host kills it by the rule and drains its queue before the
    /// release.
    colocated: bool = false,
    /// Z claims a key other than its host's at its JoinAppender, freezes with
    /// a write and dies; its death record carries no key, so the page's is
    /// the fence's (round-2 issue 6).
    joiner_key: bool = false,
    /// A sibling's registration runs the stale-sibling rule (W5: false).
    rule: bool = true,
    /// W14: WERO held by the manager, no fencer.
    meta_rtype: MetaRtype = .wear,
    /// The death fence of the manager's own key withheld (W15: false).
    own_key_guard: bool = true,
    /// C4: the re-fence after the page went Recovering (W16: false).
    refence: bool = true,
    /// The rule's re-judge after its register (W16: false; the step stays as
    /// A5's closing marker).
    rejudge: bool = true,
    /// The manager verifies a joiner's claimed key against the report at
    /// JoinAppender (W30: false).
    join_key_verify: bool = true,
    strict: Strict = .{},
};

const Page = enum(u8) { live, recovering, recovered, free };
const MLine = enum(u8) { none, j, l };

/// MG's steps per recovered pair (§4.8): the eviction, the guard (the page
/// check and `admittedHere` under the sequence lock), the report first, the
/// fence, the report after, the page Recovering, C4's re-fence, the
/// recoverer's kill and drain (a co-located member), the tail scan, the
/// release, and MG's beat (page check; report).
const DStep = enum(u8) { evict, guard, report_first, fence, report_after, recovering, refence, kill, drain, scan, release, beat };
const D_STEPS: u8 = @typeInfo(DStep).@"enum".field_names.len;

/// `Death` — MG (host A) manages; host B carries X (a frozen member re-joined
/// in place once: its retirement `r` names appender 1 at term 1, its page is
/// appender 1 at term 2) and Y (leaving); host C carries a live member
/// throughout (its registration stands) and Z's JoinAppender:
///   0 X   one metadata write: issued while X lives (the resume), admitted or
///         refused at its arrival, landed.
///   1 Y   leaving: its line marked `l`, its leave's write (the page Free,
///         the identity cleared), the line removed.
///   2 MG  each member whose page is Live evicted in turn (§4.8): the key
///         from the record, or from the page when the record carries 0; the
///         own-key guard; the page check and `admittedHere`; the report
///         first (an absent key is fenced); `fence(k)` (P5 + P9: the victim's
///         admitted writes drained first); the report (a key still present:
///         the tripwire, `.failed`, a full tail scan); the page Recovering;
///         C4's re-fence; the tail scan; the release (Free, the pair
///         reusable); MG's beat after each recovery (WEAR without host A:
///         usurped, fail-stop); then MG's clean unmount (§4.12).
///   3 S   a joiner mounting on host B: the rule (read, judge, kill), the
///         data node drained, the register, the re-judge.
///   4 Z   a joiner on host C: JoinAppender takes the lowest Free pair
///         (`joiner_key`: its claimed key verified, then a frozen write and
///         its death, recovered by MG with the page's key).
///   5 W   (`colocated`) a metadata write of the member co-located with MG.
pub fn Death(comptime cfg: DeathConfig) type {
    return extern struct {
        const Self = @This();
        pub const thread_count: usize = 5 + @as(usize, @intFromBool(cfg.colocated));
        const X: u8 = 0;
        const Y: u8 = 1;
        const W: u8 = 2;
        /// Z's identity (pair index 3 is its own initially-Free pair under
        /// `joiner_key`).
        const Z: u8 = 3;
        const PAIRS = if (cfg.joiner_key) 4 else 3;
        /// The recoveries MG runs: X's, Y's and W's pairs, then (with
        /// `joiner_key`) the pair Z holds.
        const SLOTS = if (cfg.joiner_key) 4 else 3;
        const MG_UNMOUNT: u8 = SLOTS * D_STEPS;
        const MG_END = MG_UNMOUNT + 1;
        const S_END = 3;
        /// The key nobody holds (Z's wrong claim).
        const K_NOBODY: u8 = D;
        const T_W = 5;

        reserved: bool = true,
        /// Every host registered on the metadata namespace (host C's live
        /// member keeps C's registration throughout).
        reg: [3]bool = @splat(true),
        /// A death fence of host h landed; cleared by a rule's register of h.
        fenced: [3]bool = @splat(false),
        owner: [PAIRS]u8 = if (cfg.joiner_key) .{ X, Y, W, NONE } else .{ X, Y, W },
        state: [PAIRS]Page = if (cfg.joiner_key) .{ .live, .live, .live, .free } else @splat(.live),
        /// The host key each page carries (layout v2's `host_key`).
        page_key: [PAIRS]u8 = if (cfg.joiner_key) .{ B, B, A, NONE } else .{ B, B, A },
        scanned: [PAIRS]bool = @splat(false),
        line: [2]MLine = .{ .j, .j },
        x_alive: bool = true,
        y_alive: bool = true,
        y_failed: bool = false,
        z_alive: bool = true,
        w_alive: bool = true,
        xw: Wr = .idle,
        ww: Wr = if (cfg.colocated) .idle else .none,
        zw: Wr = if (cfg.joiner_key) .idle else .none,
        /// Where an X write was admitted: inside S's register window (A5),
        /// under S's registration that read X's page Live before step 2,
        /// before C4's re-fence (A4).
        xw_window: bool = false,
        xw_readmit: bool = false,
        /// MG's current recovery: its pair (NONE: spared), its key, the
        /// report-first verdict.
        cur_pair: u8 = NONE,
        cur_key: u8 = NONE,
        cur_fenced: bool = false,
        /// C4's re-fence of X's pair ran (A4 closes).
        x_refenced: bool = false,
        mg_failed: bool = false,
        mg_left: bool = false,
        pc: [6]u8 = @splat(0),
        /// S holds host B's registry lock (Y's line changes wait for it).
        locked: bool = false,
        window: bool = false,
        /// S's rule read X's page Live (before step 2).
        s_read_live: bool = false,
        /// S's registration stands, made after that read.
        readmit: bool = false,
        killed_current: bool = false,
        fhr: bool = false,
        fenced_member_wrote: bool = false,
        mfsu: bool = false,
        hit_register: bool = false,
        hit_readmit: bool = false,

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                0 => self.pc[0] >= 3,
                1 => self.pc[1] >= 3 or !self.y_alive or self.y_failed,
                2 => self.pc[2] >= MG_END or self.mg_failed,
                3 => self.pc[3] >= S_END,
                4 => self.pc[4] >= 4,
                T_W => self.pc[T_W] >= 3,
                else => unreachable,
            };
        }

        fn admits(self: *const Self, h: u8) bool {
            return !self.reserved or self.reg[h];
        }

        fn ownLive(self: *const Self, v: u8) bool {
            return self.owner[v] == v and self.state[v] == .live;
        }

        fn hostOfKey(k: u8) u8 {
            return if (k < 3) k else NONE;
        }

        /// A frozen member's write on host `h`, at `pc` from `base`: issue
        /// (while alive), arrival (admitted or refused), landing (after its
        /// pair's tail scan: a fenced member wrote, outside the accepted
        /// windows). Ends at `base + 3`.
        fn write(self: *Self, pc: *u8, base: u8, w: *Wr, alive: bool, h: u8, v: u8) void {
            switch (pc.* - base) {
                0 => if (!alive) {
                    w.* = .none;
                    pc.* = base + 3;
                    return;
                } else {
                    w.* = .queued;
                },
                1 => if (self.admits(h)) {
                    w.* = .admitted;
                    if (v == X) {
                        self.xw_window = self.window;
                        self.xw_readmit = self.readmit and !self.x_refenced;
                    }
                } else {
                    w.* = .refused;
                    pc.* = base + 3;
                    return;
                },
                2 => {
                    w.* = .landed;
                    const in_readmit = v == X and self.xw_readmit and !self.x_refenced;
                    const p = self.pairOf(v);
                    if (v == X and self.xw_window) {
                        if (p != NONE and self.scanned[p]) self.hit_register = true;
                    } else if (in_readmit) {
                        self.hit_readmit = true;
                    } else if (p != NONE and self.scanned[p]) self.fenced_member_wrote = true;
                },
                else => unreachable,
            }
            pc.* += 1;
        }

        /// The pair member `v` writes into (Z: the pair it took).
        fn pairOf(self: *const Self, v: u8) u8 {
            if (v != Z) return v;
            for (0..PAIRS) |i| if (self.owner[i] == Z) return @intCast(i);
            return NONE;
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                0 => self.write(&self.pc[0], 0, &self.xw, self.x_alive, B, X),
                1 => self.leaver(),
                2 => self.manager(),
                3 => self.sibling(),
                4 => self.joiner(),
                T_W => self.write(&self.pc[T_W], 0, &self.ww, self.w_alive, A, W),
                else => unreachable,
            }
        }

        fn leaver(self: *Self) void {
            if (self.pc[1] != 1 and self.locked) return;
            switch (self.pc[1]) {
                0 => self.line[Y] = .l,
                // The leave's write: the page Free, the identity cleared
                // (`LeaveAppender`); refused, Y latches.
                1 => if (!self.admits(B)) {
                    self.y_failed = true;
                    return;
                } else if (self.ownLive(Y)) {
                    self.state[Y] = .free;
                    self.owner[Y] = NONE;
                },
                2 => self.line[Y] = .none,
                else => unreachable,
            }
            self.pc[1] += 1;
        }

        fn joiner(self: *Self) void {
            switch (self.pc[4]) {
                0 => {
                    // JoinAppender: the lowest Free pair, the claimed key
                    // verified against the report (`hostKey(registrant)`).
                    const claimed: u8 = if (cfg.joiner_key) K_NOBODY else C;
                    if (cfg.join_key_verify and claimed != C) {
                        self.z_alive = false;
                        self.pc[4] = 4;
                        return;
                    }
                    for (0..PAIRS) |i| if (self.state[i] == .free) {
                        self.owner[i] = Z;
                        self.state[i] = .live;
                        self.page_key[i] = claimed;
                        break;
                    } else {
                        // No Free pair yet: JoinAppender waits.
                        return;
                    };
                    if (!cfg.joiner_key) {
                        self.pc[4] = 4;
                        return;
                    }
                    self.pc[4] = 1;
                },
                // Z's frozen write (issue, arrival, landing).
                1, 2, 3 => self.write(&self.pc[4], 1, &self.zw, self.z_alive, C, Z),
                else => unreachable,
            }
        }

        /// MG's death fence of key `k` (P5 + P9): every registrant of the key
        /// unregistered, the sender included, the reservation kept unless the
        /// last registrant went (P10); waits while the victim has a write
        /// admitted. The sender must be registered (P8). False: waiting.
        fn fence(self: *Self, k: u8) bool {
            const h = hostOfKey(k);
            if (h == NONE or !self.reg[A]) return true;
            if (self.victimAdmitted(h)) return false;
            if (cfg.meta_rtype == .wero_holder) return true; // P7: re-types its own hold
            if (!self.reg[h]) return true;
            self.reg[h] = false;
            self.fenced[h] = true;
            if (!self.reg[A] and !self.reg[B] and !self.reg[C]) self.reserved = false;
            return true;
        }

        fn victimAdmitted(self: *const Self, h: u8) bool {
            return switch (h) {
                A => self.ww == .admitted,
                B => self.xw == .admitted,
                C => self.zw == .admitted,
                else => false,
            };
        }

        fn manager(self: *Self) void {
            if (self.pc[2] == MG_UNMOUNT) {
                self.unmount();
                self.pc[2] += 1;
                return;
            }
            const slot: u8 = @intCast(self.pc[2] / D_STEPS);
            const s: DStep = @fromBackingInt(@intCast(self.pc[2] % D_STEPS));
            switch (s) {
                .evict => {
                    const p: u8 = if (slot < 3) slot else self.pairOf(Z);
                    const evicts = if (slot < 3) self.ownLive(slot) else (p != NONE and self.state[p] == .live);
                    if (!evicts) {
                        self.cur_pair = NONE;
                        self.pc[2] = (slot + 1) * D_STEPS;
                        return;
                    }
                    self.cur_pair = p;
                    // The record's key; Z's record carries 0, so the page's
                    // is used (X's, Y's and W's records equal their pages').
                    self.cur_key = self.page_key[p];
                    self.cur_fenced = false;
                },
                // The page check reads ours (no successor); `admittedHere`
                // must find host A registered under the reservation, else the
                // recovery fails and re-runs at the next poll (only a fence of
                // A's own key unregisters it, and MG's beat right after that
                // recovery fail-stops first).
                .guard => if (!self.reg[A]) return,
                .report_first => {
                    if (self.ownKey()) {
                        self.pc[2] = slot * D_STEPS + @backingInt(DStep.recovering);
                        return;
                    }
                    const h = hostOfKey(self.cur_key);
                    self.cur_fenced = h == NONE or !self.reg[h];
                },
                .fence => if (!self.cur_fenced) {
                    if (!self.fence(self.cur_key)) return;
                },
                // The report after: a key still present is the tripwire and
                // `.failed` (a full tail scan follows).
                .report_after => {},
                .recovering => self.state[self.cur_pair] = .recovering,
                .refence => {
                    if (cfg.refence and !self.ownKey()) {
                        if (!self.fence(self.cur_key)) return;
                    }
                    if (self.cur_pair == X) self.x_refenced = true;
                },
                .kill => if (self.cur_pair == W and cfg.colocated and self.w_alive) {
                    // The recoverer on the same host kills W by the rule.
                    if (self.ownLive(W)) self.killed_current = true;
                    self.w_alive = false;
                },
                .drain => if (self.cur_pair == W and cfg.colocated and (self.ww == .queued or self.ww == .admitted)) return,
                .scan => {
                    self.scanned[self.cur_pair] = true;
                    self.state[self.cur_pair] = .recovered;
                },
                .release => self.state[self.cur_pair] = .free,
                // MG's beat: WEAR without host A registered reads usurped.
                .beat => if (self.reserved and !self.reg[A]) {
                    self.mg_failed = true;
                    self.noteFailStop();
                    return;
                },
            }
            self.pc[2] += 1;
        }

        /// The recovery's key is the manager's own (the own-key guard).
        fn ownKey(self: *const Self) bool {
            return cfg.own_key_guard and self.cur_key == A;
        }

        /// MG fail-stops: no successor's preempt-all exists here and its page
        /// never mismatches, so every fail-stop is unproven.
        fn noteFailStop(self: *Self) void {
            if (!self.mg_left) self.mfsu = true;
        }

        /// MG's clean unmount (§4.12): another host registered ⇒ unregister;
        /// the last registrant keeps its registration (it fenced someone).
        fn unmount(self: *Self) void {
            self.mg_left = true;
            if (!self.reg[A]) return;
            if (self.reg[B] or self.reg[C]) self.reg[A] = false;
        }

        /// `judgeRegion` for member `v`'s line on host B: stale unless its
        /// page is Live under its writer id and term, or retired (X's
        /// retirement covers appender 1 at term 1 only).
        fn stale(self: *const Self, v: u8) bool {
            if (self.line[v] == .none) return false;
            return !self.ownLive(v);
        }

        fn anyStaleAlive(self: *const Self) bool {
            return (self.x_alive and self.stale(X)) or (self.y_alive and self.stale(Y));
        }

        fn killStale(self: *Self) void {
            if (self.x_alive and self.stale(X)) {
                if (self.ownLive(X)) self.killed_current = true;
                self.x_alive = false;
            }
            if (self.y_alive and self.stale(Y)) {
                if (self.ownLive(Y)) self.killed_current = true;
                self.y_alive = false;
            }
        }

        /// S's admission on host B: the rule (read, judge, kill), the data
        /// node drained after a kill (C3), the register, the re-judge.
        fn sibling(self: *Self) void {
            switch (self.pc[3]) {
                0 => if (self.reg[B]) {
                    self.pc[3] = S_END;
                    return;
                } else {
                    self.locked = true;
                    self.s_read_live = self.ownLive(X);
                    if (cfg.rule) self.killStale();
                },
                1 => {
                    if (!self.x_alive and (self.xw == .queued or self.xw == .admitted)) return;
                    self.reg[B] = true;
                    if (cfg.rule) {
                        self.fenced[B] = false;
                        self.window = true;
                        self.readmit = self.s_read_live;
                    } else if (self.fenced[B]) self.fhr = true;
                },
                2 => {
                    self.window = false;
                    if (cfg.rule and cfg.rejudge and self.anyStaleAlive()) {
                        self.reg[B] = false;
                        self.readmit = false;
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
            var r: Rows = .{};
            r.killed_current = self.killed_current;
            r.fenced_host_registered = self.fhr;
            r.fenced_member_wrote = self.fenced_member_wrote;
            r.mgr_failstop_unproven = self.mfsu;
            r.hit_register = self.hit_register;
            r.hit_readmit = self.hit_readmit;
            return checkRows(r, cfg.strict);
        }

        pub fn final(self: *const Self) !void {
            // Every evicted pair recovered and released (the pairs MG passed
            // over stay as they were).
            if (self.mg_failed) return;
            if (self.pc[2] < MG_END) return error.RecoveryStuck;
        }

        pub fn stuck(_: *const Self) !void {
            return error.StrandedAtQuiescence;
        }
    };
}

// ---------------------------------------------------------------------------
// Rule L's words (§4.5.3, §4.5.7): a resolution word, its held waiter, and
// the per-word environment bookkeeping the liveness invariants read.
// ---------------------------------------------------------------------------

const WState = enum(u8) { pending, cleared, terminal };

/// A resolution word `{state, gen}`: `terminal` absorbing, every `cleared`
/// a new generation; with rule L's per-word environment.
const Word = extern struct {
    st: WState = .cleared,
    gen: u8 = 0,
    /// The terminal was the deadline reaction's (pin 1: the accepted end of
    /// a stall).
    by_deadline: bool = false,
    /// `elapsed(w)` fired (one-shot), bound to generation `el_gen`; a tick
    /// ran after it.
    el_fired: bool = false,
    el_gen: u8 = 0,
    el_ticked: bool = false,
    /// A readable tick met this word waiting (pending, or its waiter
    /// blocked) while its host was registered under a standing reservation
    /// and the incarnation current, and all of that has held since.
    rtick: bool = false,
    /// The word's one unreadable read was used (at most one per run).
    unread_used: bool = false,
    /// A resolution step of this word is in flight — an establishing arm
    /// between its register and its completion step, or a `pending` store
    /// before its store-then-recheck: a tick meeting it then is no anchor
    /// (the arm's `cleared` is `ClearedAfterEstablish`'s to check, the
    /// recheck is the storer's next step).
    arm_open: bool = false,
    recheck_due: bool = false,

    /// A readable tick meeting this word may anchor `RegisteredButPending`.
    fn anchors(w: *const Word) bool {
        return !w.arm_open and !w.recheck_due;
    }

    /// A CAS from any non-terminal state.
    fn pend(w: *Word) void {
        if (w.st != .terminal) w.st = .pending;
    }

    /// False: already terminal (the CAS refuses to leave it).
    fn clear(w: *Word) bool {
        if (w.st == .terminal) return false;
        w.st = .cleared;
        w.gen +%= 1;
        return true;
    }

    fn terminate(w: *Word, deadline: bool) void {
        if (w.st == .terminal) return;
        w.st = .terminal;
        w.by_deadline = deadline;
    }

    /// `elapsed(w)`: generation `gen`'s deadline passed.
    fn elapse(w: *Word) void {
        w.el_fired = true;
        w.el_gen = w.gen;
        w.el_ticked = false;
    }

    /// The deadline reaction applies: still pending at the generation
    /// `elapsed` was bound to.
    fn pastDeadline(w: *const Word) bool {
        return w.st == .pending and w.el_fired and w.el_gen == w.gen;
    }
};

/// A held operation's view of its word: the generation (and, for the
/// state-only weakening W35, the state) captured at its refusal.
const Waiter = extern struct {
    /// The held verdict was given and not yet acted on.
    held: bool = false,
    /// The capture ran (W37 moves it after the verdict).
    captured: bool = false,
    /// In its futex wait (past its pre-sleep re-check).
    sleeping: bool = false,
    cap_gen: u8 = 0,
    cap_st: WState = .cleared,

    fn capture(x: *Waiter, w: *const Word) void {
        x.captured = true;
        x.cap_gen = w.gen;
        x.cap_st = w.st;
    }

    fn reset(x: *Waiter) void {
        x.* = .{};
    }

    /// The wake predicate: terminal, or a `cleared` past the capture
    /// (`by_gen = false`, W35: the state alone compared — a `cleared` equal
    /// to the captured state never wakes).
    fn wakes(x: *const Waiter, w: *const Word, by_gen: bool) bool {
        if (w.st == .terminal) return true;
        if (by_gen) return w.gen != x.cap_gen;
        return w.st == .cleared and x.cap_st != .cleared;
    }

    /// Blocked: asleep in its wait with its wake predicate false (pin 2;
    /// an operation still running its hook's or its pre-sleep steps is not
    /// waiting yet).
    fn blocked(x: *const Waiter, w: *const Word, by_gen: bool) bool {
        return x.sleeping and !x.wakes(w, by_gen);
    }
};

/// `RegisteredButPending` for word `w`: waiting while its host is registered
/// and current, past a readable tick that met it so.
fn registeredButPending(w: *const Word, waiting: bool, reg_current: bool) bool {
    return waiting and reg_current and w.rtick;
}

/// `PendingPastDeadline` for word `w`: pending at the generation `elapsed`
/// was bound to, or a waiter blocked since then, after a later tick.
fn pendingPastDeadline(w: *const Word, x: ?*const Waiter, by_gen: bool) bool {
    if (!w.el_fired or !w.el_ticked) return false;
    if (w.st == .pending and w.gen == w.el_gen) return true;
    if (x) |waiter| {
        if (waiter.blocked(w, by_gen) and waiter.cap_gen <= w.el_gen) return true;
    }
    return false;
}

// ---------------------------------------------------------------------------
// `Succession` — the successor's open under WEAR: the claim's three probes,
// the preempt-all (the last one wins), the generation outcome, and the
// superseded manager's co-located joiner re-registering.
// ---------------------------------------------------------------------------

pub const Takeover = enum {
    /// Preempt-and-Abort PRKEY 0 RTYPE 5 (as built).
    preempt_all,
    /// W1: a preempt of the claim's key (N1 under WEAR).
    preempt_pred_key,
};
pub const Judge = enum {
    /// A manager line: stale while reserved and unregistered, by appender
    /// 0's page while unreserved (as built).
    report_then_page,
    /// W21: by the page alone.
    page_only,
};
pub const Budget = enum {
    /// One retry per takeover (by successor) and per restart that
    /// unregistered the host: §4.5.3's budget since revision 16 (M7).
    per_succession,
    /// W22: one retry in all.
    once,
    /// W48: revision 15's literal budget, one retry per change of
    /// `mgr_writer[v]` the member sees (and per lapse it sees).
    per_mgr_writer,
};
pub const RejudgeKill = enum {
    /// A stale verdict at the re-judge: the SIGKILL and the release of the
    /// registration lock in one step, the withdrawal a later step (the
    /// product's order, revision 19).
    with_release,
    /// W57: the release first, the kill one step later.
    after_release,
};
pub const LapseEnd = enum {
    /// The lapse ends only by M0's own acquire: a report showing WEAR before
    /// it stands M0 down (G4, revision 18).
    own_acquire,
    /// W56: revision 17's beat — WEAR with host A registered reads `.ours`
    /// and ends the lapse with no acquire.
    on_report,
};
pub const LapseConflict = enum {
    /// A lapse re-acquire that conflicts is a successor's takeover: the
    /// manager stands down (DC21, as built).
    stands_down,
    /// W50: revision 15's §4.9 step 4 — WEAR reported with this host
    /// registered counts as this manager's own re-acquire.
    counts_as_reacquire,
};
pub const OutcomeBy = enum { generation, r0_only };
pub const ReadEio = enum { barrier_retry, latch };
pub const GenCapture = enum { at_refusal, after_verdict };
pub const R1Unreadable = enum { stays_pending, terminal };

pub const SuccessionConfig = struct {
    // ---- configurations (§7.2) ----
    /// M0 off (dead); JA's read steps on; JA split into JA.op and JA.beat.
    drain_reads: bool = false,
    /// (with `drain_reads`) a drain barrier may answer unknown; S2 off.
    barrier_unknown: bool = false,
    /// (with `drain_reads, barrier_unknown`) JA2, a fresh joiner on host A.
    ja2: bool = false,
    /// L, a target restart; S2 off.
    lapse: bool = false,
    /// U, S1's clean unmount; S2 and JA off.
    kept_law: bool = false,
    /// JA's rule does not see M0 (A9); JA is a fresh joiner admission (no
    /// re-assertion wait); S2 off.
    m0_unlisted: bool = false,
    /// JA is a fresh joiner admission whose rule sees M0; S2 off.
    fresh_ja: bool = false,
    /// M0 off; JA split; S2 may preempt-all after JA re-registered under S1.
    chain_after_reregister: bool = false,
    /// S1's page check may read unreadable at any of its beats; S2 off.
    page_unreadable: bool = false,
    /// (with `drain_reads`) `elapsed` enabled for JA's word.
    all_elapsed: bool = false,
    /// (with `lapse`) the product's two commands: a rule's re-read and its
    /// register as separate steps, the re-judge's mark clause, M0's grace
    /// report and its acquire's mark under the registration lock.
    split_register: bool = false,
    /// Trims (§7.4's ledger): M0's second write; S2's probe 1 and register
    /// merged.
    m0_second_write: bool = true,
    s2_merge_probe1: bool = false,
    /// N_tick.
    ticks: u8 = 2,
    // ---- weakenings ----
    takeover: Takeover = .preempt_all,
    abort: bool = true,
    probe_under_hold: bool = true,
    rule: bool = true,
    skip_if_registered: bool = false,
    keep_law: bool = true,
    lapse_claim_reread: bool = true,
    manager_page_check: bool = true,
    probe_before_preempt: bool = true,
    judge: Judge = .report_then_page,
    retry_budget: Budget = .per_succession,
    outcome_by: OutcomeBy = .generation,
    read_eio: ReadEio = .barrier_retry,
    unreadable_failstop: bool = false,
    barrier_unknown_as_eio: bool = false,
    resolution_gen: bool = true,
    gen_capture: GenCapture = .at_refusal,
    meta_defer_pending: bool = true,
    r1_unreadable: R1Unreadable = .stays_pending,
    claim_first: bool = true,
    reregister: bool = true,
    reassert_gate: bool = true,
    /// DC20: M0's page check, `admittedHere` and its command inside host A's
    /// registration lock (W49: false, revision 15's separate steps).
    mutation_lock: bool = true,
    /// DC24: a rule's register re-judges M0 and kills it if stale (W54:
    /// false, revision 16's rule judging M0 only at its read).
    register_reread: bool = true,
    /// DC24's mark clause at the re-judge (`split_register`; W55: false).
    rejudge_mark: bool = true,
    rejudge_kill: RejudgeKill = .with_release,
    lapse_end: LapseEnd = .own_acquire,
    lapse_conflict: LapseConflict = .stands_down,
    strict: Strict = .{},
};

/// A successor's steps.
const SStep = enum(u8) { probe1, register, probe2, r0, acquire, preempt, r1, probe3, replay, cover_admit, cover_land, bind, beat, done };

fn successorOrder(comptime cfg: SuccessionConfig, comptime s: usize) []const SStep {
    const merged = s == 1 and cfg.s2_merge_probe1;
    const front: []const SStep = if (!cfg.claim_first)
        &.{ .register, .replay, .cover_admit, .cover_land, .probe1, .probe2, .r0, .acquire, .preempt, .r1, .probe3, .bind }
    else if (merged)
        &.{ .register, .probe2, .r0, .acquire, .preempt, .r1, .probe3, .replay, .cover_admit, .cover_land, .bind }
    else
        &.{ .probe1, .register, .probe2, .r0, .acquire, .preempt, .r1, .probe3, .replay, .cover_admit, .cover_land, .bind };
    const beats: []const SStep = if (s == 0 and cfg.page_unreadable) &.{ .beat, .beat, .beat, .beat, .beat } else &.{};
    return front ++ beats ++ &[_]SStep{.done};
}

/// M0's steps (§7.2 thread 0).
const MStep = enum(u8) { w1_admit, w1_land, beat_page, beat_report, lapse_register, grace_report, lapse_acquire, claim_reread, w2_admit, w2_land, rec_page, rec_admitted, rec_fence, done };

/// The unsplit JA's steps, and JA.op's and JA.beat's.
const JStep = enum(u8) { write, land, barrier, idle, classify, await_reassert, rule, reread, register, rejudge, kill, withdraw, post, retry, done };
const OStep = enum(u8) { read, read_barrier, write, land, capture, verdict, verdict_barrier, pend, recheck, record, pre_report, pre_r1, wait, eio_barrier, retry, done };
const BStep = enum(u8) { idle, await_reassert, rule, register, rejudge, post };
const J2Step = enum(u8) { rule, register, rejudge, post, done };

/// `Succession` — one metadata namespace; hosts A, B, C:
///   0  M0   host A's manager, frozen past its TTL (its claim aged), resuming
///           anywhere: a write (admit; land), its beat (page check; report;
///           after a lapse whose claim is ours, the lapse register under
///           host A's registration lock, the acquire answered by the holder
///           pointer — a conflict stands M0 down (DC21) — and the claim
///           re-read under the hold), a second write, and a recovery
///           mutation inside host A's registration lock (DC20: take it, page
///           check, `admittedHere`, `fence(K_S)` of S1's host key, release).
///   1  S1   the successor on host B: probe 1, register, probe 2, R0,
///           acquire if unreserved, the preempt-all (per victim the ref kill
///           and the unregister, then the drain and its completion), R1 (the
///           generation outcome), probe 3, the replay, the cover write
///           (admit; land), the bind (claim; appender 0's page names S1).
///   2  S2   the successor on host C, the same, pausable anywhere.
///   3  JA   the joiner co-located with M0: its metadata write (admitted, or
///           refused EBADE and classified, or EIO at P9c and the barrier),
///           the member beat (S3: WEAR with its host unregistered), the
///           classifier (R-1, R-2, R-3), the park (re-assertion, C1), the
///           rule (judge M0, kill, drain), the register, the re-judge, the
///           post-check (C2) and `cleared`, the retry within its budget —
///           looping per observed succession. Split (JA.op, thread 3; JA.beat,
///           thread 4) in `drain_reads` and `chain_after_reregister`.
///           In `split_register` JA's re-read (under the registration lock),
///           register and re-judge (with the mark clause) are three steps,
///           and M0 reads a grace report between its lapse register and its
///           acquire.
///   5  L    a target restart; 6 U S1's clean unmount; 7 JA2 a fresh joiner
///           on host A (rule read, register, re-judge, post-check).
///   8  TICK, 9 ELAPSED (JA's word), 10 an unreadable tick, 11 an unreadable
///           R-1 of JA's own steps, 12 a barrier answering unknown, 13 an
///           unreadable page at S1's beat: rule L's environment and the
///           alternative outcomes.
pub fn Succession(comptime cfg: SuccessionConfig) type {
    return extern struct {
        const Self = @This();
        pub const thread_count: usize = 14;
        const T_M0 = 0;
        const T_S1 = 1;
        const T_S2 = 2;
        const T_JA = 3;
        const T_JB = 4;
        const T_L = 5;
        const T_U = 6;
        const T_JA2 = 7;
        const T_TICK = 8;
        const T_EL = 9;
        const T_UTICK = 10;
        const T_UOP = 11;
        const T_UBAR = 12;
        const T_UPAGE = 13;

        const SPLIT = cfg.drain_reads or cfg.chain_after_reregister;
        const HAS_M0 = !SPLIT;
        const HAS_S2 = !(cfg.lapse or cfg.kept_law or cfg.m0_unlisted or cfg.fresh_ja or cfg.page_unreadable or cfg.barrier_unknown or cfg.ja2);
        const HAS_JA = !cfg.kept_law;
        const FRESH = cfg.m0_unlisted or cfg.fresh_ja;
        const ORDER = [2][]const SStep{ successorOrder(cfg, 0), successorOrder(cfg, 1) };
        const R1_IDX = [2]u8{ indexOf(ORDER[0], .r1), indexOf(ORDER[1], .r1) };

        fn indexOf(comptime order: []const SStep, comptime target: SStep) u8 {
            for (order, 0..) |e, k| if (e == target) return k;
            unreachable;
        }
        const HOST = [3]u8{ A, B, C };
        const M0: u8 = 0;

        comptime {
            if (cfg.barrier_unknown and !cfg.drain_reads) @compileError("barrier_unknown rides drain_reads");
            if (cfg.ja2 and !cfg.barrier_unknown) @compileError("ja2 rides drain_reads, barrier_unknown");
            if (cfg.all_elapsed and !SPLIT) @compileError("all_elapsed rides a split configuration");
            if (cfg.split_register and !cfg.lapse) @compileError("split_register rides lapse");
        }

        // ---- the device ----
        rtype: u8 = 5,
        reg: [3]bool = .{ true, false, false },
        gen: u8 = 0,
        /// Per host, its controllers' namespace refs killed (a drain).
        killed: [3]bool = @splat(false),
        /// The successor whose preempt-all holds pr_sem (NONE: none).
        drain_by: u8 = NONE,
        /// A target restart happened (A3 opens), and each incarnation's own
        /// acquire since it (A3 closes for that incarnation's writes: a write
        /// past its own re-acquire is the re-acquire's, as
        /// `host_fence_model.Election`).
        restarted: bool = false,
        reacq: [3]bool = @splat(false),
        // ---- the claim and appender 0's page ----
        claim: u8 = M0,
        page0: u8 = M0,
        epoch: u8 = 0,
        alive: [3]bool = .{ HAS_M0, true, HAS_S2 },
        bound: [3]bool = .{ true, false, false },
        failed: [3]bool = @splat(false),
        view: [3]u8 = @splat(0),
        // ---- M0 ----
        m_pc: u8 = 0,
        m_w: Wr = .idle,
        m_w_after: bool = false,
        m_w_unres: bool = false,
        m_w_blind: bool = false,
        m_w_window: bool = false,
        /// A successor's preempt-all reached host A since M0's last acquire
        /// (row 7: M0's writes issued after it are fenced writes).
        pa_hit_a: bool = false,
        /// A successor's takeover reached host A since M0's last acquire —
        /// its preempt-all, or its acquire of a namespace a restart left
        /// without A: M0 is no longer the current manager (rows 4, 15).
        sup_a: bool = false,
        // ---- the successors ----
        s_pc: [2]u8 = @splat(0),
        r0_rtype: [2]u8 = @splat(0),
        r0_others: [2]bool = @splat(false),
        r0_gen: [2]u8 = @splat(0),
        keep: [2]bool = @splat(false),
        pa_n: [2]u8 = @splat(0),
        pa_i: [2]u8 = @splat(0),
        pa_v: [2][2]u8 = @splat(@splat(NONE)),
        pa_started: [2]bool = @splat(false),
        fresh2: [2]u8 = @splat(NONE),
        fenced_claim: [2]u8 = @splat(NONE),
        s_w: [2]Wr = @splat(.idle),
        s_w_unres: [2]bool = @splat(false),
        /// S1's consecutive unreadable page checks (`page_unreadable`).
        unread_beats: u8 = 0,
        /// The successor's preempt-all landed on S1's host (S1 superseded).
        pa_hit_b: bool = false,
        // ---- JA (unsplit, or JA.op) and JA.beat ----
        j_pc: u8 = 0,
        b_pc: u8 = 0,
        j_failed: bool = false,
        j_held: bool = false,
        /// The successions (bit per successor) and the restart that
        /// unregistered host A, and the retries JA's held write made: one
        /// retry per such succession or lapse (W22: one in all).
        a_events: u8 = 0,
        j_retries: u8 = 0,
        /// The manager JA last saw itself registered under (`mgr_writer[v]`).
        mgr_writer: u8 = M0,
        /// Appender 0's writer when JA's (or JA.beat's) register ran: the
        /// manager it saw itself registered under.
        reg_writer: u8 = M0,
        j_w: Wr = .idle,
        /// The rule that killed M0 waits for its in-flight write (C3).
        j_killed_m0: bool = false,
        j_blind: bool = false,
        j_fresh_reg: bool = false,
        b_killed_m0: bool = false,
        b_fresh_reg: bool = false,
        bar_used: bool = false,
        /// The hook's deferred classification (inputs unreadable).
        deferred: bool = false,
        waiter: Waiter = .{},
        word: Word = .{},
        // ---- JA2 ----
        j2_pc: u8 = if (cfg.ja2) 0 else @backingInt(J2Step.done),
        j2_fresh: bool = false,
        // ---- host A's registration bookkeeping ----
        /// A rule's register of host A stands, its re-judge pending (A5).
        window_a: bool = false,
        /// Host A's registration was made by a blind rule (A9).
        blind_a: bool = false,
        /// Host A's last unregistration is explained (a preempt-all, a
        /// restart, a rule's withdrawal of its fresh registration).
        explained_a: bool = false,
        /// Host A's registration lock (DC20, narrowed in revision 17): the
        /// thread holding it (M0's recovery mutation; a non-blind rule from
        /// its register through its re-judge), NONE when free.
        lock_a: u8 = NONE,
        /// M0's recovery mutation's page check read "ours" (A9's instant
        /// under a blind registration).
        m_checked_ours: bool = false,
        /// (`split_register`) M0's re-acquire mark, set by its acquire's
        /// success; JA's re-read cleared it, and found the namespace
        /// unreserved (DC24's mark clause).
        m0_marked: bool = false,
        rr_unres: bool = false,
        /// The reservation's holder (§7.1): set by an acquire or a
        /// preempt-all, handed to the first remaining registrant in
        /// registration order when the holder unregisters, cleared by a
        /// restart; NONE while unreserved.
        holder: u8 = A,
        reg_order: [3]u8 = .{ A, NONE, NONE },
        /// W48: the changes of `mgr_writer` and the lapse JA saw.
        mw_changes: u8 = 0,
        lapse_seen: bool = false,
        // ---- the environment ----
        ticks: u8 = 0,
        l_done: bool = !cfg.lapse,
        u_done: bool = !cfg.kept_law,
        // ---- flags ----
        killed_current: bool = false,
        preempted_live: bool = false,
        wrote_over_live: bool = false,
        two_managers: bool = false,
        overwrote_acked: bool = false,
        fenced_wrote: bool = false,
        live_fenced: bool = false,
        cleared_after_establish: bool = false,
        mfsu: bool = false,
        hit_lapse: bool = false,
        hit_register: bool = false,
        hit_probe: bool = false,
        hit_blind: bool = false,

        pub fn init() Self {
            var s: Self = .{};
            if (!HAS_JA) {
                s.j_pc = @backingInt(JStep.done);
                s.j_failed = true;
            } else if (SPLIT) {
                s.j_pc = @backingInt(if (cfg.drain_reads) OStep.read else OStep.write);
            }
            return s;
        }

        // ------------------------------------------------------------------
        // Thread status.
        // ------------------------------------------------------------------

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                T_M0 => !HAS_M0 or ((self.m_pc >= @backingInt(MStep.done) or !self.alive[M0] or self.failed[M0]) and self.m_w != .admitted),
                T_S1, T_S2 => blk: {
                    const s = t - T_S1;
                    break :blk !self.alive[s + 1] or self.failed[s + 1] or ORDER[s][self.s_pc[s]] == .done;
                },
                T_JA => !HAS_JA or self.jaDone(),
                T_JB => !SPLIT or self.jbDone(),
                T_L => self.l_done,
                T_U => self.u_done or !self.alive[1] or self.failed[1],
                T_JA2 => self.j2_pc >= @backingInt(J2Step.done),
                T_TICK => !HAS_JA or !self.waitingAny(),
                T_EL => !self.elapsedApplies(),
                T_UTICK => !self.uTickApplies(),
                T_UOP => !self.uOpApplies(),
                T_UBAR => !self.uBarApplies(),
                T_UPAGE => !self.uPageApplies(),
                else => unreachable,
            };
        }

        fn jaDone(self: *const Self) bool {
            if (self.j_failed) return self.j_w != .admitted;
            if (SPLIT) return self.j_pc == @backingInt(OStep.done);
            const pc: JStep = @fromBackingInt(@intCast(self.j_pc));
            return pc == .done or (pc == .idle and !self.jaTriggered());
        }

        /// The unsplit JA's member beat has work: its host unregistered
        /// (WEAR: S3; unreserved: the lapse beat).
        fn jaTriggered(self: *const Self) bool {
            return !self.reg[A];
        }

        fn jbDone(self: *const Self) bool {
            if (self.j_failed) return true;
            if (self.word.st == .terminal) return false;
            const pc: BStep = @fromBackingInt(@intCast(self.b_pc));
            return pc == .idle and !(self.rtype == 5 and !self.reg[A]);
        }

        /// Some word pending or some held operation waiting: `tick` runs.
        fn waitingAny(self: *const Self) bool {
            return self.word.st == .pending or (SPLIT and self.waiter.held and !self.j_failed);
        }

        // ------------------------------------------------------------------
        // The device.
        // ------------------------------------------------------------------

        /// pr_sem is held by a preempt-all's drain: Register, Acquire,
        /// Release, Replace and Preempt wait (P9d); Report does not.
        fn prBusy(self: *const Self) bool {
            return self.drain_by != NONE;
        }

        fn admits(self: *const Self, h: u8) bool {
            return self.rtype == 0 or self.reg[h];
        }

        fn othersRegistered(self: *const Self, h: u8) bool {
            for (0..3) |x| if (x != h and self.reg[x]) return true;
            return false;
        }

        fn register(self: *Self, h: u8) void {
            self.reg[h] = true;
            self.gen +%= 1;
            for (&self.reg_order) |*o| if (o.* == NONE) {
                o.* = h;
                break;
            };
            if (h == A) {
                self.explained_a = false;
                self.blind_a = false;
            }
        }

        /// Host `h` leaves the registrants; a hold it had passes to the first
        /// remaining registrant in registration order (`pr.c:261-290`).
        fn dropReg(self: *Self, h: u8) void {
            self.reg[h] = false;
            var o: [3]u8 = @splat(NONE);
            var n: usize = 0;
            for (self.reg_order) |x| if (x != NONE and x != h) {
                o[n] = x;
                n += 1;
            };
            self.reg_order = o;
            if (self.holder == h) self.holder = o[0];
        }

        /// The host's own unregister (P10: the reservation ends with its last
        /// registrant).
        fn unregister(self: *Self, h: u8) void {
            if (!self.reg[h]) return;
            self.dropReg(h);
            self.gen +%= 1;
            if (!self.reg[A] and !self.reg[B] and !self.reg[C]) {
                self.rtype = 0;
                self.holder = NONE;
            }
        }

        /// A preempt-and-abort of host `v`'s key, PRKEY ≠ 0 (P5 + P9): `v`
        /// unregistered. Under All-Registrants it sets no holder; a hold `v`
        /// had passes to the first remaining registrant (`pr.c:486-507`,
        /// `:261-291`).
        fn fenceKey(self: *Self, v: u8) void {
            self.dropReg(v);
            self.gen +%= 1;
        }

        /// Host A's registration lock is free for thread `t` (DC20).
        fn lockFree(self: *const Self, t: u8) bool {
            return self.lock_a == NONE or self.lock_a == t;
        }

        fn unlockA(self: *Self, t: u8) void {
            if (self.lock_a == t) self.lock_a = NONE;
        }

        /// A rule takes host A's registration lock from its register through
        /// its re-judge; a blind rule's registry is another directory
        /// (`m0_unlisted`), and a rule-less register (W5) takes none.
        const RULE_LOCKS = cfg.rule and !cfg.m0_unlisted;

        /// A rule's register by thread `t`: false while another holds the
        /// lock.
        fn ruleLock(self: *Self, t: u8) bool {
            if (!RULE_LOCKS) return true;
            if (!self.lockFree(t)) return false;
            self.lock_a = t;
            return true;
        }

        /// Another writer's claim is fresh (M0's aged past its TTL).
        fn claimFreshForeign(self: *const Self, i: u8) bool {
            const c = self.claim;
            return c != NONE and c != i and c != M0 and self.alive[c] and self.bound[c] and !self.failed[c];
        }

        fn claimLiveForeign(self: *const Self, i: u8) bool {
            return self.claimFreshForeign(i) and self.admits(HOST[self.claim]);
        }

        /// The incarnation is a current manager: bound, alive, not
        /// fail-stopped, its claim on the device.
        fn currentManager(self: *const Self, i: u8) bool {
            return self.bound[i] and self.alive[i] and !self.failed[i] and self.claim == i;
        }

        // ------------------------------------------------------------------
        // Steps.
        // ------------------------------------------------------------------

        pub fn step(self: *Self, t: usize) void {
            self.stepAs(t, false);
            self.post();
        }

        /// `unreadable`: the alternative outcome of the step it replaces.
        fn stepAs(self: *Self, t: usize, unreadable: bool) void {
            switch (t) {
                T_M0 => self.manager0(),
                T_S1 => self.successor(0),
                T_S2 => self.successor(1),
                T_JA => if (self.j_failed) {
                    // A latched member's admitted write still lands.
                    self.j_w = .landed;
                } else if (SPLIT) self.op(unreadable) else self.joiner(unreadable),
                T_JB => self.beat(),
                T_L => self.restart(),
                T_U => self.unmountS1(),
                T_JA2 => self.joiner2(),
                T_TICK => self.tick(false),
                T_EL => self.word.elapse(),
                T_UTICK => self.tick(true),
                T_UOP => {
                    self.word.unread_used = true;
                    self.stepAs(T_JA, true);
                },
                T_UBAR => {
                    self.bar_used = true;
                    self.op(true);
                },
                T_UPAGE => self.successor1Beat(true),
                else => unreachable,
            }
        }

        /// After every step: rule L's readable-tick anchor holds only while
        /// its conditions do.
        fn post(self: *Self) void {
            if (!(self.wordWaiting() and self.regCurrentA())) self.word.rtick = false;
        }

        /// JA's word is waiting: pending, or its held operation blocked.
        fn wordWaiting(self: *const Self) bool {
            return self.word.st == .pending or (SPLIT and self.waiter.blocked(&self.word, cfg.resolution_gen));
        }

        /// Host A registered under a standing reservation (JA is current
        /// throughout this model).
        fn regCurrentA(self: *const Self) bool {
            return HAS_JA and !self.j_failed and self.rtype == 5 and self.reg[A];
        }

        // ---- M0 ----

        fn manager0(self: *Self) void {
            if (!self.alive[M0] or self.failed[M0] or self.m_pc >= @backingInt(MStep.done)) {
                // A killed or latched process's admitted write still lands
                // (the target holds it).
                self.m_w = .landed;
                self.managerLanded(M0, self.m_w_after, self.m_w_unres, self.m_w_blind, self.m_w_window);
                return;
            }
            const s: MStep = @fromBackingInt(@intCast(self.m_pc));
            switch (s) {
                .w1_admit, .w2_admit => {
                    if (s == .w2_admit and !cfg.m0_second_write) {
                        self.m_pc = @backingInt(MStep.rec_page);
                        return;
                    }
                    // A refusal or a P9c EIO on the manager's own volume
                    // latches (R-0).
                    if (self.killed[A] or !self.admits(A)) {
                        self.failStopM0();
                        return;
                    }
                    self.m_w = .admitted;
                    self.m_w_after = self.pa_hit_a;
                    self.m_w_unres = self.inA3(M0);
                    self.m_w_blind = self.blind_a;
                    self.m_w_window = self.window_a;
                },
                .w1_land, .w2_land => {
                    self.m_w = .landed;
                    self.managerLanded(M0, self.m_w_after, self.m_w_unres, self.m_w_blind, self.m_w_window);
                },
                // During a drain of host A its page reads get EIO (P9b): the
                // check is unreadable — the beat skips its refresh (bounded
                // by 5 such beats, never reached here) and fail-stops only
                // on a positive mismatch.
                .beat_page => if (!self.killed[A] and cfg.manager_page_check and self.page0 != M0) {
                    self.failStopM0();
                    return;
                },
                .beat_report => {
                    if (self.rtype == 5) {
                        if (!self.reg[A]) {
                            self.failStopM0();
                            return;
                        }
                        self.m_pc = @backingInt(MStep.w2_admit);
                        return;
                    }
                },
                .lapse_register => {
                    // DC6: the device's claim must be ours.
                    if (self.claim != M0) {
                        self.failStopM0();
                        return;
                    }
                    // The lapse rule's register, under host A's registration
                    // lock.
                    if (cfg.rule and !self.lockFree(T_M0)) return;
                    if (self.prBusy()) return;
                    if (!self.reg[A]) self.register(A);
                    if (!cfg.split_register) {
                        self.m_pc = @backingInt(MStep.lapse_acquire);
                        return;
                    }
                },
                .grace_report => if (self.rtype == 5) {
                    // G4: WEAR before M0's own acquire — a successor's
                    // acquire took the namespace; M0 stands down (W56: with
                    // host A registered the lapse ends with no acquire).
                    if (cfg.lapse_end == .on_report and self.reg[A]) {
                        self.m_pc = @backingInt(MStep.w2_admit);
                        return;
                    }
                    self.failStopM0();
                    return;
                },
                .lapse_acquire => {
                    if (self.prBusy()) return;
                    // `split_register`: the acquire and its mark under host
                    // A's registration lock.
                    if (cfg.split_register and !self.lockFree(T_M0)) return;
                    if (!self.reg[A]) {
                        self.failStopM0();
                        return;
                    }
                    if (self.holder == NONE) {
                        self.rtype = 5;
                        self.holder = A;
                    } else if (self.holder != A and cfg.lapse_conflict == .stands_down) {
                        // DC21: another host holds — after a restart only an
                        // open acquires, so a successor took the namespace.
                        self.sup_a = true;
                        self.failStopM0();
                        return;
                    }
                    self.reacq[M0] = true;
                    self.pa_hit_a = false;
                    self.sup_a = false;
                    if (cfg.split_register) self.m0_marked = true;
                },
                .claim_reread => if (cfg.lapse_claim_reread and self.claim != M0) {
                    self.failStopM0();
                    return;
                },
                .rec_page => {
                    // Unreadable during host A's drain: the mutation is
                    // withheld, the recovery re-runs at the next poll.
                    if (self.killed[A]) return;
                    // DC20: the section takes host A's registration lock
                    // first.
                    if (cfg.mutation_lock) {
                        if (!self.lockFree(T_M0)) return;
                        self.lock_a = T_M0;
                    }
                    if (cfg.manager_page_check and self.page0 != M0) {
                        self.failStopM0();
                        return;
                    }
                    self.m_checked_ours = cfg.manager_page_check;
                },
                .rec_admitted => if (!(self.rtype == 5 and self.reg[A])) {
                    self.unlockA(T_M0);
                    self.m_pc = @backingInt(MStep.done);
                    return;
                },
                .rec_fence => {
                    if (self.prBusy()) return;
                    // fence(K_B) (P5 + P9): the sender must be registered
                    // (P8); S1's admitted write drains first.
                    if (self.reg[A] and self.rtype == 5 and self.reg[B]) {
                        if (self.s_w[0] == .admitted) return;
                        self.fenceKey(B);
                        self.pa_hit_b = true;
                        // A3 exempts writes, never a PR mutation (M1). M0 is
                        // superseded by a takeover reaching host A, or by a
                        // successor's bind after its own (a fact M0's
                        // re-acquire does not undo).
                        if ((self.sup_a or self.view[M0] < self.epoch) and (self.liveBound(1) or self.liveBound(2))) {
                            // A9: a check that read "ours" before the bind
                            // while a blind rule re-admitted host A.
                            if (self.blind_a and self.m_checked_ours) self.hit_blind = true else self.two_managers = true;
                        }
                    }
                    self.unlockA(T_M0);
                },
                .done => unreachable,
            }
            self.m_pc += 1;
        }

        fn liveBound(self: *const Self, i: u8) bool {
            return self.bound[i] and self.alive[i] and !self.failed[i];
        }

        /// M0 fail-stops: proven when a successor's preempt-all reached its
        /// host or appender 0's page names another writer; unproven otherwise
        /// (row 15).
        fn failStopM0(self: *Self) void {
            self.failed[M0] = true;
            self.unlockA(T_M0);
            if (!self.sup_a and self.page0 == M0) self.mfsu = true;
        }

        /// A manager incarnation's metadata write landed (rows 3, 6, 7 and
        /// their windows; `i == M0`: the superseded manager).
        fn managerLanded(self: *Self, i: u8, after_pa: bool, unres: bool, blind: bool, window: bool) void {
            const stale = self.view[i] < self.epoch;
            const over_live = i != M0 and self.claimLiveForeign(i) and self.fenced_claim[i - 1] != self.claim;
            const fenced = i == M0 and after_pa;
            if (!stale and !over_live and !fenced) return;
            if (unres) {
                self.hit_lapse = true;
            } else if (blind) {
                self.hit_blind = true;
            } else if (window) {
                self.hit_register = true;
            } else {
                if (over_live) self.wrote_over_live = true;
                if (stale) self.overwrote_acked = true;
                if (fenced) self.fenced_wrote = true;
            }
        }

        // ---- the successors ----

        fn successor(self: *Self, s: usize) void {
            const i: u8 = @intCast(s + 1);
            const h = HOST[i];
            const st = ORDER[s][self.s_pc[s]];
            switch (st) {
                .probe1 => if (self.claimFreshForeign(i)) {
                    self.alive[i] = false;
                    return;
                },
                .register => {
                    if (self.prBusy()) return;
                    if (!self.reg[h]) self.register(h);
                },
                .probe2 => {
                    self.fresh2[s] = if (self.claimFreshForeign(i)) self.claim else NONE;
                    if (cfg.probe_before_preempt and self.fresh2[s] != NONE) return self.discard(s);
                },
                .r0 => {
                    self.r0_rtype[s] = self.rtype;
                    self.r0_others[s] = self.othersRegistered(h);
                    self.r0_gen[s] = self.gen;
                    if (self.rtype == 5 and !self.reg[h]) return self.discard(s);
                    if (cfg.skip_if_registered and self.rtype == 5 and self.reg[h]) {
                        self.s_pc[s] = R1_IDX[s];
                        return;
                    }
                },
                .acquire => if (self.r0_rtype[s] == 0) {
                    if (self.prBusy()) return;
                    if (self.rtype == 0 and self.reg[h]) {
                        // M3: a successor's acquire after a restart supersedes
                        // M0 (it lands only before M0's own re-acquire).
                        self.rtype = 5;
                        self.holder = h;
                        self.reacq[i] = true;
                        self.sup_a = true;
                    } else if (self.rtype != 5) return self.discard(s);
                },
                .preempt => {
                    if (!self.preempt(s)) return;
                },
                .r1 => {
                    if (!(self.rtype == 5 and self.reg[h])) return self.discard(s);
                    const by_gen = cfg.outcome_by == .r0_only or self.gen == self.r0_gen[s] +% 1;
                    self.keep[s] = !(self.r0_rtype[s] == 0 and !self.r0_others[s] and by_gen);
                },
                .probe3 => if (cfg.probe_under_hold and self.claimFreshForeign(i)) {
                    // A fresh claim whose writer this open's preempt fenced
                    // is taken over; another refuses.
                    if (!self.reg[HOST[self.claim]]) self.fenced_claim[s] = self.claim else return self.discard(s);
                },
                .replay => self.view[i] = self.epoch,
                .cover_admit => {
                    if (self.killed[h] or !self.admits(h)) return self.discard(s);
                    self.s_w[s] = .admitted;
                    self.s_w_unres[s] = self.inA3(i);
                },
                .cover_land => {
                    self.s_w[s] = .landed;
                    self.managerLanded(i, false, self.s_w_unres[s], false, false);
                },
                .bind => {
                    if (self.killed[h] or !self.admits(h)) return self.discard(s);
                    self.managerLanded(i, false, self.inA3(i), false, false);
                    self.claim = i;
                    self.page0 = i;
                    self.bound[i] = true;
                    self.epoch += 1;
                    self.view[i] = self.epoch;
                },
                .beat => return self.successor1Beat(false),
                .done => unreachable,
            }
            self.s_pc[s] += 1;
        }

        /// S1's beat after its bind (`page_unreadable`): its page check reads
        /// ours, or (the alternative) unreadable — fail-stop at 5
        /// consecutive (W31: at the first).
        fn successor1Beat(self: *Self, unreadable: bool) void {
            if (unreadable) {
                self.unread_beats += 1;
                if (cfg.unreadable_failstop or self.unread_beats >= 5) {
                    self.failed[1] = true;
                    if (!self.pa_hit_b and self.unread_beats < 5) self.mfsu = true;
                    return;
                }
            } else self.unread_beats = 0;
            self.s_pc[0] += 1;
        }

        /// The preempt-all, one action per step: its start (P8, P6, the
        /// victims fixed, the holder and the rtype), per victim the ref kill
        /// and the unregister, then the completion once no victim has an
        /// admitted command (P9). False: not advanced past the step.
        fn preempt(self: *Self, s: usize) bool {
            const i: u8 = @intCast(s + 1);
            const h = HOST[i];
            if (!self.pa_started[s]) {
                if (self.prBusy()) return false;
                // P8: the sender must be registered; P6: reserved.
                if (!self.reg[h] or self.rtype != 5) {
                    self.discard(s);
                    return false;
                }
                var n: u8 = 0;
                switch (cfg.takeover) {
                    // Victims in registration order (`pr.c:530-547`).
                    .preempt_all => for (self.reg_order) |x| {
                        if (x != NONE and x != h) {
                            self.pa_v[s][n] = x;
                            n += 1;
                        }
                    },
                    // The claim's key: absent counts as fenced (report first).
                    .preempt_pred_key => {
                        const ph = HOST[self.claim];
                        if (self.claim != NONE and ph != h and self.reg[ph]) {
                            self.pa_v[s][0] = ph;
                            n = 1;
                        }
                    },
                }
                self.pa_n[s] = n;
                self.pa_i[s] = 0;
                self.pa_started[s] = true;
                self.drain_by = @intCast(s);
                // The generation at the command (`pr.c:664-666`), before
                // the abort work.
                self.gen +%= 1;
                // P4: the preemptor holds; a key preempt (W1) sets no holder.
                if (cfg.takeover == .preempt_all) self.holder = h;
                if (!cfg.abort) {
                    for (0..n) |k| self.preemptUnreg(s, self.pa_v[s][k]);
                    self.completePreempt(s);
                    return true;
                }
                return false;
            }
            const k = self.pa_i[s];
            if (k < 2 * self.pa_n[s]) {
                const v = self.pa_v[s][k / 2];
                if (k % 2 == 0) {
                    self.killed[v] = true;
                    if (v == A) {
                        self.pa_hit_a = true;
                        self.sup_a = true;
                    }
                    if (v == B) self.pa_hit_b = true;
                } else self.preemptUnreg(s, v);
                self.pa_i[s] += 1;
                return false;
            }
            // The drain: every victim's admitted commands complete first.
            for (0..self.pa_n[s]) |x| if (self.victimAdmitted(self.pa_v[s][x])) return false;
            self.completePreempt(s);
            return true;
        }

        fn completePreempt(self: *Self, s: usize) void {
            self.killed = @splat(false);
            self.drain_by = NONE;
            self.reacq[s + 1] = true;
        }

        /// A victim's unregistration inside the preempt-all (row 2 at a bound
        /// manager's host).
        fn preemptUnreg(self: *Self, s: usize, v: u8) void {
            const i: u8 = @intCast(s + 1);
            for (0..3) |m| {
                const mi: u8 = @intCast(m);
                if (mi == i or HOST[mi] != v or !self.liveBound(mi) or !self.admits(v)) continue;
                if (mi != M0 and self.claim == mi) {
                    if (self.fresh2[s] == mi) self.preempted_live = true else self.hit_probe = true;
                }
            }
            self.dropReg(v);
            if (v == A) {
                self.a_events |= @as(u8, 1) << @intCast(s);
                self.pa_hit_a = true;
                self.sup_a = true;
                self.explained_a = true;
                self.blind_a = false;
            }
            if (v == B) self.pa_hit_b = true;
        }

        fn victimAdmitted(self: *const Self, v: u8) bool {
            return switch (v) {
                A => self.m_w == .admitted or self.j_w == .admitted,
                B => self.s_w[0] == .admitted,
                C => self.s_w[1] == .admitted,
                else => false,
            };
        }

        /// A refused open: no longer alive; its registration given back as
        /// `leaveRegistration` says (§4.12; a preempting hold kept).
        fn discard(self: *Self, s: usize) void {
            const i: u8 = @intCast(s + 1);
            const h = HOST[i];
            self.alive[i] = false;
            if (!self.reg[h]) return;
            if (self.othersRegistered(h)) {
                self.unregister(h);
            } else if (!(cfg.keep_law and self.keep[s])) {
                self.unregister(h);
            }
        }

        // ---- L, U ----

        fn restart(self: *Self) void {
            // The namespace's disable waits for its in-flight requests, and a
            // preempt-all holds the namespace until its drain ends.
            if (self.prBusy() or self.anyAdmitted()) return;
            if (self.reg[A]) self.a_events |= 4;
            self.rtype = 0;
            self.reg = @splat(false);
            self.reg_order = @splat(NONE);
            self.holder = NONE;
            self.restarted = true;
            self.reacq = @splat(false);
            self.explained_a = true;
            self.blind_a = false;
            self.l_done = true;
        }

        fn anyAdmitted(self: *const Self) bool {
            return self.m_w == .admitted or self.j_w == .admitted or self.s_w[0] == .admitted or self.s_w[1] == .admitted;
        }

        /// S1's clean unmount (§4.12): the claim deleted, the page Free, then
        /// another host registered ⇒ unregister; the last registrant keeps its
        /// registration unless its outcome was `.acquired` (it fenced nobody).
        fn unmountS1(self: *Self) void {
            if (!self.bound[1]) return;
            if (self.claim == 1) self.claim = NONE;
            if (self.page0 == 1) self.page0 = NONE;
            self.bound[1] = false;
            self.alive[1] = false;
            if (self.reg[B]) {
                if (self.othersRegistered(B)) {
                    self.unregister(B);
                } else if (!(cfg.keep_law and self.keep[0])) {
                    self.unregister(B);
                }
            }
            self.u_done = true;
        }

        // ---- JA: the stale-sibling rule's verdict on M0 ----

        /// M0's manager line judged stale (`judgeRegion`): reserved with host A
        /// unregistered, or appender 0's page naming another writer (the
        /// verdict while unreserved, and the re-judge's after its own
        /// register); W21: by the page alone.
        fn m0Stale(self: *const Self) bool {
            return switch (cfg.judge) {
                .report_then_page => (self.rtype == 5 and !self.reg[A]) or self.page0 != M0,
                .page_only => self.page0 != M0,
            };
        }

        /// The rule kills M0 when it judges it stale (`m0_unlisted`: the rule
        /// cannot see it; W5: no rule). True: killed now.
        fn ruleKillsM0(self: *Self) bool {
            if (!self.ruleJudgesM0Stale()) return false;
            self.killM0();
            return true;
        }

        /// The rule's verdict on M0: stale (`m0_unlisted`: the rule cannot
        /// see it; W5: no rule).
        fn ruleJudgesM0Stale(self: *const Self) bool {
            if (!HAS_M0 or !cfg.rule or cfg.m0_unlisted) return false;
            return self.alive[M0] and self.m0Stale();
        }

        /// A rule's SIGKILL of M0 (row 1: M0 current — bound, alive, its
        /// claim, no takeover reached host A).
        fn killM0(self: *Self) void {
            if (self.currentManager(M0) and !self.sup_a) self.killed_current = true;
            self.alive[M0] = false;
            // A killed process's lock is released with it.
            self.unlockA(T_M0);
        }

        /// (`split_register`) DC24's mark clause at the re-judge: JA's re-read
        /// found the namespace unreserved, it is reserved now, and M0 has not
        /// acquired since — another host's acquire superseded M0.
        fn markClauseStale(self: *const Self) bool {
            if (!cfg.split_register or !cfg.rejudge_mark or !RULE_LOCKS) return false;
            return self.alive[M0] and self.rr_unres and self.rtype == 5 and !self.m0_marked;
        }

        /// The rule's register waits for the killed M0's in-flight write (C3:
        /// the node drained).
        fn m0Draining(self: *const Self) bool {
            return self.m_w == .admitted;
        }

        /// The register's re-read (DC24): the rule judges M0 again, atomically
        /// with its register, and kills a stale M0 instead of registering;
        /// the next attempt waits for its drain (W54: no re-read).
        fn registerRereadKills(self: *Self) bool {
            return cfg.register_reread and self.ruleKillsM0();
        }

        /// A rule's register of host A (its re-judge pending: A5; blind: A9).
        fn ruleRegisterA(self: *Self, blind: bool) void {
            self.register(A);
            self.window_a = cfg.rule and !blind;
            self.blind_a = blind;
        }

        /// JA's refusal (row 13a): unexplained since host A's last
        /// registration.
        fn noteRefusal(self: *Self) void {
            if (!self.explained_a) self.live_fenced = true;
        }

        /// JA latches, expires or fail-stops (row 13b) — exempt: the
        /// deadline's own terminal (pin 1).
        fn latchJA(self: *Self) void {
            self.j_failed = true;
            if (!(self.word.st == .terminal and self.word.by_deadline)) self.live_fenced = true;
            self.waiter.reset();
            self.unlockA(T_JA);
            self.unlockA(T_JB);
        }

        /// A retry is left: fewer made than the takeovers (by successor) and
        /// the restart that unregistered host A (M7, §4.5.3 since revision
        /// 16). Every such event reaches JA as a refusal or its beat's S3
        /// before the retry it pays for; counting the events, not appender
        /// 0's writers, keeps a re-registration under a newer successor's
        /// reservation that lands before that successor's page from spending
        /// the newer succession's retry (W48 counts the writers).
        fn retryLeft(self: *const Self) bool {
            return switch (cfg.retry_budget) {
                .once => self.j_retries < 1,
                .per_succession => self.j_retries < @popCount(self.a_events),
                .per_mgr_writer => self.j_retries < self.mw_changes + @as(u8, if (self.lapse_seen) 1 else 0),
            };
        }

        /// W48's bookkeeping: JA saw itself registered under a new manager.
        fn noteMgrWriter(self: *Self) void {
            if (cfg.retry_budget == .per_mgr_writer and self.reg_writer != self.mgr_writer) self.mw_changes += 1;
            self.mgr_writer = self.reg_writer;
        }

        /// W48's bookkeeping: JA's classifier saw a lapse.
        fn noteLapseSeen(self: *Self) void {
            if (cfg.retry_budget == .per_mgr_writer) self.lapse_seen = true;
        }

        /// The successor JA can re-assert with: bound and alive, appender 0's
        /// page naming it, not the manager JA last saw (C1).
        fn reassertTarget(self: *const Self) u8 {
            const p = self.page0;
            if (p == NONE or p == M0 or !self.liveBound(p)) return NONE;
            if (p == self.mgr_writer) return NONE;
            return p;
        }

        // ---- the unsplit JA ----

        fn joiner(self: *Self, unreadable: bool) void {
            const pc: JStep = @fromBackingInt(@intCast(self.j_pc));
            // The member left the park other than by re-registering
            // (`terminal`): its held work takes its failure path.
            if (self.word.st == .terminal) return self.latchJA();
            switch (pc) {
                .write => {
                    if (self.admits(A) and !self.killed[A]) {
                        self.j_w = .admitted;
                        self.j_pc = @backingInt(JStep.land);
                        return;
                    }
                    if (self.killed[A] and self.admits(A)) {
                        // P9c: INTERNAL → EIO; the barrier decides.
                        self.j_pc = @backingInt(JStep.barrier);
                        return;
                    }
                    self.noteRefusal();
                    self.j_held = true;
                    self.j_pc = @backingInt(JStep.classify);
                },
                .land => {
                    self.j_w = .landed;
                    self.j_held = false;
                    self.j_pc = @backingInt(JStep.idle);
                },
                .barrier => {
                    if (self.prBusy()) return;
                    // Registered: the EIO was transient (§7.1), re-issued.
                    if (self.reg[A]) {
                        self.j_pc = @backingInt(JStep.write);
                        return;
                    }
                    self.j_held = true;
                    self.j_pc = @backingInt(JStep.classify);
                },
                .idle => {
                    if (!self.jaTriggered()) return;
                    self.j_pc = @backingInt(JStep.classify);
                },
                .classify => {
                    if (self.killed[A]) return; // the hook's barrier: the drain first
                    self.jaClassify(unreadable);
                },
                .await_reassert => {
                    if (self.killed[A]) return;
                    if (self.reg[A] and self.rtype == 5) {
                        // Someone re-registered host A (observing: R-1, then
                        // `cleared`).
                        return self.jaObservedRegistered(unreadable);
                    }
                    if (self.rtype == 0) {
                        // A lapse: the lapse beat re-registers through the
                        // rule.
                        self.j_pc = @backingInt(JStep.rule);
                        return;
                    }
                    if (!FRESH and cfg.reassert_gate) {
                        const target = self.reassertTarget();
                        if (target == NONE) return;
                    }
                    self.j_pc = @backingInt(JStep.rule);
                },
                .rule => {
                    if (self.killed[A] or (!cfg.reregister and self.rtype == 5)) return;
                    if (self.reg[A]) {
                        // The rule's read finds the host registered: an
                        // adoption, nothing mutated (`registerUnderRule`).
                        self.j_fresh_reg = false;
                        self.reg_writer = self.page0;
                        self.j_pc = @backingInt(JStep.post);
                        return;
                    }
                    self.j_killed_m0 = self.ruleKillsM0();
                    self.j_blind = cfg.m0_unlisted and HAS_M0 and self.alive[M0];
                    self.j_pc = @backingInt(if (cfg.split_register) JStep.reread else JStep.register);
                },
                .reread => {
                    // `split_register`: the re-read, a command of its own
                    // under the registration lock, before the register.
                    if (self.j_killed_m0 and self.m0Draining()) return;
                    if (!self.ruleLock(T_JA)) return;
                    if (self.registerRereadKills()) {
                        // The SIGKILL sent, the lock released; the drain is
                        // waited for outside it.
                        self.j_killed_m0 = true;
                        self.unlockA(T_JA);
                        return;
                    }
                    if (self.reg[A]) {
                        // An adoption.
                        self.unlockA(T_JA);
                        self.j_fresh_reg = false;
                        self.reg_writer = self.page0;
                        self.j_pc = @backingInt(JStep.post);
                        return;
                    }
                    self.rr_unres = self.rtype == 0;
                    self.m0_marked = false;
                    self.j_pc = @backingInt(JStep.register);
                },
                .register => {
                    if (self.prBusy()) return;
                    if (!cfg.split_register) {
                        if (self.j_killed_m0 and self.m0Draining()) return;
                        if (!self.ruleLock(T_JA)) return;
                        if (self.registerRereadKills()) {
                            // The SIGKILL sent, the lock released; the drain
                            // is waited for outside it.
                            self.j_killed_m0 = true;
                            self.unlockA(T_JA);
                            return;
                        }
                    }
                    self.j_fresh_reg = !self.reg[A];
                    if (self.j_fresh_reg) self.ruleRegisterA(self.j_blind);
                    self.reg_writer = self.page0;
                    self.word.arm_open = true;
                    self.j_pc = @backingInt(JStep.rejudge);
                },
                .rejudge => {
                    self.window_a = false;
                    if (self.j_fresh_reg and (self.ruleJudgesM0Stale() or self.markClauseStale())) {
                        // A stale verdict: the SIGKILL and the release in one
                        // step; the withdrawal of the fresh registration
                        // next; the register again after the drain (W57: the
                        // release first, the kill a step later).
                        self.j_killed_m0 = true;
                        self.unlockA(T_JA);
                        if (cfg.rejudge_kill == .after_release) {
                            self.j_pc = @backingInt(JStep.kill);
                            return;
                        }
                        self.killM0();
                        self.j_pc = @backingInt(JStep.withdraw);
                        return;
                    }
                    self.unlockA(T_JA);
                    self.j_pc = @backingInt(JStep.post);
                },
                .kill => {
                    if (self.alive[M0]) self.killM0();
                    self.j_pc = @backingInt(JStep.withdraw);
                },
                .withdraw => {
                    self.unregister(A);
                    self.explained_a = true;
                    self.j_pc = @backingInt(if (cfg.split_register) JStep.reread else JStep.register);
                },
                .post => {
                    if (self.killed[A]) return;
                    self.word.arm_open = false;
                    // C2 holds (JA is current). `cleared` once JA sees itself
                    // registered (§4.5.4); a registration a newer preempt-all
                    // already removed re-classifies instead.
                    if (!self.reg[A]) {
                        self.j_pc = @backingInt(JStep.classify);
                        return;
                    }
                    if (!self.word.clear()) {
                        if (self.j_fresh_reg) self.unregister(A);
                        return self.latchJA();
                    }
                    self.noteMgrWriter();
                    if (self.word.st == .pending) self.cleared_after_establish = true;
                    self.j_pc = @backingInt(if (self.j_held) JStep.retry else JStep.idle);
                },
                .retry => {
                    if (!self.retryLeft()) return self.latchJA();
                    self.j_retries += 1;
                    self.j_pc = @backingInt(JStep.write);
                },
                .done => unreachable,
            }
        }

        /// The classifier (§4.5.3), R-1 first (JA is current throughout).
        fn jaClassify(self: *Self, unreadable: bool) void {
            if (self.rtype == 0) {
                // R-2: a lapse — a held write retries within the budget (a
                // lapse observed refills it); the lapse beat re-registers.
                self.noteLapseSeen();
                if (self.j_held) {
                    self.j_pc = @backingInt(JStep.retry);
                } else self.j_pc = @backingInt(JStep.rule);
                return;
            }
            if (self.reg[A]) {
                // R-2: registered again — observing, R-1 first.
                return self.jaObservedRegistered(unreadable);
            }
            // R-3: succession (page 0 names another writer) or undecided:
            // held, the word pending.
            self.word.pend();
            self.j_pc = @backingInt(JStep.await_reassert);
        }

        /// An observing path found host A registered: R-1 (three-valued) —
        /// Live: `cleared`, retry; unreadable: the word stays as it is (the
        /// re-examiner retries); not Live: never (JA is current).
        fn jaObservedRegistered(self: *Self, unreadable: bool) void {
            if (unreadable) {
                if (cfg.r1_unreadable == .terminal) {
                    self.word.terminate(false);
                    return self.latchJA();
                }
                return;
            }
            _ = self.word.clear();
            self.j_pc = @backingInt(if (self.j_held) JStep.retry else JStep.idle);
        }

        // ---- the split JA: JA.op ----

        fn op(self: *Self, unreadable: bool) void {
            const pc: OStep = @fromBackingInt(@intCast(self.j_pc));
            switch (pc) {
                .read => {
                    // A node read: INTERNAL during the drain (P9b).
                    if (self.killed[A]) {
                        if (cfg.read_eio == .latch) return self.latchJA();
                        self.j_pc = @backingInt(OStep.read_barrier);
                        return;
                    }
                    self.j_pc = @backingInt(OStep.write);
                },
                .read_barrier => {
                    if (self.prBusy()) return;
                    // Unknown: re-issued on its own bound; then the retried
                    // read passes (reads need no registration).
                    if (unreadable) return;
                    self.j_pc = @backingInt(OStep.write);
                },
                .write => {
                    if (self.admits(A) and !self.killed[A]) {
                        self.j_w = .admitted;
                        self.j_pc = @backingInt(OStep.land);
                        return;
                    }
                    if (self.killed[A] and self.admits(A)) {
                        self.j_pc = @backingInt(OStep.capture);
                        self.deferred = true; // the EIO arm: its barrier next
                        return;
                    }
                    self.noteRefusal();
                    self.deferred = false;
                    self.j_pc = @backingInt(OStep.capture);
                },
                .land => {
                    self.j_w = .landed;
                    self.waiter.reset();
                    self.j_pc = @backingInt(OStep.done);
                },
                .capture => {
                    if (cfg.gen_capture == .at_refusal) self.waiter.capture(&self.word);
                    self.j_pc = @backingInt(if (self.deferred) OStep.eio_barrier else OStep.verdict);
                },
                .eio_barrier => {
                    if (self.prBusy()) return;
                    if (unreadable) {
                        // Unknown (P15a): held, the word pending (deferred),
                        // store-then-recheck (W32: read as a genuine EIO).
                        if (cfg.barrier_unknown_as_eio) return self.latchJA();
                        self.waiter.held = true;
                        self.j_pc = @backingInt(if (cfg.meta_defer_pending) OStep.pend else OStep.recheck);
                        return;
                    }
                    self.deferred = false;
                    if (self.reg[A]) {
                        // The EIO was transient (§7.1): re-issued.
                        self.waiter.reset();
                        self.j_pc = @backingInt(OStep.write);
                        return;
                    }
                    // Unregistered: P9c — classified as a refusal.
                    self.j_pc = @backingInt(OStep.verdict);
                },
                .verdict => {
                    if (self.killed[A]) {
                        // The classifier's raw reads meet the drain: one
                        // barrier and one re-read inside the hook.
                        self.j_pc = @backingInt(OStep.verdict_barrier);
                        return;
                    }
                    self.opClassify(unreadable);
                },
                .verdict_barrier => {
                    if (self.prBusy()) return;
                    if (unreadable) {
                        // Still unreadable: held, classified off the I/O path
                        // by the beat's next tick.
                        self.waiter.held = true;
                        self.j_pc = @backingInt(if (cfg.meta_defer_pending) OStep.pend else OStep.recheck);
                        return;
                    }
                    self.opClassify(false);
                },
                .pend => {
                    self.word.pend();
                    self.word.recheck_due = true;
                    self.j_pc = @backingInt(OStep.recheck);
                },
                .recheck => {
                    self.word.recheck_due = false;
                    // Store-then-recheck: the registration and R-1 re-read
                    // (R-1 unreadable during host A's drain, P9b).
                    if (self.word.st == .pending and self.rtype == 5 and self.reg[A] and !self.killed[A]) {
                        if (unreadable) {
                            if (cfg.r1_unreadable == .terminal) {
                                self.word.terminate(false);
                                return self.latchJA();
                            }
                        } else _ = self.word.clear();
                    }
                    self.j_pc = @backingInt(if (cfg.gen_capture == .after_verdict) OStep.record else OStep.pre_report);
                },
                .record => {
                    self.waiter.capture(&self.word);
                    self.j_pc = @backingInt(OStep.pre_report);
                },
                .pre_report => {
                    if (self.waiter.wakes(&self.word, cfg.resolution_gen)) return self.opWake();
                    self.j_pc = @backingInt(if (self.rtype == 5 and self.reg[A]) OStep.pre_r1 else OStep.wait);
                },
                .pre_r1 => {
                    if (unreadable or self.killed[A]) {
                        if (cfg.r1_unreadable == .terminal) {
                            self.word.terminate(false);
                            return self.latchJA();
                        }
                        self.j_pc = @backingInt(OStep.wait);
                        return;
                    }
                    // registered@g: `cleared` (gen++), then act.
                    _ = self.word.clear();
                    return self.opWake();
                },
                .wait => {
                    if (!self.waiter.wakes(&self.word, cfg.resolution_gen)) {
                        self.waiter.sleeping = true;
                        return;
                    }
                    self.opWake();
                },
                .retry => {
                    if (!self.retryLeft()) return self.latchJA();
                    self.j_retries += 1;
                    self.waiter.reset();
                    self.j_pc = @backingInt(OStep.write);
                },
                .done => unreachable,
            }
        }

        /// The verdict, with its inputs read (§4.5.3).
        fn opClassify(self: *Self, unreadable: bool) void {
            if (self.rtype == 0) {
                self.noteLapseSeen();
                self.waiter.held = true;
                self.j_pc = @backingInt(OStep.retry);
                return;
            }
            if (self.reg[A]) {
                // R-2 registered again: R-1, then `cleared` and retry; an
                // unreadable R-1 leaves the word for the re-examiner.
                self.waiter.held = true;
                if (unreadable) {
                    if (cfg.r1_unreadable == .terminal) {
                        self.word.terminate(false);
                        return self.latchJA();
                    }
                    self.j_pc = @backingInt(OStep.pend);
                    return;
                }
                _ = self.word.clear();
                self.j_pc = @backingInt(OStep.retry);
                return;
            }
            self.waiter.held = true;
            self.j_pc = @backingInt(OStep.pend);
        }

        /// The held operation woke: `terminal` takes its failure path; a
        /// newer `cleared`, its retry.
        fn opWake(self: *Self) void {
            if (self.word.st == .terminal) return self.latchJA();
            self.j_pc = @backingInt(OStep.retry);
        }

        // ---- the split JA: JA.beat (the establishing arm) ----

        fn beat(self: *Self) void {
            if (self.j_failed) return;
            // `terminal`: the member left the park other than by
            // re-registering (Latched, Expired).
            if (self.word.st == .terminal) return self.latchJA();
            const pc: BStep = @fromBackingInt(@intCast(self.b_pc));
            switch (pc) {
                .idle => {
                    if (!(self.rtype == 5 and !self.reg[A])) return;
                    // S3: the classifier marks the volume pending a succession;
                    // its store-then-recheck is the next step's re-read.
                    self.word.pend();
                    self.word.recheck_due = true;
                },
                .await_reassert => {
                    self.word.recheck_due = false;
                    if (self.reg[A] and self.rtype == 5) {
                        self.b_pc = @backingInt(BStep.idle);
                        return;
                    }
                    if (cfg.reassert_gate) {
                        const target = self.reassertTarget();
                        if (target == NONE) return;
                    }
                },
                .rule => {
                    if (!cfg.reregister or self.killed[A]) return;
                    if (self.reg[A]) {
                        // An adoption (`registerUnderRule`).
                        self.b_fresh_reg = false;
                        self.reg_writer = self.page0;
                        self.word.arm_open = true;
                        self.b_pc = @backingInt(BStep.post);
                        return;
                    }
                    self.b_killed_m0 = self.ruleKillsM0();
                },
                .register => {
                    if (self.prBusy()) return;
                    if (self.b_killed_m0 and self.m0Draining()) return;
                    if (!self.ruleLock(T_JB)) return;
                    if (self.registerRereadKills()) {
                        self.b_killed_m0 = true;
                        self.unlockA(T_JB);
                        return;
                    }
                    self.b_fresh_reg = !self.reg[A];
                    if (self.b_fresh_reg) self.ruleRegisterA(false);
                    self.reg_writer = self.page0;
                    self.word.arm_open = true;
                },
                .rejudge => {
                    self.window_a = false;
                    self.unlockA(T_JB);
                },
                .post => {
                    // C2's page reads wait out a drain of host A (P9b). C2
                    // holds; `cleared` once JA sees itself registered
                    // (§4.5.4); a registration a newer preempt-all already
                    // removed leaves the word to the next arm.
                    if (self.killed[A]) return;
                    self.word.arm_open = false;
                    self.b_pc = @backingInt(BStep.idle);
                    if (!(self.rtype == 5 and self.reg[A])) return;
                    if (!self.word.clear()) {
                        // A late `cleared` refused after `terminal`: the fresh
                        // registration withdrawn.
                        if (self.b_fresh_reg) {
                            self.unregister(A);
                            self.explained_a = true;
                        }
                        return;
                    }
                    self.noteMgrWriter();
                    if (self.word.st == .pending) self.cleared_after_establish = true;
                    return;
                },
            }
            self.b_pc += 1;
        }

        // ---- JA2 ----

        fn joiner2(self: *Self) void {
            const pc: J2Step = @fromBackingInt(@intCast(self.j2_pc));
            switch (pc) {
                .rule => if (self.reg[A]) {
                    self.j2_pc = @backingInt(J2Step.done);
                    return;
                },
                .register => {
                    if (self.prBusy()) return;
                    if (!self.ruleLock(T_JA2)) return;
                    self.j2_fresh = !self.reg[A];
                    if (self.j2_fresh) self.ruleRegisterA(false);
                },
                .rejudge => {
                    self.window_a = false;
                    self.unlockA(T_JA2);
                },
                .post => {},
                .done => unreachable,
            }
            self.j2_pc += 1;
        }

        // ------------------------------------------------------------------
        // Rule L's environment.
        // ------------------------------------------------------------------

        /// `elapsed` for JA's word: in `all_elapsed`; in `page_unreadable`
        /// once S1 fail-stopped at the bound (the premise's own stall: the set
        /// left managerless, pin 7).
        fn elapsedApplies(self: *const Self) bool {
            const on = cfg.all_elapsed or (cfg.page_unreadable and self.failed[1]);
            return on and HAS_JA and !self.word.el_fired and self.word.st == .pending;
        }

        /// No other thread's step changes the state (the quiescence tick's
        /// condition); TICK and its unreadable alternative excluded.
        fn othersQuiescent(self: *const Self) bool {
            for (0..thread_count) |t| {
                if (t == T_TICK or t == T_UTICK or self.done(t)) continue;
                var next = self.*;
                next.step(t);
                if (!std.mem.eql(u8, std.mem.asBytes(&next), std.mem.asBytes(self))) return false;
            }
            return true;
        }

        /// The tick may run: within its budget, or a quiescence tick.
        fn tickRuns(self: *const Self) bool {
            if (!self.waitingAny()) return false;
            return self.ticks < cfg.ticks or self.othersQuiescent();
        }

        /// The re-examiner's reaction would read R-1 for JA's word (the
        /// unreadable alternative's precondition).
        fn tickReadsR1(self: *const Self) bool {
            return self.word.st == .pending and !self.word.pastDeadline() and !self.killed[A] and (self.rtype == 0 or self.reg[A]);
        }

        fn uTickApplies(self: *const Self) bool {
            return HAS_JA and !self.word.unread_used and self.tickReadsR1() and self.tickRuns();
        }

        /// `tick`: the budget counted (saturating at a quiescence tick), then
        /// the member-fence beat's reaction for JA's word in the same step —
        /// the deadline's `terminal` (whatever the reads), else the
        /// registration re-read and R-1 (an observing path: Live ⇒
        /// `cleared`; unreadable ⇒ the word stays pending).
        fn tick(self: *Self, unreadable: bool) void {
            if (!self.tickRuns()) return;
            if (self.ticks < cfg.ticks) self.ticks += 1;
            if (unreadable) self.word.unread_used = true;
            if (self.word.pastDeadline()) {
                self.word.terminate(true);
            } else if (self.word.st == .pending and (self.rtype == 0 or self.reg[A])) {
                if (!unreadable and !self.killed[A]) _ = self.word.clear();
            }
            if (self.word.el_fired) self.word.el_ticked = true;
            if (!unreadable and !self.killed[A] and self.word.anchors() and self.wordWaiting() and self.regCurrentA()) self.word.rtick = true;
        }

        // ------------------------------------------------------------------
        // The alternatives' preconditions: the step they replace is next.
        // ------------------------------------------------------------------

        fn uOpApplies(self: *const Self) bool {
            if (!HAS_JA or self.j_failed or self.word.unread_used) return false;
            // During host A's drain its reads fail anyway (P9b).
            if (self.killed[A]) return false;
            if (!SPLIT) {
                const pc: JStep = @fromBackingInt(@intCast(self.j_pc));
                return (pc == .classify or pc == .await_reassert) and self.rtype == 5 and self.reg[A];
            }
            const pc: OStep = @fromBackingInt(@intCast(self.j_pc));
            return switch (pc) {
                .verdict => self.rtype == 5 and self.reg[A],
                .recheck => self.word.st == .pending and self.rtype == 5 and self.reg[A],
                .pre_r1 => true,
                else => false,
            };
        }

        fn uBarApplies(self: *const Self) bool {
            if (!cfg.barrier_unknown or self.bar_used or self.j_failed or self.prBusy()) return false;
            const pc: OStep = @fromBackingInt(@intCast(self.j_pc));
            return pc == .read_barrier or pc == .eio_barrier or pc == .verdict_barrier;
        }

        fn uPageApplies(self: *const Self) bool {
            if (!cfg.page_unreadable or !self.alive[1] or self.failed[1]) return false;
            return ORDER[0][self.s_pc[0]] == .beat;
        }

        // ------------------------------------------------------------------
        // Invariants.
        // ------------------------------------------------------------------

        /// Row 4's state part: two bound incarnations, alive and not
        /// fail-stopped, both able to write (their hosts registered under a
        /// standing reservation); A9 exempts a blindly re-admitted M0's
        /// writes.
        fn twoManagersNow(self: *const Self) bool {
            var n: u8 = 0;
            for (0..3) |i| {
                const mi: u8 = @intCast(i);
                if (!self.liveBound(mi)) continue;
                const h = HOST[mi];
                if (!(self.rtype == 5 and self.reg[h])) continue;
                if (mi == M0 and self.blind_a) continue;
                if (self.inA3(mi)) continue;
                // A host inside a rule's register window (A5): its re-judge
                // kills or withdraws (as `host_fence_model.Election`).
                if (h == A and self.window_a and !cfg.strict.register_window) continue;
                n += 1;
            }
            return n > 1;
        }

        /// Row 5: unreserved while a current manager lives, outside A3.
        fn unreservedUnderManager(self: *const Self) bool {
            if (self.rtype != 0) return false;
            for (0..3) |i| {
                const mi: u8 = @intCast(i);
                if (self.currentManager(mi) and !self.inA3(mi)) return true;
            }
            return false;
        }

        /// A3 for incarnation `i`: a target restart happened and `i` has not
        /// acquired since.
        fn inA3(self: *const Self, i: u8) bool {
            return self.restarted and !self.reacq[i];
        }

        pub fn invariant(self: *const Self) !void {
            var r: Rows = .{};
            r.killed_current = self.killed_current;
            r.preempted_live = self.preempted_live;
            r.wrote_over_live = self.wrote_over_live;
            r.two_managers = self.two_managers or self.twoManagersNow();
            r.unreserved_under_manager = self.unreservedUnderManager();
            r.overwrote_acked = self.overwrote_acked;
            r.fenced_wrote = self.fenced_wrote;
            r.live_fenced = self.live_fenced;
            r.cleared_after_establish = self.cleared_after_establish;
            r.mgr_failstop_unproven = self.mfsu;
            if (HAS_JA) {
                r.registered_but_pending = registeredButPending(&self.word, self.wordWaiting(), self.regCurrentA());
                r.pending_past_deadline = pendingPastDeadline(&self.word, if (SPLIT) &self.waiter else null, cfg.resolution_gen);
            }
            r.hit_lapse = self.hit_lapse;
            r.hit_register = self.hit_register;
            r.hit_probe = self.hit_probe;
            r.hit_blind = self.hit_blind;
            return checkRows(r, cfg.strict);
        }

        pub fn final(self: *const Self) !void {
            // LiveFenced's final part: JA (current throughout) unregistered on
            // its namespace, or parked, with no `elapsed` for its word.
            if (HAS_JA and !self.j_failed and !self.word.el_fired) {
                if (self.rtype == 5 and !self.reg[A]) return error.LiveFenced;
                if (self.word.st == .pending) return error.LiveFenced;
            }
        }

        pub fn stuck(_: *const Self) !void {
            return error.StrandedAtQuiescence;
        }
    };
}

// ---------------------------------------------------------------------------
// `Debt` — the superseded manager's data: the durable fence debt paid at the
// successor's data admission, through a chain of successions; the
// superseded manager's co-located joiner held on data while its metadata
// re-registration is pending (§4.4.4, §4.5.5, §4.7).
// ---------------------------------------------------------------------------

pub const DataRefusal = enum {
    /// A data refusal while the metadata re-registration is pending answers
    /// `held` (as built).
    held,
    /// W34: it declines (revision 4's behaviour).
    decline_while_pending,
};
pub const SyncArmFailure = enum {
    /// A failed metadata re-check of the synchronous arm keeps the data
    /// registration, stores `cleared` and retries (DC23, revision 17).
    keep_registration,
    /// W51: revision 15's arm declines (the data device latches).
    decline,
};
pub const DataBeat = enum {
    /// The data beat reacts to every periodic tick and every kick (as built).
    periodic,
    /// W38, W40: only to kicks (the deadline reaction kept).
    kick_only,
};

/// `Debt{.ja}`'s operations (revision 18's split: each weakening's witness
/// lies within one operation; the full form is a one-off measurement).
pub const DebtOp = enum { both, one, two };

pub const DebtConfig = struct {
    /// S1's bind lands between S2's probe 2 and S2's preempt-all; S1 then
    /// freezes with a data write before paying (W3's configuration).
    bind_race: bool = false,
    /// JA, the joiner co-located with M on host A: its data writes, its
    /// metadata member fence, its data fence beat.
    ja: bool = false,
    /// (with `ja`) the first data refusal's classification inputs unreadable.
    unreadable: bool = false,
    /// (with `ja`) `elapsed` for every word.
    all_elapsed: bool = false,
    /// (with `ja`) JA's operations: op 1 alone, op 2 alone, or both.
    op: DebtOp = .both,
    /// Trims (§7.4's ledger): S1's data write; M's data write as one step;
    /// M's data write off (the op-1 forms).
    s1_write: bool = true,
    m_write_split: bool = true,
    m_data_write: bool = true,
    ticks: u8 = 2,
    // ---- weakenings ----
    debt: bool = true,
    debt_chain: bool = true,
    probe_under_hold: bool = true,
    data_refusal: DataRefusal = .held,
    gen_capture: GenCapture = .at_refusal,
    store_then_recheck: bool = true,
    data_beat: DataBeat = .periodic,
    sync_arm_clears: bool = true,
    sync_arm_failure: SyncArmFailure = .keep_registration,
    strict: Strict = .{},
};

/// A successor's steps in `Debt`: the metadata takeover (probe 2, the
/// preempt-all, probe 3 naming the predecessor), the bind (claim, the debt
/// written, appender 0's page), the data admission (its data registration
/// and its metadata re-check, the page check, the debt paid key by key:
/// fence and proof), the allocation (the superseded writers' blocks
/// reallocated), its own data write.
const DtStep = enum(u8) { probe2, preempt, probe3, bind, dreg, drecheck, pay, realloc, w_issue, w_admit, w_land, done };

/// JA's held data write (op 1), its synchronous-arm write (op 2), its
/// metadata member fence (JM) and its data fence beat (DB).
const JdStep = enum(u8) { issue, arrive, land, capture, verdict, pend, recheck, record, pre_report, pre_r1, wait, retry, sync_register, sync_rejudge, done };
const JmStep = enum(u8) { idle, await_reassert, rule, register, post };
const DbStep = enum(u8) { idle, register, rejudge };

/// `Debt` — hosts A (M, the superseded manager, frozen with a data write to
/// its block; JA beside it with `ja`), B (S1), C (S2); one metadata namespace
/// (the takeovers' preempt-alls, one step each here: their drain is
/// `Succession`'s), one data namespace (WEAR):
///   0 M    its data write (issue; arrival; landing).
///   1 S1   probe 2, the preempt-all, probe 3, the bind with debt {K_A},
///          the data admission paying the debt, M's block reallocated, its
///          own data write — freezable anywhere (its claim ages: thread 7).
///   2 S2   the same over S1 once S1's claim aged (`bind_race`: S1's bind
///          between S2's probe 2 and its preempt-all), the debt ∪ {K_pred}
///          (probe 3 names the predecessor), M's and S1's blocks
///          reallocated.
///   3 JA.op1  (`ja`) a data write issued at any point: refused ⇒ the gen
///          captured, `classifyForData`, `held` with the data word pending
///          (store-then-recheck), the pre-sleep re-check, the wait, one retry.
///   4 JA.op2  (`ja`) a second data write after JA's metadata re-registration:
///          refused ⇒ §DL.3's synchronous arm (the data rule's register and
///          re-judge, `cleared`), the retry; a failed metadata re-check
///          keeps the data registration (DC23).
///   5 JM   (`ja`) JA's metadata member fence: S3, re-assertion, the rule
///          (M killed, its nodes drained), the register, the post-check and
///          `cleared`, the data beat kicked.
///   6 DB   (`ja`) the data fence beat: on a kick (the metadata `cleared`, a
///          tick, a store-then-recheck) the data rule's register and
///          re-judge, `cleared`.
///   7 AGE  S1's claim ages past its TTL (S1 frozen), once, after its bind.
///   8 TICK, 9 ELAPSED (the data word), 10 ELAPSED (the metadata word),
///   11 an unreadable tick, 12 an unreadable R-1 of op 1 (the data word).
pub fn Debt(comptime cfg: DebtConfig) type {
    return extern struct {
        const Self = @This();
        pub const thread_count: usize = 13;
        const T_M = 0;
        const T_S1 = 1;
        const T_S2 = 2;
        const T_OP1 = 3;
        const T_OP2 = 4;
        const T_JM = 5;
        const T_DB = 6;
        const T_AGE = 7;
        const T_TICK = 8;
        const T_ELD = 9;
        const T_ELM = 10;
        const T_UTICK = 11;
        const T_UOP = 12;
        const HOST = [3]u8{ A, B, C };
        const MI: u8 = 0;

        comptime {
            if ((cfg.unreadable or cfg.all_elapsed or cfg.op != .both) and !cfg.ja) @compileError("unreadable, all_elapsed and op ride ja");
        }

        // ---- the metadata namespace ----
        mreg: [3]bool = .{ true, false, false },
        claim: u8 = MI,
        page0: u8 = MI,
        alive: [3]bool = @splat(true),
        bound: [3]bool = .{ true, false, false },
        /// The incarnation's claim aged past its TTL (M's from the start).
        aged: [3]bool = .{ true, false, false },
        // ---- the data namespace ----
        dreg: [3]bool = .{ true, false, false },
        /// The durable fence debt (bit per host key), volume 0's tree 0.
        debt: u8 = 0,
        /// Blocks reallocated: M's, S1's.
        realloc: [2]bool = @splat(false),
        /// Data writes: M's, S1's, JA's two.
        mw: Wr = if (cfg.m_data_write) .idle else .none,
        sw: [2]Wr = .{ if (cfg.s1_write) .idle else .none, .none },
        jw: [2]Wr = .{ if (cfg.ja) .idle else .none, if (cfg.ja) .idle else .none },
        /// M's data write admitted before its host's debt was paid while M
        /// was superseded (A2).
        mw_debt: bool = false,
        // ---- the successors ----
        s_pc: [2]u8 = .{ 0, 0 },
        /// The claim's writer probe 2 and probe 3 read (the predecessor).
        pred2: [2]u8 = @splat(NONE),
        pred: [2]u8 = @splat(NONE),
        /// The admission's working debt.
        owe: [2]u8 = @splat(0),
        sup_a: bool = false,
        // ---- JA ----
        op_pc: [2]u8 = .{
            if (cfg.ja and cfg.op != .two) 0 else @backingInt(JdStep.done),
            if (cfg.ja and cfg.op != .one) 0 else @backingInt(JdStep.done),
        },
        jm_pc: u8 = 0,
        db_pc: u8 = 0,
        j_failed: bool = false,
        /// JA's metadata re-registration completed once (op 2 issues after).
        j_reregistered: bool = false,
        /// The data beat's level kick.
        kick: bool = false,
        /// op 1's classification deferred (inputs unreadable).
        deferred: bool = false,
        dword: Word = .{},
        mword: Word = .{},
        waiter: Waiter = .{},
        /// A succession-driven data fence (the debt) unregistered host A on
        /// data since its last data registration (row 13a's explanation).
        d_explained_a: bool = false,
        ticks: u8 = 0,
        // ---- flags ----
        realloc_unfenced: bool = false,
        stale_data_write: bool = false,
        live_fenced: bool = false,
        cleared_after_establish: bool = false,
        hit_debt: bool = false,

        pub fn init() Self {
            var s: Self = .{};
            if (!cfg.ja) {
                s.jm_pc = @backingInt(JmStep.idle);
                s.j_failed = true;
            }
            return s;
        }

        // ------------------------------------------------------------------
        // Thread status.
        // ------------------------------------------------------------------

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                T_M => self.mw == .landed or self.mw == .refused or self.mw == .none,
                T_S1, T_S2 => blk: {
                    const s = t - T_S1;
                    const st: DtStep = @fromBackingInt(@intCast(self.s_pc[s]));
                    break :blk st == .done or !self.alive[s + 1];
                },
                T_OP1, T_OP2 => blk: {
                    const k = t - T_OP1;
                    if (self.j_failed) break :blk self.jw[k] != .admitted;
                    break :blk self.op_pc[k] == @backingInt(JdStep.done);
                },
                T_JM => !cfg.ja or self.j_failed or self.jmDone(),
                T_DB => !cfg.ja or self.j_failed or self.dbDone(),
                T_AGE => self.aged[1] or !self.bound[1],
                T_TICK => !cfg.ja or !self.waitingAny(),
                T_ELD => !(cfg.all_elapsed and !self.dword.el_fired and self.dword.st == .pending),
                T_ELM => !(cfg.all_elapsed and !self.mword.el_fired and self.mword.st == .pending),
                T_UTICK => !self.uTickApplies(),
                T_UOP => !self.uOpApplies(),
                else => unreachable,
            };
        }

        fn jmDone(self: *const Self) bool {
            if (self.mword.st == .terminal) return false;
            return self.jm_pc == @backingInt(JmStep.idle) and self.mreg[A];
        }

        fn dbDone(self: *const Self) bool {
            if (self.dword.st == .terminal) return false;
            return self.db_pc == @backingInt(DbStep.idle) and !self.dbHasWork();
        }

        fn waitingAny(self: *const Self) bool {
            if (self.j_failed) return false;
            return self.dword.st == .pending or self.mword.st == .pending or self.waiter.held;
        }

        // ------------------------------------------------------------------
        // Steps.
        // ------------------------------------------------------------------

        pub fn step(self: *Self, t: usize) void {
            self.stepAs(t, false);
            self.post();
        }

        fn stepAs(self: *Self, t: usize, unreadable: bool) void {
            switch (t) {
                T_M => self.mWrite(),
                T_S1 => self.successor(0),
                T_S2 => self.successor(1),
                T_OP1 => self.op(0, unreadable),
                T_OP2 => self.op(1, false),
                T_JM => self.jm(),
                T_DB => self.db(),
                T_AGE => self.aged[1] = true,
                T_TICK => self.tick(false),
                T_ELD => self.dword.elapse(),
                T_ELM => self.mword.elapse(),
                T_UTICK => self.tick(true),
                T_UOP => {
                    self.dword.unread_used = true;
                    self.op(0, true);
                },
                else => unreachable,
            }
        }

        fn post(self: *Self) void {
            if (!(self.dWaiting() and self.dRegCurrent())) self.dword.rtick = false;
            if (!(self.mword.st == .pending and self.mRegCurrent())) self.mword.rtick = false;
            // A refused successor's program counter and working values are
            // read by no later step: canonical.
            for (0..2) |x| if (!self.alive[x + 1]) {
                self.s_pc[x] = @backingInt(DtStep.done);
                self.owe[x] = 0;
                self.pred[x] = NONE;
                self.pred2[x] = NONE;
            };
        }

        /// The data word waits: pending, or op 1 asleep and blocked.
        fn dWaiting(self: *const Self) bool {
            return self.dword.st == .pending or self.waiter.blocked(&self.dword, true);
        }

        /// Host A registered on the data namespace (reserved throughout), JA
        /// current.
        fn dRegCurrent(self: *const Self) bool {
            return cfg.ja and !self.j_failed and self.dreg[A];
        }

        fn mRegCurrent(self: *const Self) bool {
            return cfg.ja and !self.j_failed and self.mreg[A];
        }

        /// JA's metadata registration settled (on its one joined volume,
        /// under WEAR).
        fn metaSettled(self: *const Self) bool {
            return self.mreg[A];
        }

        // ---- M ----

        fn mWrite(self: *Self) void {
            switch (self.mw) {
                .idle => {
                    if (!self.alive[MI]) {
                        self.mw = .none;
                        return;
                    }
                    if (cfg.m_write_split) {
                        self.mw = .queued;
                        return;
                    }
                    // Admit and land in one step (the ledger's trim).
                    if (!self.dreg[A]) {
                        self.mw = .refused;
                        return;
                    }
                    self.mw_debt = self.sup_a;
                    self.mLands();
                },
                .queued => {
                    if (!self.dreg[A]) {
                        self.mw = .refused;
                        return;
                    }
                    self.mw = .admitted;
                    self.mw_debt = self.sup_a;
                },
                .admitted => self.mLands(),
                else => unreachable,
            }
        }

        fn mLands(self: *Self) void {
            self.mw = .landed;
            defer self.mw_debt = false;
            if (self.realloc[0]) {
                self.stale_data_write = true;
            } else if (self.mw_debt) self.hit_debt = true;
        }

        // ---- the successors ----

        fn claimFreshForeign(self: *const Self, i: u8) bool {
            const c = self.claim;
            return c != NONE and c != i and !self.aged[c] and self.alive[c] and self.bound[c];
        }

        fn successor(self: *Self, s: usize) void {
            const i: u8 = @intCast(s + 1);
            const h = HOST[i];
            const st: DtStep = @fromBackingInt(@intCast(self.s_pc[s]));
            switch (st) {
                .probe2 => {
                    // S2 (outside `bind_race`) opens once S1 bound: its first
                    // probe after S1's tenure began.
                    if (s == 1 and !cfg.bind_race and !self.bound[1] and self.alive[1]) return;
                    if (self.claimFreshForeign(i)) {
                        self.alive[i] = false;
                        return;
                    }
                    self.pred2[s] = self.claim;
                    self.mreg[h] = true;
                },
                .preempt => {
                    // The preempt-all (P4 + P9, its drain `Succession`'s).
                    if (!self.mreg[h]) {
                        self.alive[i] = false;
                        return;
                    }
                    for (0..3) |x| if (x != h and self.mreg[x]) {
                        self.mreg[x] = false;
                        if (x == A) self.sup_a = true;
                    };
                },
                .probe3 => {
                    // Under the hold: a fresh claim whose writer this open's
                    // preempt fenced is taken over; it names the predecessor
                    // (W3: probe 2's reading).
                    if (self.claimFreshForeign(i) and self.mreg[HOST[self.claim]]) {
                        self.alive[i] = false;
                        return;
                    }
                    self.pred[s] = if (cfg.probe_under_hold) self.claim else self.pred2[s];
                    self.pred2[s] = NONE;
                },
                .bind => {
                    if (!self.mreg[h]) {
                        self.alive[i] = false;
                        return;
                    }
                    // The claim and the debt in one transaction (§4.4 step
                    // 10): debt ∪ {predecessor's page key} \\ {own}.
                    const p = self.pred[s];
                    var d: u8 = if (cfg.debt_chain) self.debt else 0;
                    if (p != NONE and HOST[p] != h) d |= @as(u8, 1) << @intCast(HOST[p]);
                    if (cfg.debt) self.debt = d;
                    self.owe[s] = if (cfg.debt) d else 0;
                    self.pred[s] = NONE;
                    self.claim = i;
                    self.page0 = i;
                    self.bound[i] = true;
                },
                .dreg => self.dreg[h] = true,
                .drecheck => {
                    // The admission's metadata re-check (withdrawn when a
                    // newer preempt-all unregistered the host), then the page
                    // check before the debt payment (§4.4.2, bounded
                    // re-reads; a superseded successor's page names another
                    // writer).
                    if (!self.mreg[h] or self.page0 != i) {
                        self.dreg[h] = false;
                        self.alive[i] = false;
                        return;
                    }
                },
                .pay => {
                    // fence(k) on data for each owed key, one key per step
                    // (P5 + P9: its admitted writes drained first), then the
                    // report proving it absent; a paid key leaves the debt.
                    // The page check guards each fence (§5.6).
                    if (self.page0 != i) {
                        self.alive[i] = false;
                        return;
                    }
                    var k: u8 = 0;
                    while (k < 3) : (k += 1) {
                        const bit = @as(u8, 1) << @intCast(k);
                        if (self.owe[s] & bit == 0) continue;
                        if (self.dAdmitted(k)) return;
                        if (self.dreg[k]) {
                            self.dreg[k] = false;
                            if (k == A) self.d_explained_a = true;
                        }
                        self.owe[s] &= ~bit;
                        self.debt &= ~bit;
                        return;
                    }
                },
                .realloc => {
                    // DataPlane.open, then the superseded tenures' blocks
                    // its replay found: M's, and (S2) S1's once S1 bound.
                    // An allocation is a metadata commit: refused once a
                    // newer preempt-all unregistered the host.
                    if (!self.mreg[h]) {
                        self.alive[i] = false;
                        return;
                    }
                    self.reallocate(0);
                    if (s == 1 and self.bound[1]) self.reallocate(1);
                },
                .w_issue => {
                    if (s == 1 or self.sw[0] == .none) {
                        self.s_pc[s] = @backingInt(DtStep.done);
                        return;
                    }
                    self.sw[0] = .queued;
                },
                .w_admit => {
                    if (!self.dreg[B]) {
                        self.sw[0] = .refused;
                        self.s_pc[s] = @backingInt(DtStep.done);
                        return;
                    }
                    self.sw[0] = .admitted;
                },
                .w_land => {
                    self.sw[0] = .landed;
                    if (self.realloc[1]) self.stale_data_write = true;
                },
                .done => unreachable,
            }
            if (st == .realloc and s == 0 and !cfg.s1_write) {
                self.s_pc[s] = @backingInt(DtStep.done);
                return;
            }
            self.s_pc[s] += 1;
        }

        /// A data write of host `k` admitted at the target.
        fn dAdmitted(self: *const Self, k: u8) bool {
            return switch (k) {
                A => self.mw == .admitted or self.jw[0] == .admitted or self.jw[1] == .admitted,
                B => self.sw[0] == .admitted,
                else => false,
            };
        }

        /// Block `b` (M's: 0, S1's: 1) reallocated: its writer must not be
        /// able to land there any more.
        fn reallocate(self: *Self, b: usize) void {
            if (self.realloc[b]) return;
            const can_land = if (b == 0) self.canLand(self.mw, self.alive[MI], A) else self.s1CanLand();
            if (can_land) self.realloc_unfenced = true;
            self.realloc[b] = true;
        }

        /// S1's write is issued only past its admission: before its metadata
        /// re-check S1 can land only if that re-check would still pass (its
        /// host registered on metadata, appender 0's page naming it).
        fn s1CanLand(self: *const Self) bool {
            if (self.sw[0] == .idle and self.alive[1] and self.s_pc[0] <= @backingInt(DtStep.drecheck)) {
                return self.mreg[B] and self.page0 == 1;
            }
            return self.canLand(self.sw[0], self.alive[1], B);
        }

        fn canLand(self: *const Self, w: Wr, alive: bool, h: u8) bool {
            return switch (w) {
                .admitted => true,
                .queued => self.dreg[h],
                .idle => alive and self.dreg[h],
                .landed, .refused, .none => false,
            };
        }

        // ---- JA ----

        /// A latch of JA's data device or a member fail-stop (row 13b);
        /// exempt: a deadline's own terminal (pin 1).
        fn latchJA(self: *Self, w: *const Word) void {
            self.j_failed = true;
            if (!(w.st == .terminal and w.by_deadline)) self.live_fenced = true;
            self.waiter.reset();
        }

        /// The data rule's register of host A (the stale-sibling rule kills
        /// M and drains the data node first). False: waiting for the drain.
        fn dataRuleRegister(self: *Self) bool {
            if (self.alive[MI] and self.page0 != MI) self.alive[MI] = false;
            if (!self.alive[MI] and (self.mw == .queued or self.mw == .admitted)) return false;
            if (!self.dreg[A]) {
                self.dreg[A] = true;
                self.d_explained_a = false;
            }
            return true;
        }

        fn op(self: *Self, k: usize, unreadable: bool) void {
            const st: JdStep = @fromBackingInt(@intCast(self.op_pc[k]));
            switch (st) {
                .issue => {
                    // op 2 issues after JA's metadata re-registration.
                    if (k == 1 and !self.j_reregistered) return;
                    self.jw[k] = .queued;
                },
                .arrive => {
                    if (self.dreg[A]) {
                        self.jw[k] = .admitted;
                        self.op_pc[k] = @backingInt(JdStep.land);
                        return;
                    }
                    if (!self.d_explained_a) self.live_fenced = true;
                    self.jw[k] = .idle;
                    if (k == 1) {
                        // §DL.3's synchronous arm (metadata settled).
                        self.op_pc[k] = @backingInt(JdStep.sync_register);
                        return;
                    }
                    self.op_pc[k] = @backingInt(JdStep.capture);
                    return;
                },
                .land => {
                    self.jw[k] = .landed;
                    if (k == 0) {
                        self.waiter.reset();
                        self.deferred = false;
                    }
                    self.op_pc[k] = @backingInt(JdStep.done);
                    return;
                },
                .capture => {
                    if (cfg.gen_capture == .at_refusal) self.waiter.capture(&self.dword);
                },
                .verdict => {
                    // classifyForData (§4.5.5).
                    if (cfg.unreadable and !self.deferred) {
                        // Inputs unreadable: held, the word pending, the
                        // beat classifies at its next tick.
                        self.deferred = true;
                        self.waiter.held = true;
                        self.op_pc[k] = @backingInt(JdStep.pend);
                        return;
                    }
                    if (self.metaSettled()) {
                        // metadata_settled: the synchronous arm.
                        self.op_pc[k] = @backingInt(JdStep.sync_register);
                        return;
                    }
                    // metadata_pending: held (W34: decline, the latch).
                    if (cfg.data_refusal == .decline_while_pending) return self.latchJA(&self.dword);
                    self.waiter.held = true;
                    self.mword.pend();
                },
                .pend => {
                    self.dword.pend();
                    if (cfg.store_then_recheck) self.dword.recheck_due = true;
                },
                .recheck => {
                    // Store-then-recheck (W38: none): d holds the host ⇒
                    // `cleared` (R-1); metadata settled ⇒ the beat kicked.
                    self.dword.recheck_due = false;
                    if (cfg.store_then_recheck) {
                        if (self.dreg[A]) {
                            if (!unreadable) _ = self.dword.clear();
                        } else if (self.metaSettled()) self.kick = true;
                    }
                    if (cfg.gen_capture == .after_verdict) {
                        self.op_pc[k] = @backingInt(JdStep.record);
                        return;
                    }
                    self.op_pc[k] = @backingInt(JdStep.pre_report);
                    return;
                },
                .record => self.waiter.capture(&self.dword),
                .pre_report => {
                    if (self.waiter.wakes(&self.dword, true)) return self.opWake(k);
                    self.op_pc[k] = @backingInt(if (self.dreg[A]) JdStep.pre_r1 else JdStep.wait);
                    return;
                },
                .pre_r1 => {
                    if (unreadable) {
                        self.op_pc[k] = @backingInt(JdStep.wait);
                        return;
                    }
                    _ = self.dword.clear();
                    return self.opWake(k);
                },
                .wait => {
                    if (!self.waiter.wakes(&self.dword, true)) {
                        self.waiter.sleeping = true;
                        return;
                    }
                    return self.opWake(k);
                },
                .retry => {
                    self.waiter.reset();
                    self.op_pc[k] = @backingInt(JdStep.arrive);
                    self.jw[k] = .queued;
                    return;
                },
                .sync_register => {
                    if (!self.dataRuleRegister()) return;
                    self.dword.arm_open = true;
                },
                .sync_rejudge => {
                    self.dword.arm_open = false;
                    // A failed metadata re-check keeps the data registration
                    // (DC23); W51: revision 15's arm declines.
                    if (!self.metaSettled() and cfg.sync_arm_failure == .decline) return self.latchJA(&self.dword);
                    // L(c): `cleared` before the retry (W39: none).
                    if (cfg.sync_arm_clears) _ = self.dword.clear();
                    if (self.dword.st == .pending) self.cleared_after_establish = true;
                    self.op_pc[k] = @backingInt(JdStep.arrive);
                    self.jw[k] = .queued;
                    return;
                },
                .done => unreachable,
            }
            self.op_pc[k] += 1;
        }

        fn opWake(self: *Self, k: usize) void {
            if (self.dword.st == .terminal) return self.latchJA(&self.dword);
            self.op_pc[k] = @backingInt(JdStep.retry);
        }

        // ---- JM: JA's metadata member fence ----

        fn jm(self: *Self) void {
            if (self.mword.st == .terminal) return self.latchJA(&self.mword);
            const st: JmStep = @fromBackingInt(@intCast(self.jm_pc));
            switch (st) {
                .idle => {
                    if (self.mreg[A]) return;
                    self.mword.pend();
                },
                .await_reassert => {
                    if (self.mreg[A]) {
                        self.jm_pc = @backingInt(JmStep.idle);
                        return;
                    }
                    const p = self.page0;
                    if (p == MI or p == NONE or !self.alive[p] or !self.bound[p]) return;
                },
                .rule => {
                    // M's line stale (reserved, host A unregistered): killed;
                    // its nodes drained (C3).
                    if (self.alive[MI]) self.alive[MI] = false;
                },
                .register => {
                    if (self.mw == .queued or self.mw == .admitted) return;
                    self.mreg[A] = true;
                    self.mword.arm_open = true;
                },
                .post => {
                    self.jm_pc = @backingInt(JmStep.idle);
                    self.mword.arm_open = false;
                    if (!self.mreg[A]) return;
                    if (self.mword.clear()) {
                        self.j_reregistered = true;
                        // The member fence kicks the data beat on `cleared`.
                        self.kick = true;
                    }
                    if (self.mword.st == .pending) self.cleared_after_establish = true;
                    return;
                },
            }
            self.jm_pc += 1;
        }

        // ---- DB: the data fence beat ----

        fn dbHasWork(self: *const Self) bool {
            if (!self.kick) return false;
            if (self.dword.st == .pending and self.dreg[A]) return true;
            return !self.dreg[A] and self.metaSettled();
        }

        fn db(self: *Self) void {
            if (self.dword.st == .terminal) return self.latchJA(&self.dword);
            const st: DbStep = @fromBackingInt(@intCast(self.db_pc));
            switch (st) {
                .idle => {
                    if (!self.dbHasWork()) return;
                    if (self.dreg[A]) {
                        // Finding the host registered (observing: R-1).
                        _ = self.dword.clear();
                        self.kick = false;
                        return;
                    }
                },
                .register => {
                    if (!self.dataRuleRegister()) return;
                    self.dword.arm_open = true;
                },
                .rejudge => {
                    // The registration stands whatever the metadata re-check
                    // reads (DC23).
                    self.db_pc = @backingInt(DbStep.idle);
                    self.dword.arm_open = false;
                    _ = self.dword.clear();
                    self.kick = false;
                    if (self.dword.st == .pending) self.cleared_after_establish = true;
                    return;
                },
            }
            self.db_pc += 1;
        }

        // ---- rule L's environment ----

        fn othersQuiescent(self: *const Self) bool {
            for (0..thread_count) |t| {
                if (t == T_TICK or t == T_UTICK or self.done(t)) continue;
                var next = self.*;
                next.step(t);
                if (!std.mem.eql(u8, std.mem.asBytes(&next), std.mem.asBytes(self))) return false;
            }
            return true;
        }

        fn tickRuns(self: *const Self) bool {
            if (!self.waitingAny()) return false;
            return self.ticks < cfg.ticks or self.othersQuiescent();
        }

        fn uTickApplies(self: *const Self) bool {
            if (!cfg.ja or self.dword.unread_used or self.j_failed) return false;
            if (!(self.dword.st == .pending and !self.dword.pastDeadline() and self.dreg[A])) return false;
            return cfg.data_beat == .periodic and self.tickRuns();
        }

        fn uOpApplies(self: *const Self) bool {
            if (!cfg.ja or self.dword.unread_used or self.j_failed) return false;
            const st: JdStep = @fromBackingInt(@intCast(self.op_pc[0]));
            return switch (st) {
                .recheck => cfg.store_then_recheck and self.dreg[A],
                .pre_r1 => true,
                else => false,
            };
        }

        /// `tick`: each word's re-examiner in the same step — the deadline's
        /// `terminal` first; the data beat's periodic reaction (W38, W40:
        /// none): host A registered on d ⇒ R-1, `cleared`; metadata settled ⇒
        /// the beat kicked (its establishing arm runs as its own steps), a
        /// deferred classification re-run likewise; the member-fence beat's:
        /// host A registered on metadata ⇒ `cleared`.
        fn tick(self: *Self, unreadable: bool) void {
            if (!self.tickRuns()) return;
            if (self.ticks < cfg.ticks) self.ticks += 1;
            if (unreadable) self.dword.unread_used = true;
            if (self.dword.pastDeadline()) {
                self.dword.terminate(true);
            } else if (self.dword.st == .pending and cfg.data_beat == .periodic) {
                if (self.dreg[A]) {
                    if (!unreadable) _ = self.dword.clear();
                } else if (self.metaSettled()) self.kick = true;
            }
            if (self.mword.pastDeadline()) {
                self.mword.terminate(true);
            } else if (self.mword.st == .pending and self.mreg[A]) _ = self.mword.clear();
            if (self.dword.el_fired) self.dword.el_ticked = true;
            if (self.mword.el_fired) self.mword.el_ticked = true;
            // A kicked data beat with work is a resolution step in flight
            // too (a kick shortens the wait; the tick guarantees one).
            const db_due = self.dbHasWork();
            if (!unreadable and !db_due and self.dword.anchors() and self.dWaiting() and self.dRegCurrent()) self.dword.rtick = true;
            if (self.mword.anchors() and self.mword.st == .pending and self.mRegCurrent()) self.mword.rtick = true;
        }

        // ---- invariants ----

        pub fn invariant(self: *const Self) !void {
            var r: Rows = .{};
            r.realloc_unfenced = self.realloc_unfenced;
            r.stale_data_write = self.stale_data_write;
            r.live_fenced = self.live_fenced;
            r.cleared_after_establish = self.cleared_after_establish;
            if (cfg.ja) {
                r.registered_but_pending = registeredButPending(&self.dword, self.dWaiting(), self.dRegCurrent()) or
                    registeredButPending(&self.mword, self.mword.st == .pending, self.mRegCurrent());
                r.pending_past_deadline = pendingPastDeadline(&self.dword, &self.waiter, true) or pendingPastDeadline(&self.mword, null, true);
            }
            r.hit_debt = self.hit_debt;
            return checkRows(r, cfg.strict);
        }

        pub fn final(self: *const Self) !void {
            // JA (current throughout) ends registered on both namespaces, its
            // data device unlatched, nothing parked — unless a deadline
            // fired for its words.
            if (cfg.ja and !self.j_failed and !self.dword.el_fired and !self.mword.el_fired) {
                if (!self.mreg[A] or !self.dreg[A]) return error.LiveFenced;
                if (self.dword.st == .pending or self.mword.st == .pending) return error.LiveFenced;
            }
        }

        pub fn stuck(_: *const Self) !void {
            return error.StrandedAtQuiescence;
        }
    };
}

// ---------------------------------------------------------------------------
// `Window` — the manager's side of the succession window: the expected set
// over rows ∪ Live pages, the settle, W_end (the reap with page keys, the
// evictions, the reconcile with its Live-page skip), the recovery of the
// dead; the members re-registering through the rule under currency.
// ---------------------------------------------------------------------------

pub const ReapScope = enum { rows_and_pages, rows_only };
pub const ExpectedFrom = enum { rows_and_pages, rows_only };
pub const MemberJudge = enum {
    /// A joined line judged by its page (Live under its writer id and term,
    /// else retired, else stale) — as built.
    line_and_page,
    /// W45: every line stale while the host is reserved and unregistered.
    registration_only,
};
pub const ClassifierOrder = enum { own_death_first, rtype_first };
pub const DataDeadline = enum { own, park_only };

pub const WindowConfig = struct {
    // ---- configurations (§7.2) ----
    /// J on host D, beside X.
    colocated_x: bool = false,
    /// J's row absent: J expected through its Live page only.
    row_missing: bool = false,
    /// X's row absent: X expected through its Live page only, reaped there.
    row_missing_frozen: bool = false,
    /// A long-held Recovering page of a dead sibling under J's node token
    /// and J's own retired predecessor page (covered by J's `r` line); J's
    /// refusal lands in the undecided interval.
    stale_recovering: bool = false,
    /// Two metadata volumes, J joined both.
    volumes: u8 = 1,
    /// (two volumes) S's open fails on volume 1 after its preempt-all on
    /// volume 0; S2 (host A) takes over; X off.
    vk_open_fails: bool = false,
    /// L, a target restart of the metadata namespaces.
    lapse: bool = false,
    /// `elapsed` for every word.
    all_elapsed: bool = false,
    /// The grace deadline fires only once J is back on every volume (or
    /// latched); `elapsed` only for X's word.
    fair: bool = false,
    /// Trims (§7.4's ledger).
    x_meta_write: bool = true,
    /// X's data write's admit and land as two steps (`.lapse`'s trim
    /// merges them: no preempt-all drain races a member's data write
    /// there).
    x_data_split: bool = true,
    j_read: bool = true,
    ticks: u8 = 2,
    // ---- weakenings ----
    currency: bool = true,
    reconcile_at_admission: bool = false,
    reap_key: bool = true,
    reconcile_in_window: bool = false,
    window_rtype: MetaRtype = .wear,
    lapse_preempt_all: bool = false,
    expected: ExpectedFrom = .rows_and_pages,
    reconcile_live_page_skip: bool = true,
    release_after_all_volumes: bool = true,
    reap_scope: ReapScope = .rows_and_pages,
    sibling_filter: bool = true,
    judge: MemberJudge = .line_and_page,
    reregister: bool = true,
    strict: Strict = .{},
};

/// The successor's steps: the preempt-all per volume, the bind, the data
/// admission, the owner armed, the window's settle (and its lapse
/// handling), W_end (reap, evict, reconcile), the recoveries.
const WsStep = enum(u8) { preempt, bind, admit, arm, settle, reap, evict, reconcile, recover, done };
/// A recovery's steps (§4.8 under WEAR, §DJ's data fence).
const WrStep = enum(u8) { fence, recovering, refence, dfence, scan, release, realloc, done };
/// J's steps (one thread here: its operation and its beat in sequence).
const WjStep = enum(u8) { read, read_barrier, write, land, barrier, idle, classify, await_reassert, rule, register, rejudge, post, retry, data, done };
/// X's metadata write: admit, land; refused ⇒ classified at once, wait,
/// retry.
const WxStep = enum(u8) { admit, land, wait, retry, done };

/// `Window` — S on host B (thread 0; `vk_open_fails`: S2 on host A, thread
/// 5), J on host C (D with `colocated_x`; thread 1), X frozen on host D
/// (its metadata write, thread 2; its data write, thread 3; X never
/// re-asserts), one data namespace:
///   S   per volume the preempt-all (start, per victim kill and unregister,
///       the drain's completion), the bind, the data admission, the owner
///       armed with the expected set (rows ∪ Live pages, keys from the
///       pages), the settle (census complete and every key registered; a
///       lapse's re-registration and re-acquire), at W_end the reap (page
///       keys), the evictions, the reconcile (the Live-page skip), then each
///       death's recovery (report-first fence, Recovering, C4, the data
///       fence, the tail scan, the release, the reallocation).
///   J   its read (INTERNAL during a drain, the barrier, the retry), its
///       metadata write (EBADE ⇒ classify ⇒ park; P9c EIO ⇒ the barrier),
///       re-assertion, the rule, the register on each unregistered volume,
///       the post-check (C2) and `cleared`, the retry, its data write.
///   X   frozen: its metadata write (refused ⇒ held, its word pending; its
///       `elapsed` on — the premise), its data write.
///   4 L, 6 GRACE (the grace deadline), 7 TICK, 8 ELAPSED (X's word), 9
///   ELAPSED (J's word, `all_elapsed`), 10 an unreadable tick for J's word,
///   11 an unreadable R-1 of J's.
pub fn Window(comptime cfg: WindowConfig) type {
    return extern struct {
        const Self = @This();
        pub const thread_count: usize = 12;
        const T_S = 0;
        const T_J = 1;
        const T_XM = 2;
        const T_XD = 3;
        const T_L = 4;
        const T_S2 = 5;
        const T_GRACE = 6;
        const T_TICK = 7;
        const T_ELX = 8;
        const T_ELJ = 9;
        const T_UTICK = 10;
        const T_UOP = 11;
        const NV: u8 = cfg.volumes;
        const HAS_X = !cfg.vk_open_fails;
        const JH: u8 = if (cfg.colocated_x) D else C;
        const SH = [2]u8{ B, A };
        /// Members: X (0), J (1); their hosts and their data blocks.
        const MX: u8 = 0;
        const MJ: u8 = 1;
        const MHOST = [2]u8{ D, JH };
        /// Incarnation ids on appender 0's page.
        const PRED: u8 = 0;

        comptime {
            if (NV != 1 and NV != 2) @compileError("one or two metadata volumes");
            if (cfg.vk_open_fails and NV != 2) @compileError("vk_open_fails needs two volumes");
        }

        // ---- the metadata namespaces ----
        rtype: [2]u8 = @splat(5),
        reg: [2][4]bool = initReg(),
        killed: [2][4]bool = @splat(@splat(false)),
        drain_by: [2]u8 = @splat(NONE),
        page0: [2]u8 = @splat(PRED),
        restarted: bool = false,
        // ---- the data namespace ----
        dreg: [4]bool = initDreg(),
        realloc: [2]bool = @splat(false),
        // ---- members' pages, rows, liveness ----
        page: [2]Page = .{ if (HAS_X) .live else .free, .live },
        row: [2]bool = .{ HAS_X and !cfg.row_missing_frozen, !cfg.row_missing },
        scanned: [2]bool = @splat(false),
        x_alive: bool = HAS_X,
        // ---- the successors (S, S2) ----
        s_pc: [2]u8 = .{ 0, if (cfg.vk_open_fails) 0 else @backingInt(WsStep.done) },
        s_alive: [2]bool = .{ true, cfg.vk_open_fails },
        s_v: [2]u8 = @splat(0),
        pa_started: [2]bool = @splat(false),
        pa_i: [2]u8 = @splat(0),
        pa_n: [2]u8 = @splat(0),
        pa_v: [2][3]u8 = @splat(@splat(NONE)),
        /// The owner armed (by successor index; NONE before).
        owner: u8 = NONE,
        expected: u8 = 0,
        reasserted: u8 = 0,
        dead: u8 = 0,
        /// The key each death carries (NONE: 0, no key).
        death_key: [2]u8 = @splat(NONE),
        grace: bool = false,
        /// The member under recovery and its step; the recoveries' order.
        rec_m: u8 = NONE,
        rec_pc: u8 = 0,
        rec_v: u8 = 0,
        recovered: u8 = 0,
        // ---- J ----
        j_pc: u8 = if (cfg.j_read) @backingInt(WjStep.read) else @backingInt(WjStep.write),
        j_failed: bool = false,
        j_held: bool = false,
        j_w: Wr = .idle,
        /// J's write admitted while a restart left the namespace unreserved
        /// (A3).
        j_w_unres: bool = false,
        /// A sibling's death fence unregistered J's host (the co-located
        /// per-host cost, Q4: J's latch is that fail-stop).
        j_death_fenced: bool = false,
        j_dw: Wr = .idle,
        /// The successions (bit per successor) and the restart that
        /// unregistered J's host; the retries its held write made.
        j_events: u8 = 0,
        j_retries: u8 = 0,
        mgr_writer: [2]u8 = @splat(PRED),
        j_fresh: [2]bool = @splat(false),
        /// J's host's last unregistration on each volume is explained.
        explained: [2]bool = @splat(false),
        /// J's host's data registration was removed by a reconcile or a
        /// death fence (its data refusal explained).
        d_explained: bool = false,
        /// The Recovering pages under J's node token J saw while registered
        /// (`recovering_seen[v]`): the stale sibling's, J's own retired.
        seen_stale: bool = cfg.stale_recovering,
        jword: [2]Word = @splat(.{}),
        // ---- X ----
        xm_pc: u8 = if (HAS_X and cfg.x_meta_write) 0 else @backingInt(WxStep.done),
        xm_w: Wr = .idle,
        xm_unres: bool = false,
        /// X's writes admitted inside J's rule's register window (A5).
        xm_window: bool = false,
        xd_window: bool = false,
        xd_w: Wr = if (HAS_X) .idle else .none,
        /// J's rule's register stands, its re-judge pending (A5).
        j_window: bool = false,
        xword: Word = .{},
        x_retries: u8 = 0,
        // ---- the environment ----
        ticks: u8 = 0,
        l_done: bool = !cfg.lapse,
        // ---- flags ----
        killed_current: bool = false,
        fenced_member_wrote: bool = false,
        realloc_unfenced: bool = false,
        stale_data_write: bool = false,
        reconcile_fenced_live: bool = false,
        live_fenced: bool = false,
        cleared_after_establish: bool = false,
        hit_lapse: bool = false,
        hit_register: bool = false,

        fn initReg() [2][4]bool {
            var r: [2][4]bool = @splat(@splat(false));
            for (0..NV) |v| {
                r[v][B] = true;
                r[v][JH] = true;
                if (HAS_X) r[v][D] = true;
            }
            return r;
        }

        fn initDreg() [4]bool {
            var r: [4]bool = @splat(false);
            r[B] = true;
            r[JH] = true;
            if (HAS_X) r[D] = true;
            if (cfg.vk_open_fails) r[A] = true;
            return r;
        }

        pub fn init() Self {
            return .{};
        }

        // ------------------------------------------------------------------
        // Thread status.
        // ------------------------------------------------------------------

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                T_S, T_S2 => blk: {
                    const k: usize = if (t == T_S) 0 else 1;
                    break :blk self.s_pc[k] == @backingInt(WsStep.done) or !self.s_alive[k];
                },
                T_J => self.jDone(),
                T_XM => self.xmDone(),
                T_XD => self.xd_w == .landed or self.xd_w == .refused or self.xd_w == .none,
                T_L => self.l_done,
                T_GRACE => !self.graceApplies(),
                T_TICK => !self.waitingAny(),
                T_ELX => !(HAS_X and !self.xword.el_fired and self.xword.st == .pending),
                T_ELJ => !(cfg.all_elapsed and self.elJApplies()),
                T_UTICK => !self.uTickApplies(),
                T_UOP => !self.uOpApplies(),
                else => unreachable,
            };
        }

        fn jDone(self: *const Self) bool {
            if (self.j_failed) return self.j_w != .admitted and self.j_dw != .admitted;
            const pc: WjStep = @fromBackingInt(@intCast(self.j_pc));
            return pc == .done or (pc == .idle and !self.jTriggered());
        }

        fn xmDone(self: *const Self) bool {
            const pc: WxStep = @fromBackingInt(@intCast(self.xm_pc));
            return pc == .done and self.xm_w != .admitted;
        }

        fn waitingAny(self: *const Self) bool {
            for (0..NV) |v| if (self.jword[v].st == .pending and !self.j_failed) return true;
            return self.xword.st == .pending;
        }

        fn elJApplies(self: *const Self) bool {
            for (0..NV) |v| if (!self.jword[v].el_fired and self.jword[v].st == .pending) return true;
            return false;
        }

        // ------------------------------------------------------------------
        // The device.
        // ------------------------------------------------------------------

        fn admits(self: *const Self, v: usize, h: u8) bool {
            return self.rtype[v] == 0 or self.reg[v][h];
        }

        fn current(self: *const Self, m: u8) bool {
            return self.page[m] == .live;
        }

        fn anyAdmittedMeta(self: *const Self, v: usize, h: u8) bool {
            if (v != self.jVol() and v != 0) return false;
            if (h == JH and self.j_w == .admitted and v == self.jVol()) return true;
            if (h == D and HAS_X and self.xm_w == .admitted and v == 0) return true;
            return false;
        }

        /// The volume J's metadata write goes to (the last joined).
        fn jVol(_: *const Self) usize {
            return NV - 1;
        }

        // ------------------------------------------------------------------
        // Steps.
        // ------------------------------------------------------------------

        pub fn step(self: *Self, t: usize) void {
            self.stepAs(t, false);
            self.post();
        }

        fn stepAs(self: *Self, t: usize, unreadable: bool) void {
            switch (t) {
                T_S => self.successor(0),
                T_S2 => self.successor(1),
                T_J => if (self.j_failed) {
                    if (self.j_w == .admitted) self.jLands();
                    if (self.j_dw == .admitted) self.j_dw = .landed;
                } else self.joiner(unreadable),
                T_XM => self.xMeta(),
                T_XD => self.xData(),
                T_L => self.restart(),
                T_GRACE => self.grace = true,
                T_TICK => self.tick(false),
                T_ELX => self.xword.elapse(),
                T_ELJ => {
                    for (0..NV) |v| if (!self.jword[v].el_fired and self.jword[v].st == .pending) {
                        self.jword[v].elapse();
                        break;
                    };
                },
                T_UTICK => self.tick(true),
                T_UOP => {
                    self.jword[self.jVol()].unread_used = true;
                    self.joiner(true);
                },
                else => unreachable,
            }
        }

        fn post(self: *Self) void {
            for (0..NV) |v| {
                if (!(self.jword[v].st == .pending and self.jRegCurrent(v))) self.jword[v].rtick = false;
            }
        }

        fn jRegCurrent(self: *const Self, v: usize) bool {
            return !self.j_failed and self.current(MJ) and self.rtype[v] == 5 and self.reg[v][JH];
        }

        // ---- the successors ----

        fn successor(self: *Self, k: usize) void {
            const h = SH[k];
            const id: u8 = @intCast(k + 1);
            const st: WsStep = @fromBackingInt(@intCast(self.s_pc[k]));
            switch (st) {
                .preempt => {
                    if (k == 1 and self.s_alive[0]) return; // S2 opens once S failed
                    const v = self.s_v[k];
                    if (k == 1 and !self.reg[v][h]) self.reg[v][h] = true;
                    if (!self.preempt(k, v)) return;
                    if (cfg.vk_open_fails and k == 0) {
                        // S's open fails on volume 1 after volume 0's
                        // preempt-all: its host stays registered there.
                        self.s_alive[0] = false;
                        return;
                    }
                    self.s_v[k] += 1;
                    if (self.s_v[k] < NV) return;
                },
                .bind => {
                    for (0..NV) |v| self.page0[v] = id;
                },
                .admit => {
                    // The data admission; W6: the reconcile here.
                    if (cfg.reconcile_at_admission and !self.reconcile(false)) return;
                },
                .arm => {
                    // The expected set: rows ∪ Live pages (W23: rows only).
                    var e: u8 = 0;
                    for (0..2) |m| {
                        const mm: u8 = @intCast(m);
                        if (mm == MX and !HAS_X) continue;
                        const by_page = cfg.expected == .rows_and_pages and self.page[mm] == .live;
                        if (self.row[mm] or by_page) e |= @as(u8, 1) << @intCast(mm);
                    }
                    self.expected = e;
                    self.owner = @intCast(k);
                },
                .settle => {
                    if (self.lapseHandling(k)) return;
                    if (cfg.reconcile_in_window and !self.reconcile(false)) return;
                    if (!self.settled() and !self.grace) return;
                },
                .reap => if (self.grace) {
                    // The deadline's reap: every expected entry neither
                    // re-asserted nor dead is shipped with its carried key —
                    // its page's (W9: none), row-only identities included
                    // unless W28 reaps rows alone.
                    for (0..2) |m| {
                        const mm: u8 = @intCast(m);
                        const bit = @as(u8, 1) << @intCast(mm);
                        if (self.expected & bit == 0 or self.reasserted & bit != 0 or self.dead & bit != 0) continue;
                        if (cfg.reap_scope == .rows_only and !self.row[mm]) continue;
                        self.die(mm, true);
                    }
                },
                .evict => {
                    // Every listed writer (a row, or a re-assertion's
                    // join) whose key is not registered on every managed
                    // namespace.
                    for (0..2) |m| {
                        const mm: u8 = @intCast(m);
                        const bit = @as(u8, 1) << @intCast(mm);
                        if (self.dead & bit != 0) continue;
                        if (!self.row[mm] and self.reasserted & bit == 0) continue;
                        if (mm == MX and !HAS_X) continue;
                        var all = true;
                        for (0..NV) |v| {
                            if (!self.reg[v][MHOST[mm]]) all = false;
                        }
                        if (!all) self.die(mm, false);
                    }
                },
                .reconcile => if (!self.reconcile(cfg.reconcile_live_page_skip)) return,
                .recover => {
                    if (!self.recoverStep()) return;
                },
                .done => unreachable,
            }
            self.s_pc[k] += 1;
        }

        /// A death record of member `m` with its carried key: an eviction's
        /// from its row, a reap's from its page (W9: the reap carries none
        /// and recovery has no page fallback).
        fn die(self: *Self, m: u8, reap: bool) void {
            self.dead |= @as(u8, 1) << @intCast(m);
            self.death_key[m] = if (reap and !cfg.reap_key) NONE else MHOST[m];
        }

        /// The census complete (every expected entry re-asserted or dead) and
        /// every expected live entry's key registered on every volume.
        fn settled(self: *const Self) bool {
            for (0..2) |m| {
                const mm: u8 = @intCast(m);
                const bit = @as(u8, 1) << @intCast(mm);
                if (self.expected & bit == 0 or self.dead & bit != 0) continue;
                if (self.reasserted & bit == 0) return false;
                for (0..NV) |v| if (!self.reg[v][MHOST[mm]]) return false;
            }
            return true;
        }

        /// S's lapse handling inside the window (§4.9): its host
        /// re-registered through the rule, the early end by the census (or
        /// the grace), the re-acquire (W17: a preempt-all). True: busy.
        fn lapseHandling(self: *Self, k: usize) bool {
            const h = SH[k];
            for (0..NV) |v| {
                if (self.rtype[v] != 0) continue;
                if (!self.reg[v][h]) {
                    self.reg[v][h] = true;
                    return true;
                }
                if (!self.lapseEarlyEnd() and !self.grace) return true;
                self.rtype[v] = 5;
                if (cfg.lapse_preempt_all) {
                    for (0..4) |x| {
                        if (x != h and self.reg[v][x]) self.reg[v][x] = false;
                    }
                }
                return true;
            }
            return false;
        }

        /// `successionCensus` while the grace stands: every expected entry
        /// re-asserted (or dead) and its key back on the namespace.
        fn lapseEarlyEnd(self: *const Self) bool {
            return self.settled();
        }

        /// The preempt-all on volume `v`, one action per step. True: done.
        fn preempt(self: *Self, k: usize, v: usize) bool {
            const h = SH[k];
            if (!self.pa_started[k]) {
                if (self.drain_by[v] != NONE) return false;
                // P8: the sender must be registered; P6: reserved. A
                // restart before the preempt-all refuses the open.
                if (!self.reg[v][h] or self.rtype[v] != 5) {
                    self.s_alive[k] = false;
                    return false;
                }
                var n: u8 = 0;
                for (0..4) |x| {
                    if (x != h and self.reg[v][x]) {
                        self.pa_v[k][n] = @intCast(x);
                        n += 1;
                    }
                }
                self.pa_n[k] = n;
                self.pa_i[k] = 0;
                self.pa_started[k] = true;
                self.drain_by[v] = @intCast(k);
                return false;
            }
            const i = self.pa_i[k];
            if (i < 2 * self.pa_n[k]) {
                const x = self.pa_v[k][i / 2];
                if (i % 2 == 0) {
                    self.killed[v][x] = true;
                } else {
                    self.reg[v][x] = false;
                    if (x == JH) {
                        self.explained[v] = true;
                        self.j_events |= @as(u8, 1) << @intCast(k);
                    }
                }
                self.pa_i[k] += 1;
                return false;
            }
            for (0..self.pa_n[k]) |n| if (self.anyAdmittedMeta(v, self.pa_v[k][n])) return false;
            self.killed[v] = @splat(false);
            self.drain_by[v] = NONE;
            // Canonical once done (nothing reads them until the next start).
            self.pa_started[k] = false;
            self.pa_i[k] = 0;
            self.pa_n[k] = 0;
            self.pa_v[k] = @splat(NONE);
            return true;
        }

        /// The reconcile: every data registrant whose key volume 0's metadata
        /// lacks — at W_end, unless a Live page on volume 0 names it (the
        /// skip). Fencing the key of a host that carries a current member
        /// beyond that host's own death fence is row 12.
        /// One step: blocked (false) while a victim has a data write the
        /// target admitted (each fence is a preempt-and-abort, P9).
        fn reconcile(self: *Self, skip: bool) bool {
            for (0..4) |x| {
                const xh: u8 = @intCast(x);
                if (self.reconcileVictim(xh, skip) and self.dataAdmitted(xh)) return false;
            }
            for (0..4) |x| {
                const xh: u8 = @intCast(x);
                if (!self.reconcileVictim(xh, skip)) continue;
                if (self.carriesLive(xh)) self.reconcile_fenced_live = true;
                self.dreg[xh] = false;
                if (xh == JH) self.d_explained = true;
            }
            return true;
        }

        fn reconcileVictim(self: *const Self, h: u8, skip: bool) bool {
            if (h == SH[0] or h == SH[1] or !self.dreg[h] or self.reg[0][h]) return false;
            return !(skip and self.livePageNames(h));
        }

        fn livePageNames(self: *const Self, h: u8) bool {
            for (0..2) |m| if (MHOST[m] == h and self.page[m] == .live) return true;
            return false;
        }

        /// Host `h` carries a current member that no death record of that
        /// host covers.
        fn carriesLive(self: *const Self, h: u8) bool {
            for (0..2) |m| {
                const mm: u8 = @intCast(m);
                if (MHOST[mm] != h or !self.current(mm)) continue;
                if (mm == MX and !HAS_X) continue;
                var own_death = false;
                for (0..2) |o| {
                    if (MHOST[o] == h and self.dead & (@as(u8, 1) << @intCast(o)) != 0) own_death = true;
                }
                if (!own_death) return true;
            }
            return false;
        }

        /// One step of the recoveries (X's first, then J's if dead). True:
        /// every death recovered.
        fn recoverStep(self: *Self) bool {
            if (self.rec_m == NONE) {
                for (0..2) |m| {
                    const mm: u8 = @intCast(m);
                    const bit = @as(u8, 1) << @intCast(mm);
                    if (self.dead & bit != 0 and self.recovered & bit == 0) {
                        self.rec_m = mm;
                        self.rec_pc = 0;
                        self.rec_v = 0;
                        return false;
                    }
                }
                return true;
            }
            const m = self.rec_m;
            const h = MHOST[m];
            const key = self.death_key[m];
            const st: WrStep = @fromBackingInt(@intCast(self.rec_pc));
            switch (st) {
                .fence, .refence => {
                    // Report first, then fence(k) on each volume (P5 + P9);
                    // W13: a WERO window's holder preempt fences nothing (the
                    // report shows the key: `.failed`, a full tail scan); a
                    // key-less death (W9) skips the fences.
                    const v = self.rec_v;
                    if (key != NONE and cfg.window_rtype == .wear and self.reg[v][h]) {
                        if (self.drain_by[v] != NONE or self.anyAdmittedMeta(v, h)) return false;
                        self.reg[v][h] = false;
                        if (h == JH) {
                            self.explained[v] = true;
                            if (m != MJ) self.j_death_fenced = true;
                        }
                    }
                    self.rec_v += 1;
                    if (self.rec_v < NV) return false;
                    self.rec_v = 0;
                },
                .recovering => self.page[m] = .recovering,
                .dfence => if (key != NONE and self.dreg[h]) {
                    if (self.dataAdmitted(h)) return false;
                    self.dreg[h] = false;
                    if (h == JH) self.d_explained = true;
                },
                .scan => {
                    self.scanned[m] = true;
                    self.page[m] = .recovered;
                },
                .release => self.page[m] = .free,
                .realloc => {
                    if (self.canLandData(m)) self.realloc_unfenced = true;
                    self.realloc[m] = true;
                },
                .done => unreachable,
            }
            self.rec_pc += 1;
            if (self.rec_pc == @backingInt(WrStep.done)) {
                self.recovered |= @as(u8, 1) << @intCast(m);
                self.rec_m = NONE;
                self.rec_pc = 0;
            }
            return false;
        }

        fn dataAdmitted(self: *const Self, h: u8) bool {
            if (h == D and HAS_X and self.xd_w == .admitted) return true;
            if (h == JH and self.j_dw == .admitted) return true;
            return false;
        }

        /// Member `m` can still land a data write on its block.
        fn canLandData(self: *const Self, m: u8) bool {
            const h = MHOST[m];
            if (m == MX) return switch (self.xd_w) {
                .admitted => true,
                .queued => self.dreg[h],
                .idle => self.x_alive and self.dreg[h],
                else => false,
            };
            return switch (self.j_dw) {
                .admitted => true,
                .queued, .idle => !self.j_failed and self.dreg[h],
                else => false,
            };
        }

        // ---- L ----

        fn restart(self: *Self) void {
            for (0..NV) |v| {
                if (self.drain_by[v] != NONE) return;
                if (self.anyAdmittedMeta(v, JH) or self.anyAdmittedMeta(v, D)) return;
            }
            for (0..NV) |v| {
                self.rtype[v] = 0;
                if (self.reg[v][JH]) {
                    self.explained[v] = true;
                    self.j_events |= 4;
                }
                self.reg[v] = @splat(false);
            }
            self.restarted = true;
            self.l_done = true;
        }

        // ---- X ----

        fn xMeta(self: *Self) void {
            const st: WxStep = @fromBackingInt(@intCast(self.xm_pc));
            if (st == .done or !self.x_alive) {
                if (self.xm_w == .admitted) self.xLands();
                self.xm_pc = @backingInt(WxStep.done);
                return;
            }
            switch (st) {
                .admit, .retry => {
                    if (self.admits(0, D) and !self.killed[0][D]) {
                        self.xm_w = .admitted;
                        self.xm_unres = self.rtype[0] == 0 and self.restarted;
                        self.xm_window = self.j_window;
                        self.xm_pc = @backingInt(WxStep.land);
                        return;
                    }
                    // Refused: X's hook classifies at once (one step): own
                    // death first (R-1), a lapse retries, else held.
                    if (!self.current(MX)) {
                        self.xm_pc = @backingInt(WxStep.done);
                        return;
                    }
                    if (self.rtype[0] == 0) return;
                    self.xword.pend();
                    self.xm_pc = @backingInt(WxStep.wait);
                    return;
                },
                .land => {
                    self.xLands();
                    self.xm_pc = @backingInt(WxStep.done);
                    return;
                },
                .wait => {
                    if (self.xword.st == .pending) return;
                    if (self.xword.st == .terminal or self.x_retries >= 1) {
                        self.xm_pc = @backingInt(WxStep.done);
                        return;
                    }
                    self.x_retries += 1;
                    self.xm_pc = @backingInt(WxStep.retry);
                    return;
                },
                .done => unreachable,
            }
            self.xm_pc += 1;
        }

        fn xLands(self: *Self) void {
            self.xm_w = .landed;
            const window = self.xm_window;
            const unres = self.xm_unres;
            self.xm_window = false;
            self.xm_unres = false;
            if (!self.scanned[MX]) return;
            if (window) {
                self.hit_register = true;
            } else if (unres) self.hit_lapse = true else self.fenced_member_wrote = true;
        }

        fn xData(self: *Self) void {
            switch (self.xd_w) {
                .idle => {
                    if (!self.x_alive) {
                        self.xd_w = .none;
                        return;
                    }
                    self.xd_w = .queued;
                },
                .queued => {
                    self.xd_w = if (self.dreg[D]) .admitted else .refused;
                    self.xd_window = self.j_window;
                    if (!cfg.x_data_split and self.xd_w == .admitted) self.xData();
                },
                .admitted => {
                    self.xd_w = .landed;
                    const window = self.xd_window;
                    self.xd_window = false;
                    if (self.realloc[MX]) {
                        if (window) self.hit_register = true else self.stale_data_write = true;
                    }
                },
                else => unreachable,
            }
        }

        // ---- J ----

        fn jTriggered(self: *const Self) bool {
            for (0..NV) |v| if (!self.reg[v][JH]) return true;
            return false;
        }

        fn jLands(self: *Self) void {
            self.j_w = .landed;
            const unres = self.j_w_unres;
            self.j_w_unres = false;
            if (!self.scanned[MJ]) return;
            if (unres) self.hit_lapse = true else self.fenced_member_wrote = true;
        }

        fn latchJ(self: *Self, explained_death: bool) void {
            self.j_failed = true;
            const deadline = blk: {
                for (0..NV) |v| if (self.jword[v].st == .terminal and self.jword[v].by_deadline) break :blk true;
                break :blk false;
            };
            // An evicted J (its death record stands) is doomed at its
            // recovery's step 2; its latch is the eviction's consequence.
            const evicted = self.dead & (@as(u8, 1) << @intCast(MJ)) != 0;
            if (self.current(MJ) and !evicted and !explained_death and !self.j_death_fenced and !deadline) self.live_fenced = true;
        }

        fn retryLeft(self: *const Self) bool {
            return self.j_retries < @popCount(self.j_events);
        }

        /// Some volume J's host is unregistered on, or NONE.
        fn jUnregVol(self: *const Self) u8 {
            for (0..NV) |v| if (!self.reg[v][JH]) return @intCast(v);
            return NONE;
        }

        fn anyKilledJ(self: *const Self) bool {
            for (0..NV) |v| if (self.killed[v][JH]) return true;
            return false;
        }

        fn joiner(self: *Self, unreadable: bool) void {
            for (0..NV) |v| if (self.jword[v].st == .terminal) return self.latchJ(false);
            const pc: WjStep = @fromBackingInt(@intCast(self.j_pc));
            switch (pc) {
                .read => self.j_pc = @backingInt(if (self.killed[0][JH]) WjStep.read_barrier else WjStep.write),
                .read_barrier => {
                    if (self.drain_by[0] != NONE) return;
                    self.j_pc = @backingInt(WjStep.write);
                },
                .write => {
                    const v = self.jVol();
                    if (self.admits(v, JH) and !self.killed[v][JH]) {
                        self.j_w = .admitted;
                        self.j_w_unres = self.rtype[v] == 0 and self.restarted;
                        self.j_pc = @backingInt(WjStep.land);
                        return;
                    }
                    if (self.killed[v][JH] and self.admits(v, JH)) {
                        self.j_pc = @backingInt(WjStep.barrier);
                        return;
                    }
                    if (!self.explained[v] and self.current(MJ)) self.live_fenced = true;
                    self.j_held = true;
                    self.j_pc = @backingInt(WjStep.classify);
                },
                .land => {
                    self.jLands();
                    self.j_held = false;
                    self.j_pc = @backingInt(WjStep.data);
                },
                .barrier => {
                    if (self.drain_by[self.jVol()] != NONE) return;
                    if (self.reg[self.jVol()][JH]) {
                        self.j_pc = @backingInt(WjStep.write);
                        return;
                    }
                    self.j_held = true;
                    self.j_pc = @backingInt(WjStep.classify);
                },
                .idle => if (self.jTriggered()) {
                    self.j_pc = @backingInt(WjStep.classify);
                },
                .classify => {
                    if (self.anyKilledJ()) return;
                    self.jClassify(unreadable);
                },
                .await_reassert => {
                    if (self.anyKilledJ()) return;
                    if (self.jUnregVol() == NONE) return self.jObserved(unreadable);
                    const v = self.jUnregVol();
                    if (self.rtype[v] == 0) {
                        self.j_pc = @backingInt(WjStep.rule);
                        return;
                    }
                    // Re-classified at each park beat: a co-located sibling's
                    // death visible now (R-3.2) latches.
                    if (cfg.colocated_x and self.page[MX] == .recovering) return self.latchJ(true);
                    // C1: the owner armed at T+1 grants the reclaim, its page
                    // named on the volume.
                    if (self.owner == NONE or self.page0[v] != self.owner + 1) return;
                    self.reasserted |= @as(u8, 1) << @intCast(MJ);
                    self.j_pc = @backingInt(WjStep.rule);
                },
                .rule => {
                    if (self.anyKilledJ()) return;
                    if (!cfg.reregister and self.rtype[self.jUnregVol()] == 5) return;
                    // C2 before (W4: none); the rule judges J's host's lines
                    // (X's with `colocated_x`).
                    if (cfg.currency and !self.current(MJ)) return self.latchJ(false);
                    if (cfg.colocated_x and self.x_alive and self.xStale()) {
                        if (self.current(MX)) self.killed_current = true;
                        self.x_alive = false;
                    }
                    self.j_pc = @backingInt(WjStep.register);
                },
                .register => {
                    const v = self.jUnregVol();
                    if (v == NONE) {
                        self.j_pc = @backingInt(WjStep.rejudge);
                        return;
                    }
                    if (self.drain_by[v] != NONE) return;
                    if (cfg.colocated_x and !self.x_alive and (self.xm_w == .admitted or self.xd_w == .queued or self.xd_w == .admitted)) return;
                    self.reg[v][JH] = true;
                    self.j_fresh[v] = true;
                    self.explained[v] = false;
                    self.jword[v].arm_open = true;
                    if (cfg.colocated_x) self.j_window = true;
                    // One volume per step; W26: release after the first.
                    if (self.jUnregVol() != NONE and cfg.release_after_all_volumes) return;
                    self.j_pc = @backingInt(WjStep.rejudge);
                },
                .rejudge => {
                    // The rule's re-judge (DC7): a sibling stale now is
                    // killed, the fresh registrations withdrawn, the
                    // register retried after the drain.
                    self.j_window = false;
                    if (cfg.colocated_x and self.x_alive and self.xStale()) {
                        if (self.current(MX)) self.killed_current = true;
                        self.x_alive = false;
                        for (0..NV) |v| if (self.j_fresh[v]) {
                            self.reg[v][JH] = false;
                            self.j_fresh[v] = false;
                            self.explained[v] = true;
                        };
                        self.j_pc = @backingInt(WjStep.register);
                        return;
                    }
                    self.j_pc = @backingInt(WjStep.post);
                },
                .post => {
                    if (self.anyKilledJ()) return;
                    for (0..NV) |v| self.jword[v].arm_open = false;
                    // C2 after (W4: none): an evicted member withdraws its
                    // fresh registrations and latches.
                    if (cfg.currency and !self.current(MJ)) {
                        for (0..NV) |v| if (self.j_fresh[v]) {
                            self.reg[v][JH] = false;
                            self.explained[v] = true;
                        };
                        return self.latchJ(false);
                    }
                    for (0..NV) |v| {
                        if (!self.reg[v][JH]) continue;
                        if (self.jword[v].st == .pending and !self.jword[v].clear()) return self.latchJ(false);
                        if (self.page0[v] != NONE) self.mgr_writer[v] = self.page0[v];
                        self.j_fresh[v] = false;
                        if (self.jword[v].st == .pending) self.cleared_after_establish = true;
                    }
                    self.seen_stale = self.seen_stale or cfg.stale_recovering;
                    if (self.jUnregVol() != NONE and !self.j_held) {
                        self.j_pc = @backingInt(WjStep.classify);
                        return;
                    }
                    self.j_pc = @backingInt(if (self.j_held) WjStep.retry else WjStep.idle);
                    return;
                },
                .retry => {
                    if (!self.retryLeft()) return self.latchJ(false);
                    self.j_retries += 1;
                    self.j_pc = @backingInt(WjStep.write);
                    return;
                },
                .data => {
                    if (self.j_dw == .idle) {
                        if (self.dreg[JH]) {
                            self.j_dw = .admitted;
                            return;
                        }
                        self.j_dw = .refused;
                        // A data refusal: explained by a death fence of its
                        // host (co-located, Q4) or else a fence.
                        return self.latchJ(self.d_explained);
                    }
                    if (self.j_dw == .admitted) {
                        self.j_dw = .landed;
                        if (self.realloc[MJ]) self.stale_data_write = true;
                    }
                    self.j_pc = @backingInt(WjStep.idle);
                    return;
                },
                .done => unreachable,
            }
        }

        /// X's j line judged stale by J's rule: by its page (Live: not
        /// stale) — W45: by the registration alone (reserved, host
        /// unregistered).
        fn xStale(self: *const Self) bool {
            return switch (cfg.judge) {
                .line_and_page => !self.current(MX),
                .registration_only => self.rtype[0] == 5 and !self.reg[0][D],
            };
        }

        /// J's classifier (§4.5.3): R-1 own death first; R-2 the rtype; R-3
        /// succession, a co-located sibling's death (with R-3.2's filters),
        /// undecided.
        fn jClassify(self: *Self, unreadable: bool) void {
            if (!self.current(MJ)) return self.latchJ(false);
            const v = self.jUnregVol();
            if (v == NONE) return self.jObserved(unreadable);
            if (self.rtype[v] == 0) {
                self.j_pc = @backingInt(WjStep.rule);
                return;
            }
            // R-3.2: a Recovering page under J's node token, new since J's
            // last confirmed registration and not covered by its own `r`
            // line, means a co-located sibling's death fenced the host.
            if (cfg.colocated_x and self.page[MX] == .recovering) return self.latchJ(true);
            if (cfg.stale_recovering and !cfg.sibling_filter) return self.latchJ(false);
            self.jword[v].pend();
            self.j_pc = @backingInt(WjStep.await_reassert);
        }

        /// Host J registered everywhere again (someone else's registration):
        /// observing — R-1, then `cleared`.
        fn jObserved(self: *Self, unreadable: bool) void {
            if (unreadable) return;
            for (0..NV) |v| if (self.jword[v].st == .pending) {
                _ = self.jword[v].clear();
            };
            self.j_pc = @backingInt(if (self.j_held) WjStep.retry else WjStep.idle);
        }

        // ---- rule L's environment ----

        /// The grace deadline (T_owner after the owner armed); `fair`: only
        /// once J is back on every volume, or latched.
        fn graceApplies(self: *const Self) bool {
            if (self.grace or self.owner == NONE) return false;
            if (self.s_pc[self.owner] > @backingInt(WsStep.settle)) return false;
            if (!cfg.fair) return true;
            return self.j_failed or self.jUnregVol() == NONE;
        }

        fn othersQuiescent(self: *const Self) bool {
            for (0..thread_count) |t| {
                if (t == T_TICK or t == T_UTICK or self.done(t)) continue;
                var next = self.*;
                next.step(t);
                if (!std.mem.eql(u8, std.mem.asBytes(&next), std.mem.asBytes(self))) return false;
            }
            return true;
        }

        fn tickRuns(self: *const Self) bool {
            if (!self.waitingAny()) return false;
            return self.ticks < cfg.ticks or self.othersQuiescent();
        }

        fn uTickApplies(self: *const Self) bool {
            const v = self.jVol();
            const w = &self.jword[v];
            if (self.j_failed or w.unread_used or w.st != .pending or w.pastDeadline() or !self.reg[v][JH]) return false;
            return self.tickRuns();
        }

        fn uOpApplies(self: *const Self) bool {
            const v = self.jVol();
            if (self.j_failed or self.jword[v].unread_used or self.anyKilledJ()) return false;
            const pc: WjStep = @fromBackingInt(@intCast(self.j_pc));
            return (pc == .classify or pc == .await_reassert) and self.jUnregVol() == NONE;
        }

        /// `tick`: every word's re-examiner in the same step — the deadline's
        /// `terminal`; else, its host registered (or the namespace
        /// unreserved): R-1 (current ⇒ `cleared`; not Live ⇒ `terminal`).
        fn tick(self: *Self, unreadable: bool) void {
            if (!self.tickRuns()) return;
            if (self.ticks < cfg.ticks) self.ticks += 1;
            for (0..NV) |v| {
                const w = &self.jword[v];
                const un = unreadable and v == self.jVol();
                if (un) w.unread_used = true;
                if (w.pastDeadline()) {
                    w.terminate(true);
                } else if (w.st == .pending and (self.rtype[v] == 0 or self.reg[v][JH]) and !self.killed[v][JH] and !un) {
                    if (self.current(MJ)) _ = w.clear() else w.terminate(false);
                }
                if (w.el_fired) w.el_ticked = true;
                if (!un and w.anchors() and w.st == .pending and self.jRegCurrent(v)) w.rtick = true;
            }
            const x = &self.xword;
            if (x.pastDeadline()) {
                x.terminate(true);
            } else if (x.st == .pending and (self.rtype[0] == 0 or self.reg[0][D]) and !self.killed[0][D]) {
                if (self.current(MX)) _ = x.clear() else x.terminate(false);
            }
            if (x.el_fired) x.el_ticked = true;
        }

        // ---- invariants ----

        pub fn invariant(self: *const Self) !void {
            var r: Rows = .{};
            r.killed_current = self.killed_current;
            r.fenced_member_wrote = self.fenced_member_wrote;
            r.realloc_unfenced = self.realloc_unfenced;
            r.stale_data_write = self.stale_data_write;
            r.reconcile_fenced_live = self.reconcile_fenced_live;
            r.live_fenced = self.live_fenced;
            r.cleared_after_establish = self.cleared_after_establish;
            for (0..NV) |v| {
                if (registeredButPending(&self.jword[v], self.jword[v].st == .pending, self.jRegCurrent(v))) r.registered_but_pending = true;
                if (pendingPastDeadline(&self.jword[v], null, true)) r.pending_past_deadline = true;
            }
            if (pendingPastDeadline(&self.xword, null, true)) r.pending_past_deadline = true;
            r.hit_lapse = self.hit_lapse;
            r.hit_register = self.hit_register;
            return checkRows(r, cfg.strict);
        }

        pub fn final(self: *const Self) !void {
            // LiveFenced's final part: J current, not latched, unregistered
            // somewhere or parked, with no `elapsed` for its words.
            if (!self.j_failed and self.current(MJ) and self.dead & (@as(u8, 1) << @intCast(MJ)) == 0) {
                var el = false;
                for (0..NV) |v| {
                    if (self.jword[v].el_fired) el = true;
                }
                if (!el) {
                    for (0..NV) |v| {
                        if (self.rtype[v] == 5 and !self.reg[v][JH]) return error.LiveFenced;
                        if (self.jword[v].st == .pending) return error.LiveFenced;
                    }
                }
            }
            // RecoveryStuck: every death recovered (its page Recovered, its
            // region released) — and an expected, never-returning X reaped.
            if (self.dead & ~self.recovered != 0) return error.RecoveryStuck;
            if (HAS_X and self.current(MX) and self.reasserted & 1 == 0 and self.owner != NONE) return error.RecoveryStuck;
        }

        pub fn stuck(_: *const Self) !void {
            return error.StrandedAtQuiescence;
        }
    };
}

// ---------------------------------------------------------------------------
// The `Window` configurations whose threads differ from `Window`'s:
// `.late_clear` (W36), `.evicted_after_sibling` (W43), `.lapse_evicted`
// (W24), `.data_stall` (W41) — each its own type, one metadata volume.
// ---------------------------------------------------------------------------

pub const WithdrawTimeout = enum {
    /// At DC22's bound the fresh registration stays and J latches; a later
    /// re-fence drains the write (as built, revision 17).
    keep_registration,
    /// W53: at the bound, unregister anyway.
    unregister,
};

pub const LateClearConfig = struct {
    /// W36: the late `cleared` overwrites `terminal`.
    terminal_absorbing: bool = true,
    /// DC22: a withdrawal waits for J's own admitted write before its
    /// unregister (W52: false, a plain unregister).
    withdraw_drains: bool = true,
    withdraw_timeout: WithdrawTimeout = .keep_registration,
    ticks: u8 = 2,
    strict: Strict = .{},
};

/// `Window{.late_clear}` — S (host B), J split (J.op, J.beat; host C), X
/// off: J's park can expire (`elapsed` on for J's word, the premise) while
/// J.beat's re-registration is in flight; W_end can evict J (the grace
/// deadline), and S's recovery runs on J's region (step 1, Recovering,
/// C4's re-fence, the data fence, the tail scan, the release).
///   0 S       the preempt-all (start, kill, unregister, the drain), the
///             bind, the owner armed, the settle or the deadline, the
///             eviction, J's recovery.
///   1 J.op    J's metadata write: refused ⇒ the gen captured, held, the
///             word pending, asleep; `cleared` ⇒ the retry; `terminal` ⇒
///             its failure path.
///   2 J.beat  re-assertion, the register, the post-check (C2), `cleared`
///             (a CAS refusing to leave `terminal`; refused ⇒ the fresh
///             registration withdrawn); a withdrawal waits until J.op's
///             admitted write completed (DC22).
///   3 GRACE, 4 TICK, 5 ELAPSED (J's word), 6 WITHDRAW_TIMEOUT: DC22's
///   bound, one-shot, enabled only while the withdrawal waits — the
///   registration stays and J latches.
pub fn WindowLateClear(comptime cfg: LateClearConfig) type {
    return extern struct {
        const Self = @This();
        pub const thread_count: usize = 7;
        const S_PA_START = 0;
        const S_KILL = 1;
        const S_UNREG = 2;
        const S_DRAIN = 3;
        const S_BIND = 4;
        const S_ARM = 5;
        const S_SETTLE = 6;
        const S_EVICT = 7;
        const S_FENCE = 8;
        const S_RECOVERING = 9;
        const S_REFENCE = 10;
        const S_DFENCE = 11;
        const S_SCAN = 12;
        const S_RELEASE = 13;
        const S_END = 14;
        const OP_WRITE = 0;
        const OP_LAND = 1;
        const OP_HELD = 2;
        const OP_WAIT = 3;
        const OP_END = 4;
        const B_AWAIT = 0;
        const B_REGISTER = 1;
        const B_POST = 2;
        const B_END = 3;

        reg_c: bool = true,
        killed_c: bool = false,
        dreg_c: bool = true,
        page: Page = .live,
        scanned: bool = false,
        armed: bool = false,
        grace: bool = false,
        evicted: bool = false,
        s_pc: u8 = 0,
        op_pc: u8 = 0,
        b_pc: u8 = 0,
        jw: Wr = .idle,
        j_failed: bool = false,
        j_fresh: bool = false,
        reasserted: bool = false,
        word: Word = .{},
        waiter: Waiter = .{},
        ticks: u8 = 0,
        withdraw_timed_out: bool = false,
        fenced_member_wrote: bool = false,
        live_fenced: bool = false,
        cleared_after_establish: bool = false,

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                0 => self.s_pc >= S_END,
                1 => self.op_pc >= OP_END and self.jw != .admitted,
                2 => self.b_pc >= B_END or self.j_failed,
                3 => self.grace or !self.armed or self.s_pc > S_SETTLE,
                4 => !self.waiting(),
                5 => self.word.el_fired or self.word.st != .pending,
                6 => self.withdraw_timed_out or !self.withdrawWaits(),
                else => unreachable,
            };
        }

        /// J.beat's post-check withdrawal waits on J.op's admitted write
        /// (DC22).
        fn withdrawWaits(self: *const Self) bool {
            return cfg.withdraw_drains and !self.j_failed and self.b_pc == B_POST and self.page != .live and self.j_fresh and self.jw == .admitted;
        }

        /// DC22's bound: no unregister (W53: unregister anyway); J, not
        /// current, latches.
        fn withdrawTimeout(self: *Self) void {
            self.withdraw_timed_out = true;
            self.word.arm_open = false;
            if (cfg.withdraw_timeout == .unregister) self.reg_c = false;
            self.b_pc = B_END;
            self.latch();
        }

        fn waiting(self: *const Self) bool {
            return self.word.st == .pending or (self.waiter.held and !self.j_failed);
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                0 => self.manager(),
                1 => self.op(),
                2 => self.beat(),
                3 => self.grace = true,
                4 => self.tick(),
                5 => self.word.elapse(),
                6 => self.withdrawTimeout(),
                else => unreachable,
            }
            if (!(self.word.st == .pending and self.regCurrent())) self.word.rtick = false;
        }

        fn regCurrent(self: *const Self) bool {
            return !self.j_failed and self.page == .live and self.reg_c;
        }

        fn manager(self: *Self) void {
            switch (self.s_pc) {
                S_PA_START => {},
                S_KILL => self.killed_c = true,
                S_UNREG => self.reg_c = false,
                S_DRAIN => {
                    if (self.jw == .admitted) return;
                    self.killed_c = false;
                },
                S_BIND => {},
                S_ARM => self.armed = true,
                S_SETTLE => if (!(self.reasserted and self.reg_c) and !self.grace) return,
                S_EVICT => {
                    if (self.reg_c and self.reasserted) {
                        // Settled: nothing to evict.
                        self.s_pc = S_END;
                        return;
                    }
                    self.evicted = true;
                },
                S_FENCE, S_REFENCE => if (self.reg_c) {
                    if (self.jw == .admitted) return;
                    self.reg_c = false;
                },
                S_RECOVERING => self.page = .recovering,
                S_DFENCE => if (self.dreg_c) {
                    self.dreg_c = false;
                },
                S_SCAN => {
                    self.scanned = true;
                    self.page = .recovered;
                },
                S_RELEASE => self.page = .free,
                else => unreachable,
            }
            self.s_pc += 1;
        }

        fn latch(self: *Self) void {
            self.j_failed = true;
            const exempt = self.page != .live or self.evicted or (self.word.st == .terminal and self.word.by_deadline);
            if (!exempt) self.live_fenced = true;
            self.waiter.reset();
        }

        fn op(self: *Self) void {
            if (self.j_failed) {
                if (self.jw == .admitted) self.lands();
                self.op_pc = OP_END;
                return;
            }
            switch (self.op_pc) {
                OP_WRITE => {
                    if (self.reg_c and !self.killed_c) {
                        self.jw = .admitted;
                        self.op_pc = OP_LAND;
                        return;
                    }
                    // Refused (or P9c's EIO, the barrier answering
                    // unregistered after the drain): the gen captured.
                    if (self.killed_c) return;
                    self.waiter.capture(&self.word);
                    self.op_pc = OP_HELD;
                    return;
                },
                OP_LAND => {
                    self.lands();
                    self.waiter.reset();
                    self.op_pc = OP_END;
                    return;
                },
                OP_HELD => {
                    // The classifier's R-1 first: J's own page not Live —
                    // its eviction's failure path.
                    if (self.page != .live) return self.latch();
                    self.waiter.held = true;
                    self.word.pend();
                },
                OP_WAIT => {
                    if (!self.waiter.wakes(&self.word, true)) {
                        self.waiter.sleeping = true;
                        return;
                    }
                    if (self.word.st == .terminal) return self.latch();
                    self.waiter.reset();
                    self.op_pc = OP_WRITE;
                    return;
                },
                else => unreachable,
            }
            self.op_pc += 1;
        }

        fn lands(self: *Self) void {
            self.jw = .landed;
            if (self.scanned) self.fenced_member_wrote = true;
        }

        fn beat(self: *Self) void {
            if (self.word.st == .terminal and cfg.terminal_absorbing) return self.latch();
            switch (self.b_pc) {
                B_AWAIT => {
                    if (self.reg_c) return;
                    if (!self.armed) return;
                    self.reasserted = true;
                },
                B_REGISTER => {
                    if (self.killed_c) return;
                    // C2 before the register: an evicted J does not register.
                    if (self.page != .live) return self.latch();
                    self.j_fresh = !self.reg_c;
                    self.reg_c = true;
                    self.word.arm_open = true;
                },
                B_POST => {
                    // C2: an evicted J (page not Live) withdraws — once its
                    // own admitted write completed (DC22).
                    if (self.withdrawWaits()) return;
                    self.word.arm_open = false;
                    if (self.page != .live) {
                        if (self.j_fresh) self.reg_c = false;
                        return self.latch();
                    }
                    // `cleared` once J sees itself registered (M5): a
                    // registration step 1 already removed leaves the word.
                    if (!self.reg_c) {
                        self.b_pc = B_END;
                        return;
                    }
                    if (cfg.terminal_absorbing) {
                        if (!self.word.clear()) {
                            if (self.j_fresh) self.reg_c = false;
                            return self.latch();
                        }
                    } else {
                        // W36: the late `cleared` overwrites `terminal`.
                        self.word.st = .cleared;
                        self.word.gen +%= 1;
                    }
                    if (self.word.st == .pending) self.cleared_after_establish = true;
                },
                else => unreachable,
            }
            self.b_pc += 1;
        }

        fn othersQuiescent(self: *const Self) bool {
            for (0..thread_count) |t| {
                if (t == 4 or self.done(t)) continue;
                var next = self.*;
                next.step(t);
                if (!std.mem.eql(u8, std.mem.asBytes(&next), std.mem.asBytes(self))) return false;
            }
            return true;
        }

        fn tick(self: *Self) void {
            if (!(self.ticks < cfg.ticks or self.othersQuiescent())) return;
            if (self.ticks < cfg.ticks) self.ticks += 1;
            if (self.word.pastDeadline()) {
                self.word.terminate(true);
            } else if (self.word.st == .pending and !self.killed_c) {
                // R-1's positive not-Live proof ends the word whatever the
                // registration; Live and registered ⇒ `cleared`.
                if (self.page != .live) {
                    self.word.terminate(false);
                } else if (self.reg_c) _ = self.word.clear();
            }
            if (self.word.el_fired) self.word.el_ticked = true;
            if (self.word.anchors() and self.word.st == .pending and self.regCurrent()) self.word.rtick = true;
        }

        pub fn invariant(self: *const Self) !void {
            var r: Rows = .{};
            r.fenced_member_wrote = self.fenced_member_wrote;
            r.live_fenced = self.live_fenced;
            r.cleared_after_establish = self.cleared_after_establish;
            r.registered_but_pending = registeredButPending(&self.word, self.word.st == .pending or self.waiter.blocked(&self.word, true), self.regCurrent());
            r.pending_past_deadline = pendingPastDeadline(&self.word, &self.waiter, true);
            return checkRows(r, cfg.strict);
        }

        pub fn final(self: *const Self) !void {
            if (!self.j_failed and self.page == .live and !self.evicted and !self.word.el_fired and !self.reg_c) return error.LiveFenced;
            if (self.evicted and self.page != .free) return error.RecoveryStuck;
        }

        pub fn stuck(_: *const Self) !void {
            return error.StrandedAtQuiescence;
        }
    };
}

pub const EvictedAfterSiblingConfig = struct {
    /// W43: an observing `cleared` without R-1.
    observed_clear_currency: bool = true,
    /// J_D's re-judge (false: the step stays as A5's closing marker).
    rejudge: bool = true,
    ticks: u8 = 2,
    strict: Strict = .{},
};

/// `Window{.evicted_after_sibling}` — S (host B), J_D a member on host D
/// beside X, X frozen with one held metadata write (its only write; its
/// data write off):
///   0 S     the preempt-all of host D (kill, unregister, the drain), the
///           bind, the owner armed, the deadline, X evicted, X's recovery
///           (step 1's fence of K_D, Recovering, C4's re-fence, the tail
///           scan, the release).
///   1 J_D   re-assertion, the rule's read (judging X by its page), the
///           register (a separate step: it can land after C4), the re-judge
///           (W43's pairing: a marker only), the post-check.
///   2 X     its metadata write: refused ⇒ held, its word pending; woken by
///           `cleared` ⇒ one retry; `terminal` ⇒ its failure path.
///   3 GRACE, 4 TICK (X's re-examiner: host D registered ⇒ R-1 — Live ⇒
///   `cleared`, not Live ⇒ `terminal`; W43: `cleared` without R-1), 5
///   ELAPSED (X's word).
pub fn WindowEvictedAfterSibling(comptime cfg: EvictedAfterSiblingConfig) type {
    return extern struct {
        const Self = @This();
        pub const thread_count: usize = 6;
        const S_KILL = 0;
        const S_UNREG = 1;
        const S_DRAIN = 2;
        const S_BIND = 3;
        const S_ARM = 4;
        const S_DEADLINE = 5;
        const S_FENCE = 6;
        const S_RECOVERING = 7;
        const S_REFENCE = 8;
        const S_SCAN = 9;
        const S_RELEASE = 10;
        const S_END = 11;
        const J_AWAIT = 0;
        const J_RULE = 1;
        const J_REGISTER = 2;
        const J_REJUDGE = 3;
        const J_POST = 4;
        const J_END = 5;
        const X_WRITE = 0;
        const X_LAND = 1;
        const X_WAIT = 2;
        const X_END = 3;
        /// X frozen with its write in flight: the preempt-all refuses it
        /// (its only write is the held one).
        const X_FROZEN = 4;

        reg_d: bool = true,
        killed_d: bool = false,
        x_page: Page = .live,
        x_alive: bool = true,
        scanned: bool = false,
        armed: bool = false,
        grace: bool = false,
        s_pc: u8 = 0,
        j_pc: u8 = 0,
        x_pc: u8 = X_FROZEN,
        xw: Wr = .idle,
        x_retried: bool = false,
        /// J_D's rule read X's page Live (before step 2); its register then
        /// stands (A4 until C4's re-fence).
        j_read_live: bool = false,
        j_killed_x: bool = false,
        j_window: bool = false,
        refenced: bool = false,
        xw_window: bool = false,
        xw_readmit: bool = false,
        j_failed: bool = false,
        xword: Word = .{},
        ticks: u8 = 0,
        killed_current: bool = false,
        fenced_member_wrote: bool = false,
        hit_register: bool = false,
        hit_readmit: bool = false,

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                0 => self.s_pc >= S_END,
                1 => self.j_pc >= J_END or self.j_failed,
                2 => self.x_pc >= X_END and self.xw != .admitted,
                3 => self.grace or !self.armed,
                4 => self.xword.st != .pending,
                5 => self.xword.el_fired or self.xword.st != .pending,
                else => unreachable,
            };
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                0 => self.manager(),
                1 => self.joiner(),
                2 => self.x(),
                3 => self.grace = true,
                4 => self.tick(),
                5 => self.xword.elapse(),
                else => unreachable,
            }
        }

        fn manager(self: *Self) void {
            switch (self.s_pc) {
                S_KILL => self.killed_d = true,
                S_UNREG => {
                    self.reg_d = false;
                    // X's in-flight write refused: held, its word pending.
                    self.xword.pend();
                    self.x_pc = X_WAIT;
                },
                S_DRAIN => {
                    if (self.xw == .admitted) return;
                    self.killed_d = false;
                },
                S_BIND => {},
                S_ARM => self.armed = true,
                // X never re-asserts: the deadline evicts it.
                S_DEADLINE => if (!self.grace) return,
                S_FENCE, S_REFENCE => {
                    if (self.reg_d) {
                        if (self.xw == .admitted) return;
                        self.reg_d = false;
                    }
                    if (self.s_pc == S_REFENCE) self.refenced = true;
                },
                S_RECOVERING => self.x_page = .recovering,
                S_SCAN => {
                    self.scanned = true;
                    self.x_page = .recovered;
                },
                S_RELEASE => self.x_page = .free,
                else => unreachable,
            }
            self.s_pc += 1;
        }

        fn joiner(self: *Self) void {
            switch (self.j_pc) {
                J_AWAIT => {
                    if (self.reg_d or !self.armed) return;
                },
                J_RULE => {
                    // X's j line judged by its page: Live ⇒ not stale.
                    self.j_read_live = self.x_page == .live;
                    if (!self.j_read_live and self.x_alive) {
                        self.x_alive = false;
                        self.j_killed_x = true;
                    }
                },
                J_REGISTER => {
                    if (self.killed_d) return;
                    if (self.j_killed_x and self.xw == .admitted) return;
                    self.reg_d = true;
                    self.j_window = true;
                },
                J_REJUDGE => {
                    // The re-judge closes A5 (a marker under rejudge =
                    // false); a stale X is killed and the registration
                    // withdrawn, the register retried after the drain.
                    self.j_window = false;
                    self.j_read_live = false;
                    if (cfg.rejudge and self.x_alive and self.x_page != .live) {
                        self.x_alive = false;
                        self.j_killed_x = true;
                        self.reg_d = false;
                        self.j_pc = J_REGISTER;
                        return;
                    }
                },
                J_POST => {},
                else => unreachable,
            }
            self.j_pc += 1;
        }

        fn x(self: *Self) void {
            if (self.x_pc == X_FROZEN) return;
            if (!self.x_alive) {
                if (self.xw == .admitted) self.xLands();
                self.x_pc = X_END;
                return;
            }
            switch (self.x_pc) {
                X_WRITE => {
                    if (self.reg_d and !self.killed_d) {
                        self.xw = .admitted;
                        self.xw_window = self.j_window;
                        self.xw_readmit = self.j_read_live and !self.refenced;
                        self.x_pc = X_LAND;
                        return;
                    }
                    if (self.killed_d) return;
                    if (self.x_page != .live or self.x_retried) {
                        self.x_pc = X_END;
                        return;
                    }
                    self.xword.pend();
                    self.x_pc = X_WAIT;
                    return;
                },
                X_LAND => {
                    self.xLands();
                    self.x_pc = X_END;
                    return;
                },
                X_WAIT => {
                    if (self.xword.st == .pending) return;
                    if (self.xword.st == .terminal) {
                        self.x_pc = X_END;
                        return;
                    }
                    self.x_retried = true;
                    self.x_pc = X_WRITE;
                    return;
                },
                else => unreachable,
            }
        }

        fn xLands(self: *Self) void {
            self.xw = .landed;
            if (!self.scanned) {
                if (self.xw_readmit and !self.refenced) self.hit_readmit = true;
                return;
            }
            if (self.xw_window) {
                self.hit_register = true;
            } else if (self.xw_readmit and !self.refenced) {
                self.hit_readmit = true;
            } else self.fenced_member_wrote = true;
        }

        fn othersQuiescent(self: *const Self) bool {
            for (0..thread_count) |t| {
                if (t == 4 or self.done(t)) continue;
                var next = self.*;
                next.step(t);
                if (!std.mem.eql(u8, std.mem.asBytes(&next), std.mem.asBytes(self))) return false;
            }
            return true;
        }

        /// X's re-examiner: the deadline's `terminal`; host D registered (a
        /// sibling's registration): an observing path — R-1 (W43: none).
        fn tick(self: *Self) void {
            if (!(self.ticks < cfg.ticks or self.othersQuiescent())) return;
            if (self.ticks < cfg.ticks) self.ticks += 1;
            if (self.xword.pastDeadline()) {
                self.xword.terminate(true);
            } else if (self.xword.st == .pending and self.reg_d and !self.killed_d) {
                if (!cfg.observed_clear_currency or self.x_page == .live) _ = self.xword.clear() else self.xword.terminate(false);
            }
            if (self.xword.el_fired) self.xword.el_ticked = true;
        }

        pub fn invariant(self: *const Self) !void {
            var r: Rows = .{};
            r.killed_current = self.killed_current;
            r.fenced_member_wrote = self.fenced_member_wrote;
            r.pending_past_deadline = pendingPastDeadline(&self.xword, null, true);
            r.hit_register = self.hit_register;
            r.hit_readmit = self.hit_readmit;
            return checkRows(r, cfg.strict);
        }

        pub fn final(self: *const Self) !void {
            if (self.x_page != .free) return error.RecoveryStuck;
        }

        pub fn stuck(_: *const Self) !void {
            return error.StrandedAtQuiescence;
        }
    };
}

pub const LapseEvictedConfig = struct {
    /// W24: the classifier reads the rtype before its own death.
    classifier_order: ClassifierOrder = .own_death_first,
    strict: Strict = .{ .lapse = true },
};

/// `Window{.lapse_evicted}` — S (host B), X (host D) evicted and recovered
/// before L's target restart; X's one metadata write refused before the
/// restart and classified after it; J off, and S's own writes after the
/// recovery off, so X's write is the only one that can land in A3:
///   0 S   the preempt-all of host D, the bind, the deadline, X's recovery
///         (step 1, Recovering, C4, the scan, the release).
///   1 X   its write refused (after the preempt-all), its classification
///         after the restart: own death first (Recovered or Free ⇒ its
///         failure path) — W24: the rtype first (0 ⇒ retry into A3).
///   2 L   the restart, once X's recovery is done.
pub fn WindowLapseEvicted(comptime cfg: LapseEvictedConfig) type {
    return extern struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        const S_PA = 0;
        const S_BIND = 1;
        const S_FENCE = 2;
        const S_RECOVERING = 3;
        const S_REFENCE = 4;
        const S_SCAN = 5;
        const S_RELEASE = 6;
        const S_END = 7;

        rtype: u8 = 5,
        reg_d: bool = true,
        page: Page = .live,
        scanned: bool = false,
        restarted: bool = false,
        s_pc: u8 = 0,
        x_pc: u8 = 0,
        xw: Wr = .idle,
        xw_unres: bool = false,
        l_done: bool = false,
        fenced_member_wrote: bool = false,
        hit_lapse: bool = false,

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                0 => self.s_pc >= S_END,
                1 => self.x_pc >= 4,
                2 => self.l_done,
                else => unreachable,
            };
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                0 => {
                    switch (self.s_pc) {
                        S_PA => self.reg_d = false,
                        S_BIND => {},
                        S_FENCE, S_REFENCE => if (self.reg_d) {
                            self.reg_d = false;
                        },
                        S_RECOVERING => self.page = .recovering,
                        S_SCAN => {
                            self.scanned = true;
                            self.page = .recovered;
                        },
                        S_RELEASE => self.page = .free,
                        else => unreachable,
                    }
                    self.s_pc += 1;
                },
                1 => switch (self.x_pc) {
                    // Its write refused after the preempt-all.
                    0 => {
                        if (self.s_pc <= S_PA) return;
                        self.x_pc = 1;
                    },
                    // Classified after the restart.
                    1 => {
                        if (!self.restarted) return;
                        const own_death = self.page != .live;
                        const lapse = self.rtype == 0;
                        const retry = switch (cfg.classifier_order) {
                            .own_death_first => !own_death and lapse,
                            .rtype_first => lapse,
                        };
                        self.x_pc = if (retry) 2 else 4;
                    },
                    2 => {
                        if (self.rtype == 0 or self.reg_d) {
                            self.xw = .admitted;
                            self.xw_unres = self.rtype == 0;
                            self.x_pc = 3;
                        } else self.x_pc = 4;
                    },
                    3 => {
                        self.xw = .landed;
                        if (self.scanned) {
                            if (self.xw_unres) self.hit_lapse = true else self.fenced_member_wrote = true;
                        }
                        self.x_pc = 4;
                    },
                    else => unreachable,
                },
                2 => {
                    if (self.s_pc < S_END or self.xw == .admitted) return;
                    self.rtype = 0;
                    self.reg_d = false;
                    self.restarted = true;
                    self.l_done = true;
                },
                else => unreachable,
            }
        }

        pub fn invariant(self: *const Self) !void {
            var r: Rows = .{};
            r.fenced_member_wrote = self.fenced_member_wrote;
            r.hit_lapse = self.hit_lapse;
            return checkRows(r, cfg.strict);
        }

        pub fn final(self: *const Self) !void {
            if (self.page != .free) return error.RecoveryStuck;
        }

        pub fn stuck(_: *const Self) !void {
            return error.StrandedAtQuiescence;
        }
    };
}

pub const DataStallConfig = struct {
    data_deadline: DataDeadline = .own,
    ticks: u8 = 2,
    strict: Strict = .{},
};

/// `Window{.data_stall}` — metadata settled (S bound, J back on metadata),
/// J's host fenced on data by the window's data fence, and d's data rule
/// refusing at every attempt (a sibling that cannot be killed), so no park
/// is engaged and only the data word's own deadline bounds the wait:
///   0 J.data  its data write: refused ⇒ the gen captured, held, the data
///             word pending; asleep; `terminal` ⇒ its fence arm (the data
///             device latches: a deadline's own terminal, pin 1).
///   1 TICK    the data beat's periodic reaction — a kick, whose data rule
///             refuses (no state change: the kick stays, a self-loop; the
///             beat's own steps are therefore not threads here) — and the
///             deadline's `terminal` (W41: none).
///   2 ELAPSED (the data word, the premise's stall).
pub fn WindowDataStall(comptime cfg: DataStallConfig) type {
    return extern struct {
        const Self = @This();
        pub const thread_count: usize = 3;

        j_pc: u8 = 0,
        j_failed: bool = false,
        kick: bool = false,
        word: Word = .{},
        waiter: Waiter = .{},
        ticks: u8 = 0,
        live_fenced: bool = false,

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                0 => self.j_pc >= 4 or self.j_failed,
                1 => !(self.word.st == .pending or (self.waiter.held and !self.j_failed)),
                2 => self.word.el_fired or self.word.st != .pending,
                else => unreachable,
            };
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                0 => switch (self.j_pc) {
                    0 => {
                        // Refused (the fence explains it): the gen captured.
                        self.waiter.capture(&self.word);
                        self.j_pc = 1;
                    },
                    1 => {
                        self.waiter.held = true;
                        self.word.pend();
                        self.j_pc = 2;
                    },
                    2 => {
                        if (!self.waiter.wakes(&self.word, true)) {
                            self.waiter.sleeping = true;
                            return;
                        }
                        if (self.word.st == .terminal) {
                            self.j_failed = true;
                            if (!self.word.by_deadline) self.live_fenced = true;
                            self.waiter.reset();
                            return;
                        }
                        self.j_pc = 4;
                    },
                    else => unreachable,
                },
                1 => self.tick(),
                2 => self.word.elapse(),
                else => unreachable,
            }
        }

        fn othersQuiescent(self: *const Self) bool {
            for (0..thread_count) |t| {
                if (t == 1 or self.done(t)) continue;
                var next = self.*;
                next.step(t);
                if (!std.mem.eql(u8, std.mem.asBytes(&next), std.mem.asBytes(self))) return false;
            }
            return true;
        }

        fn tick(self: *Self) void {
            if (!(self.ticks < cfg.ticks or self.othersQuiescent())) return;
            if (self.ticks < cfg.ticks) self.ticks += 1;
            if (self.word.pastDeadline() and cfg.data_deadline == .own) {
                self.word.terminate(true);
            } else if (self.word.st == .pending) self.kick = true;
            if (self.word.el_fired) self.word.el_ticked = true;
        }

        pub fn invariant(self: *const Self) !void {
            var r: Rows = .{};
            r.live_fenced = self.live_fenced;
            r.pending_past_deadline = pendingPastDeadline(&self.word, &self.waiter, true);
            return checkRows(r, cfg.strict);
        }

        pub fn final(_: *const Self) !void {}

        pub fn stuck(_: *const Self) !void {
            return error.StrandedAtQuiescence;
        }
    };
}

// ---------------------------------------------------------------------------
// The checks.
// ---------------------------------------------------------------------------

const limits: explorer.Limits = .{};
const testing = std.testing;

/// Explore `M`; on a failure name its error by replaying the schedule
/// (`explorer.replayError`, which mirrors the search). Prints the evidence
/// note's line.
fn outcome(comptime name: []const u8, comptime M: type) !?anyerror {
    var report: explorer.Report = undefined;
    explorer.explore(M, testing.allocator, limits, &report) catch |explored| {
        switch (explored) {
            error.InvariantViolated, error.FinalViolated, error.StuckViolated, error.CycleViolated => {},
            else => return explored,
        }
        defer testing.allocator.free(report.schedule);
        const e = explorer.replayError(M, report.schedule);
        std.debug.print("\nmeta fence model {s}: first error {s} ({s}), schedule length {d}, {d} states: {any}\n", .{ name, @errorName(e), @errorName(explored), report.schedule.len, report.states, report.schedule });
        return @as(?anyerror, e);
    };
    std.debug.print("\nmeta fence model {s}: holds, {d} states, depth {d}\n", .{ name, report.states, report.max_depth_seen });
    return null;
}

fn expectHolds(comptime name: []const u8, comptime M: type) !void {
    if (try outcome(name, M)) |_| return error.ModelFails;
}

/// The weakening's first error is in its acceptable set (§7.5.3).
fn expectBreaks(comptime name: []const u8, comptime M: type, allowed: []const anyerror) !void {
    const e = (try outcome(name, M)) orelse return error.WeakeningNotDetected;
    for (allowed) |a| if (e == a) return;
    return error.FirstErrorOutsideAcceptableSet;
}

// ---- Death ----

test "meta fence model: Death as built (WEAR, the manager's own fence(k) report-first, C4's re-fence, the own-key guard, the rule with the data node's drain and its re-judge, a joiner's key verified at JoinAppender) holds; co-located; a joiner claiming a wrong key" {
    try expectHolds("Death{}", Death(.{}));
    try expectHolds("Death{.colocated}", Death(.{ .colocated = true }));
    try expectHolds("Death{.joiner_key}", Death(.{ .joiner_key = true }));
}

test "meta fence model weakenings on Death: W5 no rule, W14 WERO without a fencer, W15 no own-key guard, W16 no re-fence / no re-judge, W30 no joiner key verification; A5 and A4 reachable under Strict" {
    try expectBreaks("W5 rule = false (Death{})", Death(.{ .rule = false }), &.{error.FencedHostRegistered});
    try expectBreaks("W14 meta_rtype = .wero_holder (Death{})", Death(.{ .meta_rtype = .wero_holder }), &.{error.FencedMemberWrote});
    try expectBreaks("W15 own_key_guard = false (Death{})", Death(.{ .own_key_guard = false }), &.{error.ManagerFailStoppedUnproven});
    try expectBreaks("W16 refence = false (Death{})", Death(.{ .refence = false }), &.{error.FencedMemberWrote});
    try expectBreaks("W16 rejudge = false (Death{})", Death(.{ .rejudge = false }), &.{error.FencedMemberWrote});
    try expectBreaks("W30 join_key_verify = false (Death{.joiner_key})", Death(.{ .joiner_key = true, .join_key_verify = false }), &.{error.FencedMemberWrote});
    try expectBreaks("A5 Strict.register_window (Death{})", Death(.{ .strict = .{ .register_window = true } }), &.{error.RegisterWindow});
    try expectBreaks("A4 Strict.readmit_window (Death{})", Death(.{ .strict = .{ .readmit_window = true } }), &.{error.ReadmitWindow});
}

// ---- Succession ----

test "meta fence model: Succession{} as built (the three probes, the preempt-all with its drain, the generation outcome, JA re-registering through the rule after re-assertion, one retry per succession) holds" {
    try expectHolds("Succession{}", Succession(.{}));
}

test "meta fence model: Succession{.drain_reads} as built (M0 dead, JA split: the drain's read EIO, the barrier and the retry; the held write's capture, verdict, pending store, store-then-recheck, pre-sleep re-check and wait) holds" {
    try expectHolds("Succession{.drain_reads}", Succession(.{ .drain_reads = true }));
}

test "meta fence model: Succession{.drain_reads, .all_elapsed} (the as-built companion with elapsed for JA's word) holds" {
    try expectHolds("Succession{.drain_reads, .all_elapsed}", Succession(.{ .drain_reads = true, .all_elapsed = true }));
}

test "meta fence model: Succession{.chain_after_reregister} as built (S2 preempts-all after JA re-registered under S1) holds" {
    try expectHolds("Succession{.chain_after_reregister}", Succession(.{ .chain_after_reregister = true }));
}

test "meta fence model: Succession{.kept_law}, {.fresh_ja}, {.page_unreadable}, {.drain_reads, .barrier_unknown}, {.drain_reads, .barrier_unknown, .ja2} as built hold" {
    try expectHolds("Succession{.kept_law}", Succession(.{ .kept_law = true }));
    try expectHolds("Succession{.fresh_ja}", Succession(.{ .fresh_ja = true }));
    try expectHolds("Succession{.page_unreadable}", Succession(.{ .page_unreadable = true }));
    try expectHolds("Succession{.drain_reads, .barrier_unknown}", Succession(.{ .drain_reads = true, .barrier_unknown = true }));
    try expectHolds("Succession{.drain_reads, .barrier_unknown, .ja2}", Succession(.{ .drain_reads = true, .barrier_unknown = true, .ja2 = true }));
}

test "meta fence model: Succession{.m0_unlisted} and {.lapse} as built (DC20, D3: the manager's page check, admittedHere and fence inside host A's registration lock, which every non-blind rule holds from its register to its re-judge; DC24, G1: the register re-judges M0 and kills it if stale) and {.lapse, .kept_law} (DC21, D7: a conflicting lapse re-acquire stands the manager down) hold" {
    try expectHolds("Succession{.m0_unlisted}", Succession(.{ .m0_unlisted = true }));
    try expectHolds("Succession{.lapse}", Succession(.{ .lapse = true }));
    try expectHolds("Succession{.lapse, .kept_law}", Succession(.{ .lapse = true, .kept_law = true }));
}

test "meta fence model weakenings on Succession{}: W1 the predecessor's key preempted, W2 no abort, W5 no rule, W11 skip if registered, W20 no probe before the preempt, W46 the claim not first, W47 no re-registration; W3 holds; A6 reachable under Strict" {
    try expectBreaks("W1 takeover = .preempt_pred_key (Succession{})", Succession(.{ .takeover = .preempt_pred_key }), &.{ error.TwoManagers, error.WroteOverLiveClaim });
    try expectBreaks("W2 abort = false (Succession{})", Succession(.{ .abort = false }), &.{ error.OverwroteAcked, error.WroteOverLiveClaim });
    try expectBreaks("W5 rule = false (Succession{})", Succession(.{ .rule = false }), &.{ error.FencedWrote, error.TwoManagers });
    try expectBreaks("W11 skip_if_registered = true (Succession{})", Succession(.{ .skip_if_registered = true }), &.{error.TwoManagers});
    try expectBreaks("W20 probe_before_preempt = false (Succession{})", Succession(.{ .probe_before_preempt = false }), &.{error.PreemptedLive});
    try expectBreaks("W46 claim_first = false (Succession{})", Succession(.{ .claim_first = false }), &.{error.WroteOverLiveClaim});
    try expectBreaks("W47 reregister = false (Succession{})", Succession(.{ .reregister = false }), &.{error.StrandedAtQuiescence});
    try expectBreaks("A6 Strict.probe_instant (Succession{})", Succession(.{ .strict = .{ .probe_instant = true } }), &.{error.ProbeInstant});
}

test "meta fence model: W3 (probe 3's verdict unused) holds in Succession{} — the preempt-all fences every foreign host" {
    try expectHolds("W3 probe_under_hold = false (Succession{}, holds)", Succession(.{ .probe_under_hold = false }));
}

test "meta fence model weakenings on the lapse and kept-law Succession configurations: W12 no kept law, W18 no claim re-read, W25 the outcome by R0 alone, W50 a conflicting lapse acquire counted as its own, W54 the register without its re-read" {
    try expectBreaks("W12 keep_law = false (Succession{.kept_law})", Succession(.{ .kept_law = true, .keep_law = false }), &.{ error.UnreservedUnderManager, error.OverwroteAcked });
    try expectBreaks("W18 lapse_claim_reread = false (Succession{.lapse})", Succession(.{ .lapse = true, .lapse_claim_reread = false }), &.{error.OverwroteAcked});
    try expectBreaks("W18 lapse_claim_reread = false (Succession{.lapse, .kept_law})", Succession(.{ .lapse = true, .kept_law = true, .lapse_claim_reread = false }), &.{error.OverwroteAcked});
    try expectBreaks("W25 outcome_by = .r0_only (Succession{.lapse, .kept_law})", Succession(.{ .lapse = true, .kept_law = true, .outcome_by = .r0_only }), &.{ error.UnreservedUnderManager, error.OverwroteAcked });
    try expectBreaks("W50 lapse_conflict = .counts_as_reacquire (Succession{.lapse, .kept_law})", Succession(.{ .lapse = true, .kept_law = true, .lapse_conflict = .counts_as_reacquire }), &.{error.TwoManagers});
    try expectBreaks("W54 register_reread = false (Succession{.lapse})", Succession(.{ .lapse = true, .register_reread = false }), &.{error.TwoManagers});
}

test "meta fence model: the Holds row W49 alone (the manager's mutation outside the registration lock) in the atomic Succession configurations — defense in depth behind the register's atomic re-read (DC24, D12); with W54 as well it fails TwoManagers" {
    try expectHolds("W49 mutation_lock = false (Succession{.lapse}, holds)", Succession(.{ .lapse = true, .mutation_lock = false }));
    try expectHolds("W49 mutation_lock = false (Succession{}, holds)", Succession(.{ .mutation_lock = false }));
    try expectHolds("W49 mutation_lock = false (Succession{.fresh_ja}, holds)", Succession(.{ .fresh_ja = true, .mutation_lock = false }));
    try expectHolds("W49 mutation_lock = false (Succession{.lapse, .kept_law}, holds)", Succession(.{ .lapse = true, .kept_law = true, .mutation_lock = false }));
    try expectHolds("W49 mutation_lock = false (Succession{.page_unreadable}, holds)", Succession(.{ .page_unreadable = true, .mutation_lock = false }));
    try expectBreaks("W49 with W54 (Succession{.lapse})", Succession(.{ .lapse = true, .mutation_lock = false, .register_reread = false }), &.{error.TwoManagers});
}

test "meta fence model: Succession{.lapse, .split_register} as built (the product's re-read and register as two commands under the registration lock, the re-judge's mark clause, M0's grace report and its acquire's mark) holds; W49 (no lock), W55 (no mark clause), W56 (the lapse ended by a report) and W57 (the re-judge's release before its kill) fail" {
    try expectHolds("Succession{.lapse, .split_register}", Succession(.{ .lapse = true, .split_register = true }));
    try expectBreaks("W49 mutation_lock = false (Succession{.lapse, .split_register})", Succession(.{ .lapse = true, .split_register = true, .mutation_lock = false }), &.{ error.TwoManagers, error.UnreservedUnderManager });
    try expectBreaks("W55 rejudge_mark = false (Succession{.lapse, .split_register})", Succession(.{ .lapse = true, .split_register = true, .rejudge_mark = false }), &.{error.TwoManagers});
    try expectBreaks("W56 lapse_end = .on_report (Succession{.lapse, .split_register})", Succession(.{ .lapse = true, .split_register = true, .lapse_end = .on_report }), &.{error.TwoManagers});
    try expectBreaks("W57 rejudge_kill = .after_release (Succession{.lapse, .split_register})", Succession(.{ .lapse = true, .split_register = true, .rejudge_kill = .after_release }), &.{ error.TwoManagers, error.UnreservedUnderManager });
}

test "meta fence model weakenings on the blind and fresh-joiner Succession configurations: W19 no manager page check, W21 the page-only judgement; A9 reachable under Strict" {
    try expectBreaks("W19 manager_page_check = false (Succession{.m0_unlisted})", Succession(.{ .m0_unlisted = true, .manager_page_check = false }), &.{error.TwoManagers});
    try expectBreaks("W21 judge = .page_only (Succession{.fresh_ja})", Succession(.{ .fresh_ja = true, .judge = .page_only }), &.{ error.FencedWrote, error.TwoManagers });
    try expectBreaks("A9 Strict.blind_window (Succession{.m0_unlisted})", Succession(.{ .m0_unlisted = true, .strict = .{ .blind_window = true } }), &.{error.BlindWindow});
}

test "meta fence model weakenings on the split-JA Succession configurations: W22 one retry, W27 read EIO latched, W32 unknown barrier as EIO, W35 the state-only wait, W37 the generation after the verdict, W42 no deferred pending, W44 an unreadable R-1 as terminal, W48 the budget per change of mgr_writer" {
    try expectBreaks("W22 retry_budget = .once (Succession{.chain_after_reregister})", Succession(.{ .chain_after_reregister = true, .retry_budget = .once }), &.{error.LiveFenced});
    try expectBreaks("W48 retry_budget = .per_mgr_writer (Succession{.chain_after_reregister})", Succession(.{ .chain_after_reregister = true, .retry_budget = .per_mgr_writer }), &.{error.LiveFenced});
    try expectBreaks("W27 read_eio = .latch (Succession{.drain_reads})", Succession(.{ .drain_reads = true, .read_eio = .latch }), &.{error.LiveFenced});
    try expectBreaks("W32 barrier_unknown = .as_eio (Succession{.drain_reads, .barrier_unknown})", Succession(.{ .drain_reads = true, .barrier_unknown = true, .barrier_unknown_as_eio = true }), &.{error.LiveFenced});
    try expectBreaks("W35 resolution_gen = false (Succession{.chain_after_reregister})", Succession(.{ .chain_after_reregister = true, .resolution_gen = false }), &.{error.RegisteredButPending});
    try expectBreaks("W37 gen_capture = .after_verdict (Succession{.chain_after_reregister})", Succession(.{ .chain_after_reregister = true, .gen_capture = .after_verdict }), &.{error.RegisteredButPending});
    try expectBreaks("W42 meta_defer_pending = false (Succession{.drain_reads, .barrier_unknown, .ja2})", Succession(.{ .drain_reads = true, .barrier_unknown = true, .ja2 = true, .meta_defer_pending = false }), &.{error.RegisteredButPending});
    try expectBreaks("W44 r1_unreadable = .terminal (Succession{.drain_reads})", Succession(.{ .drain_reads = true, .r1_unreadable = .terminal }), &.{error.LiveFenced});
}

test "meta fence model weakening W31 (fail-stop on the first unreadable page check) in Succession{.page_unreadable}; the Holds row reassert_gate = false in the single-successor configurations" {
    try expectBreaks("W31 unreadable_failstop = true (Succession{.page_unreadable})", Succession(.{ .page_unreadable = true, .unreadable_failstop = true }), &.{error.ManagerFailStoppedUnproven});
    try expectHolds("reassert_gate = false (Succession{.page_unreadable}, holds)", Succession(.{ .page_unreadable = true, .reassert_gate = false }));
    try expectHolds("reassert_gate = false (Succession{.drain_reads, .barrier_unknown}, holds)", Succession(.{ .drain_reads = true, .barrier_unknown = true, .reassert_gate = false }));
}

// ---- Window ----

test "meta fence model: Window{} as built (the preempt-all of every member host, the expected set over rows and Live pages, the settle, W_end's reap with page keys, the evictions and the reconcile with its Live-page skip, each death's recovery) holds; and .row_missing, .row_missing_frozen, .stale_recovering" {
    try expectHolds("Window{}", Window(.{}));
    try expectHolds("Window{.row_missing}", Window(.{ .row_missing = true }));
    try expectHolds("Window{.row_missing_frozen}", Window(.{ .row_missing_frozen = true }));
    try expectHolds("Window{.stale_recovering}", Window(.{ .stale_recovering = true }));
}

test "meta fence model: Window{.volumes = 2} as built (J held until both volumes re-registered) holds" {
    try expectHolds("Window{.volumes = 2}", Window(.{ .volumes = 2 }));
}

test "meta fence model: Window{.all_elapsed} (the as-built companion) holds" {
    try expectHolds("Window{.all_elapsed}", Window(.{ .all_elapsed = true }));
}

test "meta fence model: Window{.volumes = 2, .vk_open_fails}, {.fair}, {.evicted_after_sibling}, {.lapse_evicted} (Strict.lapse), {.data_stall} as built hold" {
    try expectHolds("Window{.volumes = 2, .vk_open_fails}", Window(.{ .volumes = 2, .vk_open_fails = true }));
    try expectHolds("Window{.fair}", Window(.{ .fair = true }));
    try expectHolds("Window{.evicted_after_sibling}", WindowEvictedAfterSibling(.{}));
    try expectHolds("Window{.lapse_evicted} under Strict.lapse", WindowLapseEvicted(.{}));
    try expectHolds("Window{.data_stall}", WindowDataStall(.{}));
}

test "meta fence model: Window{.colocated_x} and Window{.lapse} as built in their trimmed forms (X's metadata write off; .lapse with X's data admit and land merged, the ledger's) hold" {
    try expectHolds("Window{.colocated_x} (X's metadata write off)", Window(.{ .colocated_x = true, .x_meta_write = false }));
    try expectHolds("Window{.lapse} (X's metadata write off, X's data admit and land merged)", Window(.{ .lapse = true, .x_meta_write = false, .x_data_split = false }));
}

test "meta fence model: Window{.late_clear} as built (DC22, D10: J's withdrawal waits for its own admitted write, up to its bound, where the registration stays) holds, W36 (a late cleared overwriting terminal) holds, W52 (a withdrawal by a plain unregister) and W53 (an unregister at the bound) fail FencedMemberWrote" {
    try expectHolds("Window{.late_clear}", WindowLateClear(.{}));
    try expectHolds("W36 terminal_absorbing = false (Window{.late_clear}, holds)", WindowLateClear(.{ .terminal_absorbing = false }));
    try expectBreaks("W52 withdraw_drains = false (Window{.late_clear})", WindowLateClear(.{ .withdraw_drains = false }), &.{error.FencedMemberWrote});
    try expectBreaks("W53 withdraw_timeout = .unregister (Window{.late_clear})", WindowLateClear(.{ .withdraw_timeout = .unregister }), &.{error.FencedMemberWrote});
}

test "meta fence model weakenings on Window{}: W4 no currency, W6 the reconcile at admission, W10 the reconcile in the window, W13 a WERO window" {
    try expectBreaks("W4 currency = false (Window{})", Window(.{ .currency = false }), &.{ error.FencedMemberWrote, error.StaleDataWrite, error.ReallocatedUnfenced });
    try expectBreaks("W6 reconcile_at_admission = true (Window{})", Window(.{ .reconcile_at_admission = true }), &.{error.ReconcileFencedLive});
    try expectBreaks("W10 reconcile_in_window = true (Window{})", Window(.{ .reconcile_in_window = true }), &.{error.ReconcileFencedLive});
    try expectBreaks("W13 window_rtype = .wero_holder (Window{})", Window(.{ .window_rtype = .wero_holder }), &.{error.FencedMemberWrote});
}

test "meta fence model weakenings on Window: W9 no reap key, W45 the registration-only judgement (.colocated_x), W17 the lapse's preempt-all (.lapse), W23 rows only and no skip (.row_missing; either half alone holds), W26 the release after one volume, W28 the reap on rows only, W29 no sibling filter, W47 no re-registration (.fair); A3 and A5 reachable under Strict" {
    try expectBreaks("W9 reap_key = false (Window{.colocated_x})", Window(.{ .colocated_x = true, .reap_key = false }), &.{ error.ReallocatedUnfenced, error.FencedMemberWrote });
    try expectBreaks("W45 judge = .registration_only (Window{.colocated_x})", Window(.{ .colocated_x = true, .judge = .registration_only }), &.{error.KilledCurrent});
    try expectBreaks("W17 lapse_preempt_all = true (Window{.lapse})", Window(.{ .lapse = true, .x_data_split = false, .lapse_preempt_all = true }), &.{error.LiveFenced});
    try expectBreaks("W23 expected = .rows_only, reconcile_live_page_skip = false (Window{.row_missing})", Window(.{ .row_missing = true, .expected = .rows_only, .reconcile_live_page_skip = false }), &.{error.ReconcileFencedLive});
    try expectHolds("W23's half: expected = .rows_only alone (Window{.row_missing}, holds)", Window(.{ .row_missing = true, .expected = .rows_only }));
    try expectHolds("W23's half: reconcile_live_page_skip = false alone (Window{.row_missing}, holds)", Window(.{ .row_missing = true, .reconcile_live_page_skip = false }));
    try expectBreaks("W26 release_after_all_volumes = false (Window{.volumes = 2})", Window(.{ .volumes = 2, .release_after_all_volumes = false }), &.{error.LiveFenced});
    try expectBreaks("W28 reap_scope = .rows_only (Window{.row_missing_frozen})", Window(.{ .row_missing_frozen = true, .reap_scope = .rows_only }), &.{error.RecoveryStuck});
    try expectBreaks("W29 sibling_filter = false (Window{.stale_recovering})", Window(.{ .stale_recovering = true, .sibling_filter = false }), &.{error.LiveFenced});
    try expectBreaks("W47 reregister = false (Window{.fair})", Window(.{ .fair = true, .reregister = false }), &.{error.StrandedAtQuiescence});
    try expectBreaks("A3 Strict.lapse (Window{.lapse})", Window(.{ .lapse = true, .x_data_split = false, .strict = .{ .lapse = true } }), &.{error.LapseWindow});
    try expectBreaks("A5 Strict.register_window (Window{.colocated_x})", Window(.{ .colocated_x = true, .strict = .{ .register_window = true } }), &.{error.RegisterWindow});
}

test "meta fence model weakenings on the dedicated Window configurations: W24 the rtype before own death (.lapse_evicted, Strict.lapse), W41 no data deadline (.data_stall), W43 an observing cleared without R-1 paired with rejudge = false (.evicted_after_sibling; its intact-R-1 companion and W43 with the re-judge hold); A4 reachable under Strict" {
    try expectBreaks("W24 classifier_order = .rtype_first (Window{.lapse_evicted}, Strict.lapse)", WindowLapseEvicted(.{ .classifier_order = .rtype_first }), &.{error.LapseWindow});
    try expectBreaks("W41 data_deadline = .park_only (Window{.data_stall})", WindowDataStall(.{ .data_deadline = .park_only }), &.{error.PendingPastDeadline});
    try expectBreaks("W43 observed_clear_currency = false, rejudge = false (Window{.evicted_after_sibling})", WindowEvictedAfterSibling(.{ .observed_clear_currency = false, .rejudge = false }), &.{error.FencedMemberWrote});
    try expectHolds("rejudge = false alone, R-1 intact (Window{.evicted_after_sibling}, holds)", WindowEvictedAfterSibling(.{ .rejudge = false }));
    try expectHolds("W43 with the re-judge intact (Window{.evicted_after_sibling}, holds)", WindowEvictedAfterSibling(.{ .observed_clear_currency = false }));
    try expectBreaks("A4 Strict.readmit_window (Window{.evicted_after_sibling})", WindowEvictedAfterSibling(.{ .strict = .{ .readmit_window = true } }), &.{error.ReadmitWindow});
}

// ---- Debt ----

test "meta fence model: Debt{} and Debt{.bind_race} as built (the debt written in the claim transaction, carried through the chain, paid at the data admission behind its metadata re-check and page check, before any reallocation) hold" {
    try expectHolds("Debt{}", Debt(.{}));
    try expectHolds("Debt{.bind_race}", Debt(.{ .bind_race = true }));
}

test "meta fence model: Debt{.ja} split by operation as built (op 2 alone; op 1 alone with M's data write off, its .unreadable form, and its .all_elapsed form with S1's data write off and one budgeted tick) holds" {
    try expectHolds("Debt{.ja, .op = .two}", Debt(.{ .ja = true, .op = .two }));
    try expectHolds("Debt{.ja, .op = .one} (M's data write off)", Debt(.{ .ja = true, .op = .one, .m_data_write = false }));
    try expectHolds("Debt{.ja, .unreadable, .op = .one} (M's data write off)", Debt(.{ .ja = true, .unreadable = true, .op = .one, .m_data_write = false }));
    try expectHolds("Debt{.ja, .all_elapsed, .op = .one} (M's data write off, S1's data write off, N_tick = 1)", Debt(.{ .ja = true, .all_elapsed = true, .op = .one, .m_data_write = false, .s1_write = false, .ticks = 1 }));
}

test "meta fence model weakening W51 (revision 15's synchronous arm declining when its re-judge finds the metadata registration gone) fails LiveFenced on op 2 alone (Debt{.ja, .op = .two}) — DC23 keeps the registration (D8)" {
    try expectBreaks("W51 sync_arm_failure = .decline (Debt{.ja, .op = .two})", Debt(.{ .ja = true, .op = .two, .sync_arm_failure = .decline }), &.{error.LiveFenced});
}

test "meta fence model weakening W39 (the synchronous arm without its cleared) fails ClearedAfterEstablish on Debt{.ja} with both operations — only op 1's held verdict pends the data word that op 2's arm then leaves pending; the failing search stops within the bound (D13)" {
    try expectBreaks("W39 sync_arm_clears = false (Debt{.ja}, both operations)", Debt(.{ .ja = true, .sync_arm_clears = false }), &.{error.ClearedAfterEstablish});
}

test "meta fence model weakenings on Debt: W3 the predecessor from probe 2 (.bind_race), W7 no debt, W8 no chain; A2 reachable under Strict; on op 1 alone (M's data write off): W34 decline while pending, W37 the generation after the verdict, W38 no recheck and kick-only, W40 kick-only with unreadable inputs" {
    try expectBreaks("W3 probe_under_hold = false (Debt{.bind_race})", Debt(.{ .bind_race = true, .probe_under_hold = false }), &.{error.ReallocatedUnfenced});
    try expectBreaks("W7 debt = false (Debt{})", Debt(.{ .debt = false }), &.{error.ReallocatedUnfenced});
    try expectBreaks("W8 debt_chain = false (Debt{})", Debt(.{ .debt_chain = false }), &.{error.ReallocatedUnfenced});
    try expectBreaks("W34 data_refusal = .decline_while_pending (Debt{.ja, .op = .one})", Debt(.{ .ja = true, .op = .one, .m_data_write = false, .data_refusal = .decline_while_pending }), &.{error.LiveFenced});
    try expectBreaks("W37 gen_capture = .after_verdict (Debt{.ja, .op = .one})", Debt(.{ .ja = true, .op = .one, .m_data_write = false, .gen_capture = .after_verdict }), &.{error.RegisteredButPending});
    try expectBreaks("W38 store_then_recheck = false, data_beat = .kick_only (Debt{.ja, .op = .one})", Debt(.{ .ja = true, .op = .one, .m_data_write = false, .store_then_recheck = false, .data_beat = .kick_only }), &.{error.RegisteredButPending});
    try expectBreaks("W40 data_beat = .kick_only (Debt{.ja, .unreadable, .op = .one})", Debt(.{ .ja = true, .unreadable = true, .op = .one, .m_data_write = false, .data_beat = .kick_only }), &.{error.RegisteredButPending});
    try expectBreaks("A2 Strict.debt_window (Debt{})", Debt(.{ .strict = .{ .debt_window = true } }), &.{error.DebtWindow});
}

test "meta fence model: W38's halves on op 1 alone (D9) — W38a (no store-then-recheck, the periodic tick intact) holds; W38b (the kick-only beat alone: the store-then-recheck meets the word's one unreadable read) fails RegisteredButPending" {
    try expectHolds("W38a store_then_recheck = false alone (Debt{.ja, .op = .one}, holds)", Debt(.{ .ja = true, .op = .one, .m_data_write = false, .store_then_recheck = false }));
    try expectBreaks("W38b data_beat = .kick_only alone (Debt{.ja, .op = .one})", Debt(.{ .ja = true, .op = .one, .m_data_write = false, .data_beat = .kick_only }), &.{error.RegisteredButPending});
}
