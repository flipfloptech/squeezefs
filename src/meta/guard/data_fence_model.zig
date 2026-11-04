//! The MODELS of DATA-namespace fencing (decision 0003 §DJ; charter §5.5
//! "Fencing", §6.2 "Block reclaim"), written against the inputs
//! `guard/data_fence.zig` and the recovery's data fence read. Dependency-
//! free: one atomic action per `step`, explored exhaustively by
//! `src/model/explorer.zig`.
//!
//! A data namespace is reserved Write Exclusive – All Registrants (rtype 5):
//! every member host registers under its host key and EVERY registrant may
//! preempt by key — nvmet 7.3's `nvmet_pr_preempt` runs its all-registrants
//! arm (`nvmet_pr_unreg_all_host_by_prkey`) before the single-holder `holder
//! == reg` re-type (§DF.11's quirk), so the manager's own client fences on
//! data. A write from a host id with no registrant is refused
//! (`nvmet_pr_check_cmd_access`); the reservation ends with its last
//! registrant (`nvmet_pr_unregister_one`). A write is ISSUED by its host (into
//! the block queue), ADMITTED at the target by the access check, then LANDS; a
//! preempt-and-abort completes only after the victim's admitted writes landed
//! (`nvmet_pr_do_abort`); a killed process's queued write still arrives (a
//! requeue).
//!
//! `Death` — data namespaces D0 and D1; hosts A (the manager MG), B (the dead
//! member X; S, a sibling mounting there), C (Y, a live member):
//!   0 X0  X's in-flight write to its block on D0, frozen: issued at any point
//!         while X lives (the resume), admitted or refused at its arrival,
//!         landed.
//!   1 X1  the same on D1.
//!   2 MG  X's recovery: the metadata preempt of host B's key, the page
//!         Recovering, the DATA FENCE (per namespace: the preempt-and-abort of
//!         host B's key when it is registered, then the report — reserved and
//!         the key absent), the release only once both are PROVEN, else the
//!         next poll re-runs (five rounds); with `colocated`, W's recovery
//!         (the own-key guard: the recoverer's rule kills W on its own host
//!         and drains its queue); then MG's clean unmount (the kept law: the
//!         last registrant stays registered when it preempted there).
//!   3 Y   after the release: X's blocks on D0 and D1 reallocated (with
//!         `colocated`, W's too); then Y leaves (never the last registrant to
//!         unreserve: it acquired nothing).
//!   4 S   a sibling's admission on host B: the metadata (adopt, else the
//!         rule, the register, the re-judge), then per data namespace adopt or
//!         the rule (X killed once its page left Live), the data node's drain,
//!         the register, the re-judge and the check that host B is still
//!         registered on the metadata namespace.
//!   5 W   (`colocated`) a member co-located with MG on host A, frozen with a
//!         write to its block on D0.
//!
//! `Succession` — one data namespace; M (host A) the manager, frozen with an
//! in-place write to one of its file's blocks and, `late_admission`, its own
//! data admission still running; S1 (host B) takes the metadata hold
//! (preempting the holder's key), admits on data and reconciles; S2 (host C)
//! takes the hold from whoever holds it, admits, reconciles — every data
//! registrant volume 0's metadata namespace no longer registers, the data
//! report read first, then the metadata's — and reallocates M's block.
//!
//! Invariants: `StaleDataWrite` no write of a fenced incarnation lands on a
//! block reallocated after its death (the live member's bytes overwritten);
//! `ReallocatedUnfenced` no block is reallocated while its dead writer can
//! still land a write there (its host registered or the namespace
//! unreserved, or a write already admitted); `KilledCurrent` a rule never
//! kills a member whose page is Live. `final` (`Death`): `RecoveryStuck` the
//! dead region is released.

const std = @import("std");

/// The data namespaces' reservation.
pub const DataRtype = enum {
    /// Write Exclusive – All Registrants: every registrant preempts by key
    /// (as built).
    wear,
    /// WERO held by the manager, its own death preempt — §DG.12's sketch
    /// without a data fencer: nvmet 7.3's holder preempt re-types its own
    /// hold and fences nothing.
    wero_holder,
    /// No registration, no reservation (the code before this rung).
    none,
};

/// Where the recovery's data fence sits.
pub const FenceOrder = enum {
    /// After the page went Recovering, before the release (as built).
    after_recovering,
    /// Before the page goes Recovering: a sibling's rule still judges the
    /// member Live and re-admits its host.
    before_recovering,
    /// After the release: the blocks reallocated before the fence.
    after_release,
};

pub const DeathConfig = struct {
    data_rtype: DataRtype = .wear,
    order: FenceOrder = .after_recovering,
    /// An unproven data fence proceeds (the metadata plane's posture, which
    /// its frame screen backs — data has no screen); false: the release waits.
    proceed_unproven: bool = false,
    /// RACQA 2: the target drains the victim's admitted writes first.
    abort: bool = true,
    /// A fresh data registration runs the stale-sibling rule and its
    /// re-judge.
    data_rule: bool = true,
    /// ... and drains the data node's block queue after a kill.
    drain: bool = true,
    /// The last registrant keeps its registration where it preempted.
    keep_law: bool = true,
    /// A member co-located with the manager (host A) dies too.
    colocated: bool = false,
    /// Its recoverer — on the same host — kills it by the rule and drains
    /// its queue before the release (false: the own-key guard alone).
    colocated_kill: bool = true,
    /// D1 was transiently absent at the manager's admission (its node
    /// reconnecting): the manager holds no client there until a remedy (a
    /// remount's admission once the node is back) — review MAJOR 1.
    transient_d1: bool = false,
    /// Step 2c holds while a non-retired set-config volume is neither fenced
    /// nor provably unfenceable (as built); false: only the manager's table
    /// is fenced (an untabled volume skipped).
    complete_check: bool = true,
};

const Page = enum(u8) { live, recovering, released };
const Wr = enum(u8) { idle, queued, admitted, landed, refused, none };

const A: u8 = 0;
const B: u8 = 1;
const C: u8 = 2;

const MgStep = enum(u8) { meta, recovering, p0, v0, p1, v1, decide, w_recovering, w_kill, w_drain, w_decide, unmount };

fn mgSequence(comptime cfg: DeathConfig) []const MgStep {
    const x: []const MgStep = switch (cfg.order) {
        .after_recovering => &.{ .meta, .recovering, .p0, .v0, .p1, .v1, .decide },
        .before_recovering => &.{ .meta, .p0, .v0, .p1, .v1, .recovering, .decide },
        .after_release => &.{ .meta, .recovering, .decide, .p0, .v0, .p1, .v1 },
    };
    const w: []const MgStep = if (cfg.colocated) &.{ .w_recovering, .w_kill, .w_drain, .w_decide } else &.{};
    return x ++ w ++ &[_]MgStep{.unmount};
}

fn indexOf(comptime seq: []const MgStep, comptime s: MgStep) u8 {
    for (seq, 0..) |e, i| if (e == s) return i;
    unreachable;
}

pub fn Death(comptime cfg: DeathConfig) type {
    return extern struct {
        const Self = @This();
        pub const thread_count: usize = 5 + @as(usize, @intFromBool(cfg.colocated)) + @intFromBool(cfg.transient_d1);
        /// The remedy's thread (`transient_d1`): D1 joins the manager's table.
        const T_REMEDY: usize = if (cfg.colocated) 6 else 5;
        const SEQ = mgSequence(cfg);
        const P0 = indexOf(SEQ, .p0);
        // The next poll re-runs: one round per registration a sibling can
        // slip between a preempt and its proof (two per namespace), plus one
        // (plus one for a round run while D1 was untabled).
        const ROUNDS = if (cfg.transient_d1) 6 else 5;
        const S_DATA = 3;
        const S_END = S_DATA + 8;
        const fenced_ns = cfg.data_rtype != .none;

        reserved: [2]bool = @splat(fenced_ns),
        reg_a: [2]bool = @splat(fenced_ns),
        reg_b: [2]bool = @splat(fenced_ns),
        reg_c: [2]bool = @splat(fenced_ns),
        /// Host B registered on the metadata namespace.
        meta_b: bool = true,
        x_alive: bool = true,
        x_page: Page = .live,
        xw: [2]Wr = @splat(.idle),
        w_alive: bool = cfg.colocated,
        w_page: Page = .live,
        ww: Wr = if (cfg.colocated) .idle else .none,
        realloc: [2]bool = @splat(false),
        realloc_w: bool = false,
        /// This round's proofs.
        fenced: [2]bool = @splat(false),
        /// MG's client preempted there (the kept law).
        preempted: [2]bool = @splat(false),
        released: bool = false,
        w_released: bool = false,
        round: u8 = 0,
        /// The manager holds a client on the namespace (its admission).
        tabled: [2]bool = .{ true, !cfg.transient_d1 },
        pc: [7]u8 = @splat(0),
        stale_write: bool = false,
        realloc_unfenced: bool = false,
        killed_current: bool = false,

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            if (cfg.transient_d1 and t == T_REMEDY) return self.pc[t] >= 1;
            return switch (t) {
                0, 1 => self.pc[t] >= 3,
                2 => self.pc[2] >= SEQ.len,
                3 => self.pc[3] >= 4,
                4 => self.pc[4] >= S_END,
                5 => self.pc[5] >= 3,
                else => unreachable,
            };
        }

        /// The target's access check for host `h`'s write on namespace `i`.
        fn admits(self: *const Self, h: u8, i: usize) bool {
            if (!self.reserved[i]) return true;
            return switch (h) {
                A => self.reg_a[i],
                B => self.reg_b[i],
                C => self.reg_c[i],
                else => unreachable,
            };
        }

        /// X can still land a write on namespace `i`.
        fn xCanLand(self: *const Self, i: usize) bool {
            return switch (self.xw[i]) {
                .admitted => true,
                .queued => self.admits(B, i),
                .idle => self.x_alive and self.admits(B, i),
                .landed, .refused, .none => false,
            };
        }

        fn wCanLand(self: *const Self) bool {
            return switch (self.ww) {
                .admitted => true,
                .queued => self.admits(A, 0),
                .idle => self.w_alive and self.admits(A, 0),
                .landed, .refused, .none => false,
            };
        }

        /// A frozen member's write on namespace `i`: issue, arrival, landing.
        fn write(self: *Self, t: usize, w: *Wr, alive: bool, host: u8, i: usize, reallocated: bool) void {
            switch (self.pc[t]) {
                0 => if (!alive) {
                    w.* = .none;
                    self.pc[t] = 3;
                    return;
                } else {
                    w.* = .queued;
                },
                1 => if (self.admits(host, i)) {
                    w.* = .admitted;
                } else {
                    w.* = .refused;
                    self.pc[t] = 3;
                    return;
                },
                2 => {
                    w.* = .landed;
                    if (reallocated) self.stale_write = true;
                },
                else => unreachable,
            }
            self.pc[t] += 1;
        }

        pub fn step(self: *Self, t: usize) void {
            if (cfg.transient_d1 and t == T_REMEDY) {
                self.tabled[1] = true;
                self.pc[t] = 1;
                return;
            }
            switch (t) {
                0 => self.write(0, &self.xw[0], self.x_alive, B, 0, self.realloc[0]),
                1 => self.write(1, &self.xw[1], self.x_alive, B, 1, self.realloc[1]),
                2 => self.manager(),
                3 => self.live(),
                4 => self.sibling(),
                5 => self.write(5, &self.ww, self.w_alive, A, 0, self.realloc_w),
                else => unreachable,
            }
        }

        /// The data fence's preempt on namespace `i`: host B's key, issued by
        /// MG's own client (a registrant). Waits while X has a write
        /// admitted there (`abort`). An untabled namespace: no client, no
        /// command.
        fn preempt(self: *Self, i: usize) bool {
            if (cfg.data_rtype == .none or !self.tabled[i]) return true;
            if (cfg.abort and self.xw[i] == .admitted) return false;
            if (!self.reg_a[i] or !self.reg_b[i]) return true;
            switch (cfg.data_rtype) {
                .wear => {
                    self.reg_b[i] = false;
                    self.preempted[i] = true;
                },
                // nvmet 7.3: the holder's preempt of another key re-types its
                // own hold; the victim stays registered.
                .wero_holder => {},
                .none => unreachable,
            }
            return true;
        }

        /// The report after it: the namespace reserved and the key absent
        /// (no device fence on a namespace without PR: the posture proceeds).
        fn verify(self: *Self, i: usize) void {
            self.fenced[i] = cfg.data_rtype == .none or (self.tabled[i] and self.reserved[i] and !self.reg_b[i]);
        }

        /// Namespace `i` counts as fenced at the decision: proven, or (only
        /// without the completeness check) skipped because untabled.
        fn proofOf(self: *const Self, i: usize) bool {
            if (!self.tabled[i] and !cfg.complete_check) return true;
            return self.fenced[i];
        }

        fn manager(self: *Self) void {
            switch (SEQ[self.pc[2]]) {
                .meta => self.meta_b = false,
                .recovering => self.x_page = .recovering,
                .p0 => if (!self.preempt(0)) return,
                .v0 => self.verify(0),
                .p1 => if (!self.preempt(1)) return,
                .v1 => self.verify(1),
                .decide => {
                    const proven = self.proofOf(0) and self.proofOf(1);
                    if (cfg.order == .after_release or proven or cfg.proceed_unproven) {
                        self.released = true;
                        self.x_page = .released;
                    } else if (cfg.transient_d1 and !self.tabled[1] and !self.done(T_REMEDY)) {
                        // A set-config volume outside the table: the
                        // recovery holds (re-polls) until the remedy.
                        return;
                    } else if (self.round + 1 < ROUNDS) {
                        self.round += 1;
                        self.fenced = @splat(false);
                        self.pc[2] = P0;
                        return;
                    }
                },
                .w_recovering => self.w_page = .recovering,
                .w_kill => if (cfg.colocated_kill and self.w_alive) {
                    if (self.w_page == .live) self.killed_current = true;
                    self.w_alive = false;
                },
                .w_drain => if (cfg.colocated_kill and (self.ww == .queued or self.ww == .admitted)) return,
                .w_decide => self.w_released = true,
                .unmount => self.leave(A),
            }
            self.pc[2] += 1;
        }

        /// Host `h`'s departure as the host's last process: another
        /// registrant stands ⇒ unregister (the reservation stays); the
        /// namespace's last registrant keeps its registration where the kept
        /// law says so (MG preempted there; Y acquired nothing).
        fn leave(self: *Self, h: u8) void {
            if (cfg.data_rtype == .none) return;
            for (0..2) |i| {
                const mine = switch (h) {
                    A => &self.reg_a[i],
                    C => &self.reg_c[i],
                    else => unreachable,
                };
                if (!mine.*) continue;
                const others = switch (h) {
                    A => self.reg_b[i] or self.reg_c[i],
                    C => self.reg_a[i] or self.reg_b[i],
                    else => unreachable,
                };
                if (cfg.data_rtype == .wero_holder and h == A) {
                    // The holder keeps the hold while another registrant stands.
                    if (others) continue;
                } else if (others) {
                    mine.* = false;
                    continue;
                }
                const keep = cfg.keep_law and (h != A or self.preempted[i]);
                if (keep) continue;
                mine.* = false;
                self.reserved[i] = false;
            }
        }

        fn live(self: *Self) void {
            // A recovery that gave up released nothing: Y reallocates nothing.
            if (self.pc[3] < 3 and !self.released and self.done(2)) {
                self.pc[3] = 3;
                return;
            }
            switch (self.pc[3]) {
                0, 1 => {
                    if (!self.released) return;
                    const i: usize = self.pc[3];
                    if (self.xCanLand(i)) self.realloc_unfenced = true;
                    self.realloc[i] = true;
                },
                2 => if (cfg.colocated) {
                    if (!self.w_released) return;
                    if (self.wCanLand()) self.realloc_unfenced = true;
                    self.realloc_w = true;
                },
                3 => self.leave(C),
                else => unreachable,
            }
            self.pc[3] += 1;
        }

        fn killStale(self: *Self) void {
            if (!self.x_alive or self.x_page == .live) return;
            self.x_alive = false;
        }

        fn sibling(self: *Self) void {
            const pc = self.pc[4];
            if (pc < S_DATA) {
                switch (pc) {
                    0 => if (self.meta_b) {
                        self.pc[4] = S_DATA;
                        return;
                    } else self.killStale(),
                    1 => self.meta_b = true,
                    2 => if (self.x_alive and self.x_page != .live) {
                        self.meta_b = false;
                        self.killStale();
                        self.pc[4] = 1;
                        return;
                    },
                    else => unreachable,
                }
                self.pc[4] += 1;
                return;
            }
            const i: usize = (pc - S_DATA) / 4;
            const base: u8 = S_DATA + @as(u8, @intCast(i)) * 4;
            switch (pc - base) {
                0 => if (cfg.data_rtype == .none or self.reg_b[i]) {
                    self.pc[4] = base + 4;
                    return;
                } else if (cfg.data_rule) self.killStale(),
                1 => if (cfg.drain and (self.xw[i] == .queued or self.xw[i] == .admitted)) return,
                2 => self.reg_b[i] = true,
                3 => {
                    if (cfg.data_rule and self.x_alive and self.x_page != .live) {
                        self.reg_b[i] = false;
                        self.killStale();
                        self.pc[4] = base + 1;
                        return;
                    }
                    if (!self.meta_b) {
                        self.reg_b[i] = false;
                        self.pc[4] = S_END;
                        return;
                    }
                },
                else => unreachable,
            }
            self.pc[4] += 1;
        }

        pub fn invariant(self: *const Self) !void {
            if (self.killed_current) return error.KilledCurrent;
            if (self.realloc_unfenced) return error.ReallocatedUnfenced;
            if (self.stale_write) return error.StaleDataWrite;
        }

        pub fn final(self: *const Self) !void {
            if (!self.released or (cfg.colocated and !self.w_released)) return error.RecoveryStuck;
        }
    };
}

/// What the successor's data admission fences on data.
pub const Reconcile = enum {
    /// Every data registrant the metadata namespace no longer registers
    /// (as built): a chain of superseded managers included.
    unregistered_on_meta,
    /// Only the key its own metadata hold preempted.
    victim_only,
    /// Nothing (the code before this rung).
    none,
};

pub const SuccessionConfig = struct {
    data_rtype: enum { wear, none } = .wear,
    reconcile: Reconcile = .unregistered_on_meta,
    /// M froze inside its own data admission.
    late_admission: bool = false,
    /// The admission checks, after its data register, that the host is still
    /// registered on the metadata namespace (withdrawn otherwise).
    meta_recheck: bool = true,
    /// A joiner on host D leaves — data, then metadata — at any point: between
    /// a successor's two reports its key is seen on data, absent from
    /// metadata, and gone by the preempt (review minor 1).
    leaver: bool = false,
    /// A reconcile preempt the target refuses (the key already gone): the
    /// data report read again and an absent key is success (as built), or
    /// the admission refused.
    preempt_error: enum { reread, refuse } = .reread,
};

const D: u8 = 3;
const NONE: u8 = 4;

pub fn Succession(comptime cfg: SuccessionConfig) type {
    return extern struct {
        const Self = @This();
        pub const thread_count: usize = 3 + @as(usize, @intFromBool(cfg.leaver));
        /// M's steps: the admission (check, register, re-check), then the
        /// write (issue, arrival, landing).
        const M_WRITE = 3;
        const M_END = 6;
        /// A successor's steps: the metadata register, the hold, the data
        /// register, its re-check, the data report, the metadata report, the
        /// preempts; S2 then reallocates.
        const S_END = 7;

        reserved: bool = cfg.data_rtype == .wear,
        dreg: [4]bool = .{ cfg.data_rtype == .wear and !cfg.late_admission, false, false, cfg.data_rtype == .wear and cfg.leaver },
        mreg: [4]bool = .{ true, false, false, cfg.leaver },
        holder: u8 = A,
        m_admitted: bool = !cfg.late_admission,
        m_refused: bool = false,
        mw: Wr = .idle,
        realloc: bool = false,
        victim: [2]u8 = @splat(NONE),
        dsnap: [2][4]bool = @splat(@splat(false)),
        msnap: [2][4]bool = @splat(@splat(false)),
        stopped: [2]bool = @splat(false),
        /// A successor's admission refused over a key that was already gone.
        refused_spuriously: bool = false,
        pc: [4]u8 = .{ if (cfg.late_admission) 0 else M_WRITE, 0, 0, 0 },
        stale_write: bool = false,
        realloc_unfenced: bool = false,

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                0 => self.pc[0] >= M_END,
                1 => self.pc[1] >= S_END or self.stopped[0],
                2 => self.pc[2] >= S_END + 1 or self.stopped[1],
                3 => self.pc[3] >= 2,
                else => unreachable,
            };
        }

        fn admits(self: *const Self, h: u8) bool {
            return !self.reserved or self.dreg[h];
        }

        /// M can still land its in-place write.
        fn mCanLand(self: *const Self) bool {
            switch (self.mw) {
                .admitted => return true,
                .queued => return self.admits(A),
                .landed, .refused, .none => return false,
                .idle => {},
            }
            if (self.m_refused) return false;
            if (self.m_admitted) return self.admits(A);
            // Its admission still running: the check before the register,
            // else the re-check after it decides.
            if (cfg.data_rtype == .none) return true;
            if (self.pc[0] == 0) return self.mreg[A];
            return !cfg.meta_recheck or self.mreg[A];
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                0 => self.manager(),
                1 => self.successor(0, B),
                2 => self.successor(1, C),
                3 => self.leave(),
                else => unreachable,
            }
        }

        /// The joiner on host D leaves: data before metadata.
        fn leave(self: *Self) void {
            switch (self.pc[3]) {
                0 => self.dreg[D] = false,
                1 => self.mreg[D] = false,
                else => unreachable,
            }
            self.pc[3] += 1;
        }

        fn manager(self: *Self) void {
            switch (self.pc[0]) {
                0 => if (!self.mreg[A]) {
                    self.m_refused = true;
                    self.pc[0] = M_END;
                    return;
                },
                1 => if (cfg.data_rtype == .wear) {
                    self.dreg[A] = true;
                },
                2 => {
                    if (cfg.meta_recheck and !self.mreg[A]) {
                        self.dreg[A] = false;
                        self.m_refused = true;
                        self.pc[0] = M_END;
                        return;
                    }
                    self.m_admitted = true;
                },
                3 => self.mw = .queued,
                4 => if (self.admits(A)) {
                    self.mw = .admitted;
                } else {
                    self.mw = .refused;
                    self.pc[0] = M_END;
                    return;
                },
                5 => {
                    self.mw = .landed;
                    if (self.realloc) self.stale_write = true;
                },
                else => unreachable,
            }
            self.pc[0] += 1;
        }

        fn successor(self: *Self, s: usize, h: u8) void {
            switch (self.pc[s + 1]) {
                0 => self.mreg[h] = true,
                // The metadata hold: the holder's key preempted.
                1 => if (self.holder != h) {
                    self.victim[s] = self.holder;
                    self.mreg[self.holder] = false;
                    self.holder = h;
                },
                // A superseded successor's admission stops at its re-check or
                // its hold check (its acts refused from here).
                2 => if (cfg.data_rtype == .wear) {
                    if (!self.mreg[h]) {
                        self.stopped[s] = true;
                        return;
                    }
                    self.dreg[h] = true;
                },
                3 => if (cfg.data_rtype == .wear and cfg.meta_recheck and !self.mreg[h]) {
                    self.dreg[h] = false;
                    self.stopped[s] = true;
                    return;
                },
                4 => self.dsnap[s] = self.dreg,
                5 => self.msnap[s] = self.mreg,
                6 => {
                    if (self.holder != h) {
                        self.stopped[s] = true;
                        return;
                    }
                    // Preempt-and-abort: M's admitted write lands first.
                    if (self.mw == .admitted) return;
                    if (cfg.data_rtype == .wear and self.dreg[h]) for (0..4) |k| {
                        if (k == h) continue;
                        const fence = switch (cfg.reconcile) {
                            .unregistered_on_meta => self.dsnap[s][k] and !self.msnap[s][k],
                            .victim_only => k == self.victim[s],
                            .none => false,
                        };
                        if (!fence) continue;
                        if (self.dreg[k]) {
                            self.dreg[k] = false;
                            continue;
                        }
                        // The key left between the reports and the preempt:
                        // the target answers a reservation conflict.
                        if (cfg.preempt_error == .refuse and cfg.reconcile == .unregistered_on_meta) {
                            self.refused_spuriously = true;
                            self.stopped[s] = true;
                            return;
                        }
                    };
                },
                // S2's first carve after its admission: M's block reallocated.
                7 => {
                    if (self.mCanLand()) self.realloc_unfenced = true;
                    self.realloc = true;
                },
                else => unreachable,
            }
            self.pc[s + 1] += 1;
        }

        pub fn invariant(self: *const Self) !void {
            if (self.realloc_unfenced) return error.ReallocatedUnfenced;
            if (self.stale_write) return error.StaleDataWrite;
        }

        pub fn final(self: *const Self) !void {
            if (self.refused_spuriously) return error.ReconcileRefused;
        }
    };
}

/// The accepted windows of a lapse made violations (each shown reachable).
pub const LapseStrict = struct {
    /// A write admitted while the namespace is unreserved after the target
    /// forgot every fence (from the restart until the re-reservation).
    lapse: bool = false,
    /// A member's own lapse re-registration admits its write before its
    /// post-check withdraws it (frozen between its pre-check and its
    /// register across its whole death).
    beat_window: bool = false,
};

pub const LapseConfig = struct {
    /// The metadata namespace lapses with the data namespace (one target).
    meta_too: bool = true,
    /// Members re-register after a lapse (the beat); false: the code before.
    member_beat: bool = true,
    /// The manager re-acquires the metadata hold and re-reserves the data
    /// namespace after a lapse; false: nobody does.
    manager_beat: bool = true,
    /// The manager waits the grace (every live member's beat first) before
    /// it re-acquires or re-reserves.
    grace: bool = true,
    /// A member's data re-registration requires its host registered on the
    /// metadata namespace (before and after).
    meta_check: bool = true,
    /// A member re-registers only while its page is Live under its own
    /// writer id (before and after).
    currency: bool = true,
    strict: LapseStrict = .{},
};

/// `Lapse` — a PTPL-less target restart (nvmet keeps no PTPL: every namespace
/// enable starts with no registrant and no reservation, pr.c:1136) under a
/// running set: the manager MG (host A) holds the metadata namespace (WERO)
/// and every host is registered on it and on the WEAR data namespace; J (host
/// C) a live joiner writing data and metadata; X (host B) a member frozen
/// with a data write, its death recovered by MG at any point:
///   0 L    the restart: every registration and both reservations forgotten
///          (`meta_too`: the metadata namespace on the same target), once the
///          namespace's in-flight requests completed.
///   1 MGR  X's recovery: the metadata preempt of host B, the page
///          Recovering, the re-fence, the data preempt-and-abort (MG's own
///          client, registered), the proof (reserved WEAR, the key absent);
///          unproven while the lapse is open, the next poll; then the release
///          and J's reallocation of X's block.
///   2 MGB  MG's beats after the lapse: the host re-registered on metadata
///          (its claim ours), the GRACE, the WERO re-acquire; its own data
///          re-registration, the wait (the metadata held and every metadata
///          registrant registered on data, else the grace), the WEAR
///          re-reservation, the reconcile (data report, metadata report, the
///          preempt of every data key the metadata namespace lacks).
///   3 JB   J's first beat after the lapse: metadata — unreserved and the
///          host unregistered ⇒ register (its page Live); data — unreserved,
///          the host unregistered and registered on metadata ⇒ register, the
///          re-check. The grace is the timing law that this beat runs first.
///   4 JW   J's data write, then its metadata write (refused ⇒ `LiveFenced`).
///   5 XB   X's first beat after the lapse (it resumes at any point): the same
///          steps under its currency (its page Live).
///   6 XW   X's data write: issued, admitted or refused, landed.
/// Invariants: `LiveFenced` a live member's write is never refused;
/// `ReallocatedUnfenced`, `StaleDataWrite` as in `Death` outside the accepted
/// windows (`LapseStrict`). `final`: `LapseUnbounded` both namespaces are
/// reserved again; `RecoveryStuck` X's region released.
pub fn Lapse(comptime cfg: LapseConfig) type {
    return extern struct {
        const Self = @This();
        pub const thread_count: usize = 7;
        const T_L = 0;
        const T_MGR = 1;
        const T_MGB = 2;
        const T_JB = 3;
        const T_JW = 4;
        const T_XB = 5;
        const T_XW = 6;
        const ROUNDS = 4;
        const R_P = 3;
        const R_END = 7;
        const MGB_END = 10;
        const BEAT_END = 7;

        m_res: bool = true,
        m_reg: [3]bool = @splat(true),
        d_res: bool = true,
        d_reg: [3]bool = @splat(true),
        lapsed: bool = false,
        x_page: Page = .live,
        fenced: bool = false,
        released: bool = false,
        realloc: bool = false,
        round: u8 = 0,
        /// J's first beat after the lapse ran (the grace's timing law).
        j_beat_done: bool = false,
        dsnap: [3]bool = @splat(false),
        msnap: [3]bool = @splat(false),
        xw: Wr = .idle,
        /// X's write was admitted while the namespace was unreserved, or
        /// inside its own beat's register window.
        xw_unreserved: bool = false,
        xw_window: bool = false,
        /// X's beat registered host B on data and has not post-checked.
        x_window: bool = false,
        pc: [7]u8 = @splat(0),
        live_fenced: bool = false,
        realloc_unfenced: bool = false,
        stale_write: bool = false,
        hit_lapse: bool = false,
        hit_beat_window: bool = false,

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                T_L => self.pc[t] >= 1,
                T_MGR => self.pc[t] >= R_END,
                T_MGB => self.pc[t] >= MGB_END,
                T_JB, T_XB => self.pc[t] >= BEAT_END,
                T_JW => self.pc[t] >= 2,
                T_XW => self.pc[t] >= 3,
                else => unreachable,
            };
        }

        fn admitsD(self: *const Self, h: u8) bool {
            return !self.d_res or self.d_reg[h];
        }

        fn admitsM(self: *const Self, h: u8) bool {
            return !self.m_res or self.m_reg[h];
        }

        fn current(self: *const Self, h: u8) bool {
            return h != B or !cfg.currency or self.x_page == .live;
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                T_L => {
                    // The namespace's disable waits for its in-flight
                    // requests before it drops the PR state (core.c
                    // `nvmet_ns_disable`: `disable_done`, then
                    // `nvmet_pr_exit_ns`); a power loss lands or loses them
                    // — never after the restart.
                    if (self.xw == .admitted) return;
                    self.d_res = false;
                    self.d_reg = @splat(false);
                    if (cfg.meta_too) {
                        self.m_res = false;
                        self.m_reg = @splat(false);
                    }
                    self.lapsed = true;
                    self.pc[t] = 1;
                },
                T_MGR => self.recovery(),
                T_MGB => self.managerBeat(),
                T_JB => self.memberBeat(T_JB, C),
                T_JW => self.liveWrites(),
                T_XB => self.memberBeat(T_XB, B),
                T_XW => self.deadWrite(),
                else => unreachable,
            }
        }

        fn recovery(self: *Self) void {
            switch (self.pc[T_MGR]) {
                0 => self.m_reg[B] = false,
                1 => self.x_page = .recovering,
                2 => self.m_reg[B] = false,
                // MG's own data client (registered: a preempt's sender
                // must be) preempts-and-aborts host B's key.
                R_P => if (self.d_reg[A] and self.d_reg[B]) {
                    if (self.xw == .admitted) return;
                    self.d_reg[B] = false;
                },
                4 => self.fenced = self.d_res and !self.d_reg[B],
                5 => {
                    if (self.fenced) {
                        self.released = true;
                        self.x_page = .released;
                    } else if (!self.d_res and !self.done(T_MGB)) {
                        // The lapse still open: the next poll.
                        return;
                    } else if (self.round + 1 < ROUNDS) {
                        self.round += 1;
                        self.pc[T_MGR] = R_P;
                        return;
                    } else {
                        self.pc[T_MGR] = R_END;
                        return;
                    }
                },
                6 => {
                    self.reallocCheck();
                    self.realloc = true;
                },
                else => unreachable,
            }
            self.pc[T_MGR] += 1;
        }

        /// J reallocates X's block: X must not be able to land there,
        /// outside the accepted windows.
        fn reallocCheck(self: *Self) void {
            switch (self.xw) {
                .admitted => {
                    if (self.xw_unreserved) {
                        self.hit_lapse = true;
                    } else if (self.xw_window) {
                        self.hit_beat_window = true;
                    } else self.realloc_unfenced = true;
                },
                .idle, .queued => if (self.admitsD(B)) {
                    if (!self.d_res) {
                        self.hit_lapse = true;
                    } else if (self.x_window) {
                        self.hit_beat_window = true;
                    } else self.realloc_unfenced = true;
                },
                .landed, .refused, .none => {},
            }
        }

        fn allMetaOnData(self: *const Self) bool {
            for (0..3) |k| if (self.m_reg[k] and !self.d_reg[k]) return false;
            return true;
        }

        fn managerBeat(self: *Self) void {
            switch (self.pc[T_MGB]) {
                0 => {
                    if (!self.lapsed) return;
                    if (!cfg.manager_beat) {
                        self.pc[T_MGB] = MGB_END;
                        return;
                    }
                },
                1 => if (cfg.meta_too and !self.m_reg[A]) {
                    self.m_reg[A] = true;
                },
                2 => if (cfg.meta_too and !self.m_res and cfg.grace and !self.j_beat_done) return,
                3 => if (cfg.meta_too and !self.m_res) {
                    self.m_res = true;
                },
                4 => if (!self.d_res and !self.d_reg[A] and self.m_reg[A]) {
                    self.d_reg[A] = true;
                },
                5 => if (!self.d_res and cfg.grace and !self.allMetaOnData() and !self.j_beat_done) return,
                6 => if (!self.d_res) {
                    self.d_res = true;
                },
                7 => self.dsnap = self.d_reg,
                8 => self.msnap = self.m_reg,
                9 => {
                    if (self.xw == .admitted and self.d_reg[B] and self.dsnap[B] and !self.msnap[B]) return;
                    for (0..3) |k| {
                        if (k != A and self.dsnap[k] and !self.msnap[k]) self.d_reg[k] = false;
                    }
                },
                else => unreachable,
            }
            self.pc[T_MGB] += 1;
        }

        /// A member's first beat after the lapse (host `h`).
        fn memberBeat(self: *Self, t: usize, h: u8) void {
            switch (self.pc[t]) {
                0 => {
                    if (!self.lapsed) return;
                    if (!cfg.member_beat) {
                        self.endBeat(t);
                        return;
                    }
                },
                1 => if (!(cfg.meta_too and !self.m_res and !self.m_reg[h] and self.current(h))) {
                    self.pc[t] = 4;
                    return;
                },
                2 => self.m_reg[h] = true,
                3 => if (!self.current(h)) {
                    self.m_reg[h] = false;
                },
                4 => if (!(!self.d_res and !self.d_reg[h] and (!cfg.meta_check or self.m_reg[h]) and self.current(h))) {
                    self.endBeat(t);
                    return;
                },
                5 => {
                    self.d_reg[h] = true;
                    if (h == B) self.x_window = true;
                },
                6 => {
                    if (h == B) self.x_window = false;
                    if ((cfg.meta_check and !self.m_reg[h]) or !self.current(h)) self.d_reg[h] = false;
                    self.endBeat(t);
                    return;
                },
                else => unreachable,
            }
            self.pc[t] += 1;
        }

        fn endBeat(self: *Self, t: usize) void {
            if (t == T_JB) self.j_beat_done = true;
            self.pc[t] = BEAT_END;
        }

        fn liveWrites(self: *Self) void {
            switch (self.pc[T_JW]) {
                0 => if (!self.admitsD(C)) {
                    self.live_fenced = true;
                },
                1 => if (!self.admitsM(C)) {
                    self.live_fenced = true;
                },
                else => unreachable,
            }
            self.pc[T_JW] += 1;
        }

        fn deadWrite(self: *Self) void {
            switch (self.pc[T_XW]) {
                0 => self.xw = .queued,
                1 => if (self.admitsD(B)) {
                    self.xw = .admitted;
                    self.xw_unreserved = !self.d_res;
                    // Admitted between its own beat's register and post-check:
                    // a withdrawal there unregisters without an abort.
                    self.xw_window = self.x_window;
                } else {
                    self.xw = .refused;
                    self.pc[T_XW] = 3;
                    return;
                },
                2 => {
                    self.xw = .landed;
                    if (self.realloc) {
                        if (self.xw_unreserved) {
                            self.hit_lapse = true;
                        } else if (self.xw_window) {
                            self.hit_beat_window = true;
                        } else self.stale_write = true;
                    }
                },
                else => unreachable,
            }
            self.pc[T_XW] += 1;
        }

        pub fn invariant(self: *const Self) !void {
            if (self.live_fenced) return error.LiveFenced;
            if (self.realloc_unfenced) return error.ReallocatedUnfenced;
            if (self.stale_write) return error.StaleDataWrite;
            if (cfg.strict.lapse and self.hit_lapse) return error.LapseWindow;
            if (cfg.strict.beat_window and self.hit_beat_window) return error.BeatWindow;
        }

        pub fn final(self: *const Self) !void {
            if (!self.d_res or (cfg.meta_too and !self.m_res)) return error.LapseUnbounded;
            if (!self.released) return error.RecoveryStuck;
        }
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

/// The invariant a failing schedule breaks first.
fn firstError(comptime M: type) anyerror {
    var report: explorer.Report = undefined;
    explorer.explore(M, testing.allocator, limits, &report) catch |explored| {
        if (explored != error.InvariantViolated and explored != error.FinalViolated) return explored;
        defer testing.allocator.free(report.schedule);
        var m = M.init();
        for (report.schedule) |t| {
            m.step(t);
            m.invariant() catch |e| return e;
        }
        m.final() catch |e| return e;
        return error.NoInvariantNamed;
    };
    return error.ModelHolds;
}

test "data fence model: a member's death on data namespaces as built — WEAR, every member host registered, the manager's own client preempts-and-aborts the dead host's key on every data namespace after the page went Recovering, the release only once every fence is proven (the next poll re-runs otherwise), a sibling's data registration through the rule with the data node's drain, its re-judge and the metadata re-check, the kept law at the last registrant — holds: no stale write lands on a reallocated block, no block reallocated while its dead writer can still land there, no Live member killed, the region released" {
    try explorer.expectHolds(Death(.{}), limits);
    const r = try statesOf(Death(.{}));
    std.debug.print("\ndata fence model (death): {d} states explored, depth {d}\n", .{ r.states, r.max_depth_seen });
}

test "data fence model: a co-located member's death as built — the own-key guard withholds the data preempt, the recoverer on the same host kills it by the rule and drains its queue before the release — holds" {
    try explorer.expectHolds(Death(.{ .colocated = true }), limits);
    const r = try statesOf(Death(.{ .colocated = true }));
    std.debug.print("\ndata fence model (death, co-located): {d} states explored, depth {d}\n", .{ r.states, r.max_depth_seen });
}

test "data fence model: a manager succession on data as built — the successor's admission preempts every data registrant volume 0's metadata namespace no longer registers (the data report first), a late admission withdrawn by its metadata re-check — holds: the superseded manager's in-place write never lands on a block the successor reallocated, through a chain of successors" {
    try explorer.expectHolds(Succession(.{}), limits);
    try explorer.expectHolds(Succession(.{ .late_admission = true }), limits);
    const r = try statesOf(Succession(.{ .late_admission = true }));
    std.debug.print("\ndata fence model (succession): {d} states explored, depth {d}\n", .{ r.states, r.max_depth_seen });
}

test "data fence model weakening: no data registration and no reservation (the code before this rung) — a dead member's resumed write lands on its reallocated block; a superseded manager's on the successor's" {
    try testing.expectEqual(error.ReallocatedUnfenced, firstError(Death(.{ .data_rtype = .none })));
    try explorer.expectFails(Succession(.{ .data_rtype = .none }), limits, error.InvariantViolated);
}

test "data fence model weakening: the blocks reallocated before the data fence (released at Recovering) — the dead member's write lands on the live member's block" {
    try testing.expectEqual(error.ReallocatedUnfenced, firstError(Death(.{ .order = .after_release })));
}

test "data fence model weakening: the holder's own death preempt on nvmet 7.3 semantics (WERO held by the manager, no data fencer) fences nothing — the release waits forever; proceeding on the unproven fence (the metadata posture) reallocates under the dead member's live registration" {
    try explorer.expectFails(Death(.{ .data_rtype = .wero_holder }), limits, error.FinalViolated);
    try testing.expectEqual(error.ReallocatedUnfenced, firstError(Death(.{ .data_rtype = .wero_holder, .proceed_unproven = true })));
}

test "data fence model weakening: the data fence before the page goes Recovering (no data re-fence) — a sibling's rule judges the dead member Live and re-admits its host on data before the release" {
    // The rule's register window (it judged the member Live) lets its write
    // land after the reallocation.
    try testing.expectEqual(error.StaleDataWrite, firstError(Death(.{ .order = .before_recovering })));
}

test "data fence model weakening: a plain preempt (no abort) — a write the target admitted before the preempt lands after the reallocation" {
    try testing.expectEqual(error.ReallocatedUnfenced, firstError(Death(.{ .abort = false })));
}

test "data fence model weakening: a sibling's data registration without the stale-sibling rule, or without the data node's drain after the kill, re-admits the dead member's writes" {
    try explorer.expectFails(Death(.{ .data_rule = false }), limits, error.InvariantViolated);
    try explorer.expectFails(Death(.{ .drain = false }), limits, error.InvariantViolated);
}

test "data fence model weakening: the last registrant unregisters although it preempted (no kept law) — the namespace unreserved, the dead member's write lands on the reallocated block" {
    try testing.expectEqual(error.StaleDataWrite, firstError(Death(.{ .keep_law = false })));
}

test "data fence model weakening: a co-located member's death under the own-key guard alone (no kill by its recoverer) — its write lands on its reallocated block" {
    try testing.expectEqual(error.ReallocatedUnfenced, firstError(Death(.{ .colocated = true, .colocated_kill = false })));
}

test "data fence model weakening: the successor fences only its own hold's victim on data (a chain of superseded managers keeps the first registered), or nothing; a late admission without its metadata re-check re-admits the superseded manager" {
    try explorer.expectFails(Succession(.{ .reconcile = .victim_only }), limits, error.InvariantViolated);
    try explorer.expectFails(Succession(.{ .reconcile = .none }), limits, error.InvariantViolated);
    try explorer.expectFails(Succession(.{ .late_admission = true, .meta_recheck = false }), limits, error.InvariantViolated);
}

test "data fence model: a member's death with a data namespace transiently absent at the manager's admission (review MAJOR 1) — step 2c holds while a non-retired set-config volume is neither fenced nor provably unfenceable, until the remedy tables it — holds" {
    try explorer.expectHolds(Death(.{ .transient_d1 = true }), limits);
    const r = try statesOf(Death(.{ .transient_d1 = true }));
    std.debug.print("\ndata fence model (death, transient-absent D1): {d} states explored, depth {d}\n", .{ r.states, r.max_depth_seen });
}

test "data fence model weakening (review MAJOR 1): step 2c fences only the manager's table — the transiently absent namespace skipped, the dead member's write lands on its reallocated block there" {
    try testing.expectEqual(error.ReallocatedUnfenced, firstError(Death(.{ .transient_d1 = true, .complete_check = false })));
}

test "data fence model: a manager succession with a joiner leaving between the successor's two reports (review minor 1) — the reconcile's preempt of the already-gone key re-reads the data report and proceeds — holds" {
    try explorer.expectHolds(Succession(.{ .leaver = true }), limits);
    try explorer.expectHolds(Succession(.{ .leaver = true, .late_admission = true }), limits);
    const r = try statesOf(Succession(.{ .leaver = true, .late_admission = true }));
    std.debug.print("\ndata fence model (succession, a leaver): {d} states explored, depth {d}\n", .{ r.states, r.max_depth_seen });
}

test "data fence model weakening (review minor 1): a reconcile preempt the target refuses (the key already gone) refuses the successor's admission" {
    try explorer.expectFails(Succession(.{ .leaver = true, .preempt_error = .refuse }), limits, error.FinalViolated);
    try testing.expectEqual(error.ReconcileRefused, firstError(Succession(.{ .leaver = true, .preempt_error = .refuse })));
}

test "data fence model: a PTPL-less target restart under a running set as built (review MAJOR 2) — every member re-registers at its beat through its currency and the metadata check, the manager re-acquires and re-reserves after the grace (or once every metadata registrant is on data) and reconciles; with the metadata namespace on the same target and without — holds: no live member's write refused, no stale write outside the accepted windows, both namespaces reserved again, the dead member's region released" {
    try explorer.expectHolds(Lapse(.{}), limits);
    try explorer.expectHolds(Lapse(.{ .meta_too = false }), limits);
    const r = try statesOf(Lapse(.{}));
    std.debug.print("\ndata fence model (lapse): {d} states explored, depth {d}\n", .{ r.states, r.max_depth_seen });
    const r2 = try statesOf(Lapse(.{ .meta_too = false }));
    std.debug.print("\ndata fence model (lapse, data only): {d} states explored, depth {d}\n", .{ r2.states, r2.max_depth_seen });
}

test "data fence model weakening (review MAJOR 2): no member beat (the code before) — the live member's write is refused once the manager re-reserves (fail-stopped); no manager beat — the namespaces stay unreserved (the lapse unbounded, the recovery stuck)" {
    try testing.expectEqual(error.LiveFenced, firstError(Lapse(.{ .member_beat = false })));
    try testing.expectEqual(error.LiveFenced, firstError(Lapse(.{ .member_beat = false, .meta_too = false })));
    try testing.expectEqual(error.LapseUnbounded, firstError(Lapse(.{ .manager_beat = false })));
    try testing.expectEqual(error.LapseUnbounded, firstError(Lapse(.{ .manager_beat = false, .meta_too = false })));
}

test "data fence model weakening (review MAJOR 2): the re-acquire and the re-reservation before the grace — the live member, not yet re-registered, is refused" {
    try testing.expectEqual(error.LiveFenced, firstError(Lapse(.{ .grace = false })));
    try testing.expectEqual(error.LiveFenced, firstError(Lapse(.{ .grace = false, .meta_too = false })));
}

test "data fence model weakening (review MAJOR 2): a lapse re-registration without the member's currency re-admits the dead member's host on metadata and data — its write lands on its reallocated block" {
    try testing.expectEqual(error.StaleDataWrite, firstError(Lapse(.{ .currency = false })));
    try explorer.expectFails(Lapse(.{ .currency = false, .meta_check = false, .meta_too = false }), limits, error.InvariantViolated);
}

test "data fence model, the lapse's accepted windows are reachable: a write admitted while the namespace is unreserved after the restart; a member's own re-registration admitting its write before its post-check withdraws it" {
    try testing.expectEqual(error.LapseWindow, firstError(Lapse(.{ .strict = .{ .lapse = true } })));
    try testing.expectEqual(error.BeatWindow, firstError(Lapse(.{ .strict = .{ .beat_window = true } })));
}
