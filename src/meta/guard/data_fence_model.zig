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
    /// The same window of a member's re-admission after a refused data write
    /// (decision 0003 §DL): another of its writes admitted between that
    /// register and its post-check.
    ebade_window: bool = false,
};

/// When the manager's metadata grace may end before its length.
pub const MetaEarly = enum {
    /// Never: the hold returns after the grace (the code before §DL).
    grace_only,
    /// Once every live member's host key in the census is registered on the
    /// metadata namespace (as built, decision 0003 §DL).
    census,
    /// Once every key the metadata namespace's own report lists is there —
    /// trivially at once: the report knows only who came back.
    registrants,
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
    /// The metadata grace's early end.
    meta_early: MetaEarly = .census,
    /// J's beat is slow on DATA: the grace's timing law covers its metadata
    /// re-registration only, its data re-registration may come after the
    /// re-reservation (its metadata part missing the grace stays the
    /// documented fail-stop, §DJ.12(a)).
    slow_member: bool = false,
    /// A member whose data write the target refuses re-admits its host
    /// (through the rule) and retries the write once when it saw a lapse,
    /// is registered on the metadata namespace and is current, before and
    /// after the register (as built, §DL); false: every refusal latches the
    /// fence (the code before).
    ebade_readmit: bool = true,
    /// ... with those metadata and currency checks (false: the lapse alone).
    ebade_checks: bool = true,
    /// X's second write in flight beside the first (the re-admission's
    /// window needs it; the slow-member runs leave it out to stay inside the
    /// explorer's bound).
    x_second_write: bool = true,
    /// A member's data re-registration requires its host registered on the
    /// metadata namespace (before and after).
    meta_check: bool = true,
    /// A member re-registers only while its page is Live under its own
    /// writer id (before and after).
    currency: bool = true,
    /// Decision 0003 §DN: a second metadata volume M1 — the manager holds
    /// both (one guard each), every member joined both. M1's target lapses
    /// with `m1_lapse`; the data namespace with `d_lapse` (with `meta_too`
    /// volume 0's target carries it).
    meta_volumes: u8 = 1,
    m1_lapse: bool = false,
    d_lapse: bool = true,
    /// The volume whose recovery of X runs first — its metadata preempt, its
    /// page Recovering, the data fence, the release and the reallocation —
    /// the other volume's metadata steps after it (volume 1 first: the
    /// order a volume-0 judgement cannot see).
    recovery_first: u1 = 0,
    /// A member's lapse beat re-registers its host on every joined metadata
    /// volume (as built, §DN); `.volume0`: volume 0's only (the code before).
    beat_scope: Scope = .every_volume,
    /// A member's currency — and the metadata registration its data checks
    /// read — over every joined volume (as built, §DN); `.volume0`: volume
    /// 0's page and registration only (the code before).
    currency_scope: Scope = .every_volume,
    /// Whose registrations end volume 1's grace early: its own namespace's
    /// (as built — each guard reads its own report); `.volume0`: volume 0's.
    early_from: EarlyFrom = .own_volume,
    strict: LapseStrict = .{},
};

/// The namespace a guard's early end reads (§DN).
pub const EarlyFrom = enum { own_volume, volume0 };

/// Which joined metadata volumes a member's lapse checks reach (§DN).
pub const Scope = enum { every_volume, volume0 };

/// `Lapse` — a PTPL-less target restart (nvmet keeps no PTPL: every namespace
/// enable starts with no registrant and no reservation, pr.c:1136) under a
/// running set: the manager MG (host A) holds the metadata namespace (WERO)
/// and every host is registered on it and on the WEAR data namespace; J (host
/// C) a live joiner writing data and metadata; X (host B) a member frozen
/// with two data writes, its death recovered by MG at any point (its census
/// row gone at its eviction, before the recovery):
///   0 L    the restart: every registration and both reservations forgotten
///          (`meta_too`: the metadata namespace on the same target), once the
///          namespace's in-flight requests completed.
///   1 MGR  X's recovery: the metadata preempt of host B, the page
///          Recovering, the re-fence, the data preempt-and-abort (MG's own
///          client, registered), the proof (reserved WEAR, the key absent);
///          unproven while the lapse is open, the next poll; then the release
///          and J's reallocation of X's block.
///   2 MGB  MG's beats after the lapse: the host re-registered on metadata
///          (its claim ours), the GRACE — ended early once every census key
///          is registered on metadata (`meta_early`) —, the WERO re-acquire;
///          its own data re-registration, the wait (the metadata held and
///          every metadata registrant registered on data, else the grace),
///          the WEAR re-reservation, the reconcile (data report, metadata
///          report, the preempt of every data key the metadata namespace
///          lacks).
///   3 JB   J's first beat after the lapse: metadata — unreserved and the
///          host unregistered ⇒ register (its page Live); data — unreserved,
///          the host unregistered and registered on metadata ⇒ register, the
///          re-check. The grace is the timing law that this beat (with
///          `slow_member`, its metadata part) runs first.
///   4 JW   J's data write — refused ⇒ the re-admission (`ebade_readmit`):
///          its lapse seen, its metadata registration and currency, the
///          register, the post-check, the retry — then its metadata write
///          (any refusal left ⇒ `LiveFenced`).
///   5 XB   X's first beat after the lapse (it resumes at any point): the same
///          steps under its currency (its page Live).
///   6 XW   X's data write: issued, admitted or refused, landed; refused ⇒
///          the same re-admission and one retry.
///   7 XW2  X's second data write, in flight beside the first.
///   8 MGB1 (`meta_volumes` 2, decision 0003 §DN) volume 1's guard after a
///          lapse of M1: the host re-registered, the grace — ended early once
///          every census key is registered on M1 —, the WERO re-acquire.
/// With two metadata volumes a member's beat re-registers on M0, then on M1
/// (`beat_scope`), then on data; its currency is its page Live on every
/// joined volume, and its data checks read every joined volume's metadata
/// registration (`currency_scope`); J writes metadata on both volumes; X's
/// recovery runs `recovery_first`'s volume whole (its data fence, the
/// release, the reallocation), then the other volume's preempt, page and
/// re-fence. The restart takes the targets `meta_too`/`d_lapse` (volume 0's)
/// and `m1_lapse` (volume 1's) in one step.
/// Invariants: `LiveFenced` a live member's write is never refused;
/// `ReallocatedUnfenced`, `StaleDataWrite` as in `Death` outside the accepted
/// windows (`LapseStrict`). `final`: `LapseUnbounded` every namespace is
/// reserved again; `RecoveryStuck` X's region released.
pub fn Lapse(comptime cfg: LapseConfig) type {
    return extern struct {
        const Self = @This();
        pub const thread_count: usize = 9;
        const T_L = 0;
        const T_MGR = 1;
        const T_MGB = 2;
        const T_JB = 3;
        const T_JW = 4;
        const T_XB = 5;
        const T_XW = 6;
        const T_XW2 = 7;
        const T_MGB1 = 8;
        const TWO = cfg.meta_volumes == 2;
        const FIRST: u1 = cfg.recovery_first;
        const SECOND: u1 = 1 - cfg.recovery_first;
        const ROUNDS = 4;
        const R_P = 3;
        // Volume `SECOND`'s preempt, page and re-fence after the reallocation.
        const R_SECOND = 7;
        const R_END = if (TWO) 10 else 7;
        const MGB_END = 10;
        const MGB1_END = 3;
        // A member's beat: M0 (pre-check, register, post-check), M1 (the
        // same, two volumes only), data (pre-check, register, post-check).
        const M1_PRE = 4;
        const D_PRE = if (TWO) 7 else 4;
        const BEAT_END = D_PRE + 3;
        // J's writes: the data write, the re-admission (pre-check, register,
        // post-check, retry), the metadata write (and M1's).
        const JW_PRE = 1;
        const JW_META = 5;
        const JW_END = if (TWO) 7 else 6;
        // X's write: issue, arrival, landing; the re-admission (pre-check,
        // register, post-check), the retry's issue, arrival, landing.
        const XW_LAND = 2;
        const XW_PRE = 3;
        const XW_END = 9;

        comptime {
            if (cfg.meta_volumes != 1 and cfg.meta_volumes != 2) @compileError("Lapse models one or two metadata volumes");
            if (!TWO and (cfg.m1_lapse or cfg.recovery_first != 0)) @compileError("M1 needs meta_volumes 2");
        }

        /// Per metadata volume: reserved (WERO by MG), the hosts registered.
        m_res: [2]bool = @splat(true),
        m_reg: [2][3]bool = @splat(@splat(true)),
        d_res: bool = true,
        d_reg: [3]bool = @splat(true),
        lapsed: bool = false,
        /// X's page on each metadata volume.
        x_page: [2]Page = @splat(.live),
        /// X's census row (gone at its eviction, which precedes the
        /// recovery).
        x_listed: bool = true,
        fenced: bool = false,
        released: bool = false,
        realloc: bool = false,
        round: u8 = 0,
        /// A member's beat saw the lapse (its re-admission's lapse word).
        seen: [3]bool = @splat(false),
        /// J's first beat after the lapse ran (the grace's timing law), and
        /// its metadata part (`slow_member`'s).
        j_beat_done: bool = false,
        j_meta_done: bool = false,
        dsnap: [3]bool = @splat(false),
        msnap: [3]bool = @splat(false),
        xw: Wr = .idle,
        xw2: Wr = if (cfg.x_second_write) .idle else .none,
        /// Where an X write was admitted: while the namespace was
        /// unreserved, inside its own beat's register window, inside its
        /// re-admission's.
        xw_unreserved: bool = false,
        xw_window: bool = false,
        xw_ewindow: bool = false,
        xw2_unreserved: bool = false,
        xw2_window: bool = false,
        xw2_ewindow: bool = false,
        /// X's beat registered host B on data and has not post-checked.
        x_window: bool = false,
        /// X's re-admission registered host B on data (checked before) and
        /// has not post-checked.
        x_ewindow: bool = false,
        /// A re-admission's register found the host unregistered (its
        /// withdrawal's word).
        x_fresh: bool = false,
        j_fresh: bool = false,
        /// A beat's data register found its host unregistered.
        beat_fresh: [3]bool = @splat(false),
        pc: [9]u8 = @splat(0),
        live_fenced: bool = false,
        realloc_unfenced: bool = false,
        stale_write: bool = false,
        hit_lapse: bool = false,
        hit_beat_window: bool = false,
        hit_ebade_window: bool = false,

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                T_L => self.pc[t] >= 1,
                T_MGR => self.pc[t] >= R_END,
                T_MGB => self.pc[t] >= MGB_END,
                T_JB, T_XB => self.pc[t] >= BEAT_END,
                T_JW => self.pc[t] >= JW_END,
                T_XW => self.pc[t] >= XW_END,
                T_XW2 => self.pc[t] >= 3,
                T_MGB1 => !TWO or self.pc[t] >= MGB1_END,
                else => unreachable,
            };
        }

        fn admitsD(self: *const Self, h: u8) bool {
            return !self.d_res or self.d_reg[h];
        }

        fn admitsM(self: *const Self, v: u1, h: u8) bool {
            return !self.m_res[v] or self.m_reg[v][h];
        }

        /// A member's currency: its page Live on every joined volume
        /// (`currency_scope` `.volume0`: volume 0's alone).
        fn current(self: *const Self, h: u8) bool {
            if (h != B or !cfg.currency) return true;
            if (self.x_page[0] != .live) return false;
            return !TWO or cfg.currency_scope == .volume0 or self.x_page[1] == .live;
        }

        /// The metadata registration a member's data checks read: every
        /// joined volume's (`currency_scope` `.volume0`: volume 0's alone).
        fn metaRegistered(self: *const Self, h: u8) bool {
            if (!self.m_reg[0][h]) return false;
            return !TWO or cfg.currency_scope == .volume0 or self.m_reg[1][h];
        }

        /// The grace's timing law: J's beat (with `slow_member`, its
        /// metadata part) ran.
        fn graceOver(self: *const Self) bool {
            return if (cfg.slow_member) self.j_meta_done else self.j_beat_done;
        }

        /// Volume `v`'s metadata grace's early end: every key the census
        /// lists is registered on that volume's namespace.
        fn metaEarlyEnd(self: *const Self, v_in: u1) bool {
            const v: u1 = if (cfg.early_from == .volume0) 0 else v_in;
            return switch (cfg.meta_early) {
                .grace_only => false,
                .census => self.m_reg[v][C] and (!self.x_listed or self.m_reg[v][B]),
                .registrants => true,
            };
        }

        /// A refused member's re-admission may register: its lapse seen,
        /// and (`ebade_checks`) its host registered on metadata and current.
        fn readmitPasses(self: *const Self, h: u8) bool {
            if (!self.seen[h]) return false;
            return !cfg.ebade_checks or (self.metaRegistered(h) and self.current(h));
        }

        fn anyXAdmitted(self: *const Self) bool {
            return self.xw == .admitted or self.xw2 == .admitted;
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                T_L => {
                    // The namespace's disable waits for its in-flight
                    // requests before it drops the PR state (core.c
                    // `nvmet_ns_disable`: `disable_done`, then
                    // `nvmet_pr_exit_ns`); a power loss lands or loses them
                    // — never after the restart.
                    if (cfg.d_lapse and self.anyXAdmitted()) return;
                    if (cfg.d_lapse) {
                        self.d_res = false;
                        self.d_reg = @splat(false);
                    }
                    if (cfg.meta_too) {
                        self.m_res[0] = false;
                        self.m_reg[0] = @splat(false);
                    }
                    if (cfg.m1_lapse) {
                        self.m_res[1] = false;
                        self.m_reg[1] = @splat(false);
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
                T_XW2 => self.deadWrite2(),
                T_MGB1 => self.volume1Guard(),
                else => unreachable,
            }
        }

        fn recovery(self: *Self) void {
            switch (self.pc[T_MGR]) {
                0 => {
                    // The eviction (its census row gone), then the metadata
                    // preempt of host B on the first volume recovered.
                    self.x_listed = false;
                    self.m_reg[FIRST][B] = false;
                },
                1 => self.x_page[FIRST] = .recovering,
                2 => self.m_reg[FIRST][B] = false,
                // MG's own data client (registered: a preempt's sender
                // must be) preempts-and-aborts host B's key.
                R_P => if (self.d_reg[A] and self.d_reg[B]) {
                    if (self.anyXAdmitted()) return;
                    self.d_reg[B] = false;
                },
                4 => self.fenced = self.d_res and !self.d_reg[B],
                5 => {
                    if (self.fenced) {
                        self.released = true;
                        self.x_page[FIRST] = .released;
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
                // The other volume's recovery of X: its preempt, its page
                // Recovering, its re-fence (its data fence a repeat).
                R_SECOND => self.m_reg[SECOND][B] = false,
                R_SECOND + 1 => self.x_page[SECOND] = .recovering,
                R_SECOND + 2 => self.m_reg[SECOND][B] = false,
                else => unreachable,
            }
            self.pc[T_MGR] += 1;
        }

        /// Where X's write `w` admitted now (or already) would count: the
        /// lapse, its beat's window, its re-admission's, or a breach.
        fn classify(self: *Self, unreserved: bool, window: bool, ewindow: bool) void {
            if (unreserved) {
                self.hit_lapse = true;
            } else if (window) {
                self.hit_beat_window = true;
            } else if (ewindow) {
                self.hit_ebade_window = true;
            } else self.realloc_unfenced = true;
        }

        /// J reallocates X's block: X must not be able to land there,
        /// outside the accepted windows.
        fn reallocCheck(self: *Self) void {
            const ws = [2]struct { w: Wr, u: bool, bw: bool, ew: bool, alive: bool }{
                .{ .w = self.xw, .u = self.xw_unreserved, .bw = self.xw_window, .ew = self.xw_ewindow, .alive = true },
                .{ .w = self.xw2, .u = self.xw2_unreserved, .bw = self.xw2_window, .ew = self.xw2_ewindow, .alive = true },
            };
            for (ws) |x| switch (x.w) {
                .admitted => self.classify(x.u, x.bw, x.ew),
                .idle, .queued => if (self.admitsD(B)) self.classify(!self.d_res, self.x_window, self.x_ewindow),
                .landed, .refused, .none => {},
            };
        }

        /// Every host registered on volume 0's metadata namespace (the data
        /// authority's) is registered on data.
        fn allMetaOnData(self: *const Self) bool {
            for (0..3) |k| if (self.m_reg[0][k] and !self.d_reg[k]) return false;
            return true;
        }

        /// Volume 0's guard and the data beat (MG's process); neither sees a
        /// lapse when only volume 1's target restarted.
        fn managerBeat(self: *Self) void {
            switch (self.pc[T_MGB]) {
                0 => {
                    if (!self.lapsed) return;
                    if (!cfg.manager_beat or !(cfg.meta_too or cfg.d_lapse)) {
                        self.pc[T_MGB] = MGB_END;
                        return;
                    }
                },
                1 => if (!self.m_res[0] and !self.m_reg[0][A]) {
                    self.m_reg[0][A] = true;
                },
                2 => if (!self.m_res[0] and cfg.grace and !self.graceOver() and !self.metaEarlyEnd(0)) return,
                3 => if (!self.m_res[0]) {
                    self.m_res[0] = true;
                },
                4 => if (!self.d_res and !self.d_reg[A] and self.m_reg[0][A]) {
                    self.d_reg[A] = true;
                },
                5 => if (!self.d_res and cfg.grace and !self.allMetaOnData() and !self.graceOver()) return,
                6 => if (!self.d_res) {
                    self.d_res = true;
                },
                7 => self.dsnap = self.d_reg,
                8 => self.msnap = self.m_reg[0],
                9 => {
                    if (self.anyXAdmitted() and self.d_reg[B] and self.dsnap[B] and !self.msnap[B]) return;
                    for (0..3) |k| {
                        if (k != A and self.dsnap[k] and !self.msnap[k]) self.d_reg[k] = false;
                    }
                },
                else => unreachable,
            }
            self.pc[T_MGB] += 1;
        }

        /// Volume 1's guard (MG's process, its own heartbeat): no lapse
        /// unless volume 1's target restarted.
        fn volume1Guard(self: *Self) void {
            switch (self.pc[T_MGB1]) {
                0 => {
                    if (!self.lapsed) return;
                    if (!cfg.manager_beat or !cfg.m1_lapse) {
                        self.pc[T_MGB1] = MGB1_END;
                        return;
                    }
                },
                // (Host A's own re-registration on M1 is not modelled:
                // nothing here reads it.)
                1 => if (!self.m_res[1] and cfg.grace and !self.graceOver() and !self.metaEarlyEnd(1)) return,
                2 => if (!self.m_res[1]) {
                    self.m_res[1] = true;
                },
                else => unreachable,
            }
            self.pc[T_MGB1] += 1;
        }

        /// A member's first beat after the lapse (host `h`): every joined
        /// metadata volume, then the data namespace.
        fn memberBeat(self: *Self, t: usize, h: u8) void {
            const pc = self.pc[t];
            if (pc == 0) {
                if (!self.lapsed) return;
                if (!cfg.member_beat) return self.endBeat(t);
                // The data side's lapse word: volume 0's target or the data
                // namespace's restarted (an M1 lapse is not one).
                if (cfg.meta_too or cfg.d_lapse) self.seen[h] = true;
            } else if (pc == 1) {
                if (!(!self.m_res[0] and !self.m_reg[0][h] and self.current(h))) {
                    self.pc[t] = M1_PRE;
                    return;
                }
            } else if (pc == 2) {
                self.m_reg[0][h] = true;
                // J is current and registered throughout: with two volumes
                // its post-checks (no-ops) are skipped.
                if (TWO and h == C) {
                    self.pc[t] = M1_PRE;
                    return;
                }
            } else if (pc == 3) {
                if (!self.current(h)) self.m_reg[0][h] = false;
            } else if (TWO and pc == M1_PRE) {
                if (cfg.beat_scope == .volume0 or !(!self.m_res[1] and !self.m_reg[1][h] and self.current(h))) {
                    self.pc[t] = D_PRE;
                    return;
                }
            } else if (TWO and pc == M1_PRE + 1) {
                self.m_reg[1][h] = true;
                if (h == C) {
                    self.pc[t] = D_PRE;
                    return;
                }
            } else if (TWO and pc == M1_PRE + 2) {
                if (!self.current(h)) self.m_reg[1][h] = false;
            } else if (pc == D_PRE) {
                if (h == C) self.j_meta_done = true;
                if (!(!self.d_res and !self.d_reg[h] and (!cfg.meta_check or self.metaRegistered(h)) and self.current(h))) return self.endBeat(t);
            } else if (pc == D_PRE + 1) {
                // A registration that stands meanwhile (its own
                // re-admission's) is adopted: the post-check withdraws only a
                // fresh one.
                self.beat_fresh[h] = !self.d_reg[h];
                self.d_reg[h] = true;
                if (h == B and self.beat_fresh[h]) self.x_window = true;
                if (TWO and h == C) return self.endBeat(t);
            } else if (pc == D_PRE + 2) {
                if (h == B) self.x_window = false;
                if (self.beat_fresh[h] and ((cfg.meta_check and !self.metaRegistered(h)) or !self.current(h))) self.d_reg[h] = false;
                return self.endBeat(t);
            } else unreachable;
            self.pc[t] += 1;
        }

        fn endBeat(self: *Self, t: usize) void {
            if (t == T_JB) self.j_beat_done = true;
            self.pc[t] = BEAT_END;
        }

        fn liveWrites(self: *Self) void {
            switch (self.pc[T_JW]) {
                0 => if (!self.admitsD(C)) {
                    if (cfg.ebade_readmit) {
                        self.pc[T_JW] = JW_PRE;
                        return;
                    }
                    self.live_fenced = true;
                    self.pc[T_JW] = JW_META;
                    return;
                } else {
                    self.pc[T_JW] = JW_META;
                    return;
                },
                JW_PRE => if (!self.readmitPasses(C)) {
                    self.live_fenced = true;
                    self.pc[T_JW] = JW_META;
                    return;
                },
                2 => {
                    self.j_fresh = !self.d_reg[C];
                    self.d_reg[C] = true;
                },
                3 => if (cfg.ebade_checks and !(self.metaRegistered(C) and self.current(C))) {
                    if (self.j_fresh) self.d_reg[C] = false;
                    self.live_fenced = true;
                    self.pc[T_JW] = JW_META;
                    return;
                },
                4 => if (!self.admitsD(C)) {
                    self.live_fenced = true;
                },
                JW_META => if (!self.admitsM(0, C)) {
                    self.live_fenced = true;
                },
                // Its metadata write on volume 1 (two volumes only).
                JW_META + 1 => if (!self.admitsM(1, C)) {
                    self.live_fenced = true;
                },
                else => unreachable,
            }
            self.pc[T_JW] += 1;
        }

        /// A frozen X write's arrival at the target: admitted (with where)
        /// or refused.
        fn arrive(self: *Self, w: *Wr, unreserved: *bool, window: *bool, ewindow: *bool) bool {
            if (!self.admitsD(B)) {
                w.* = .refused;
                return false;
            }
            w.* = .admitted;
            unreserved.* = !self.d_res;
            // Admitted between a register of its own process and that
            // register's post-check: a withdrawal there unregisters without
            // an abort.
            window.* = self.x_window;
            ewindow.* = self.x_ewindow;
            return true;
        }

        fn land(self: *Self, w: *Wr, unreserved: bool, window: bool, ewindow: bool) void {
            w.* = .landed;
            if (self.realloc) {
                if (unreserved) {
                    self.hit_lapse = true;
                } else if (window) {
                    self.hit_beat_window = true;
                } else if (ewindow) {
                    self.hit_ebade_window = true;
                } else self.stale_write = true;
            }
        }

        fn deadWrite(self: *Self) void {
            switch (self.pc[T_XW]) {
                0, 6 => self.xw = .queued,
                1, 7 => if (!self.arrive(&self.xw, &self.xw_unreserved, &self.xw_window, &self.xw_ewindow)) {
                    // A refusal: the re-admission once, then the fence.
                    self.pc[T_XW] = if (cfg.ebade_readmit and self.pc[T_XW] == 1) XW_PRE else XW_END;
                    return;
                },
                XW_LAND, 8 => {
                    self.land(&self.xw, self.xw_unreserved, self.xw_window, self.xw_ewindow);
                    self.pc[T_XW] = XW_END;
                    return;
                },
                XW_PRE => if (!self.readmitPasses(B)) {
                    self.pc[T_XW] = XW_END;
                    return;
                },
                4 => {
                    // A registration standing already is adopted: the
                    // post-check withdraws only its own.
                    self.x_fresh = !self.d_reg[B];
                    self.d_reg[B] = true;
                    // Checked before: the accepted window until the
                    // post-check (unchecked, a breach).
                    if (cfg.ebade_checks and self.x_fresh) self.x_ewindow = true;
                },
                5 => {
                    self.x_ewindow = false;
                    if (cfg.ebade_checks and !(self.metaRegistered(B) and self.current(B))) {
                        if (self.x_fresh) self.d_reg[B] = false;
                        self.pc[T_XW] = XW_END;
                        return;
                    }
                },
                else => unreachable,
            }
            self.pc[T_XW] += 1;
        }

        fn deadWrite2(self: *Self) void {
            switch (self.pc[T_XW2]) {
                0 => if (!cfg.x_second_write) {
                    self.pc[T_XW2] = 3;
                    return;
                } else {
                    self.xw2 = .queued;
                },
                1 => if (!self.arrive(&self.xw2, &self.xw2_unreserved, &self.xw2_window, &self.xw2_ewindow)) {
                    self.pc[T_XW2] = 3;
                    return;
                },
                2 => self.land(&self.xw2, self.xw2_unreserved, self.xw2_window, self.xw2_ewindow),
                else => unreachable,
            }
            self.pc[T_XW2] += 1;
        }

        pub fn invariant(self: *const Self) !void {
            if (self.live_fenced) return error.LiveFenced;
            if (self.realloc_unfenced) return error.ReallocatedUnfenced;
            if (self.stale_write) return error.StaleDataWrite;
            if (cfg.strict.lapse and self.hit_lapse) return error.LapseWindow;
            if (cfg.strict.beat_window and self.hit_beat_window) return error.BeatWindow;
            if (cfg.strict.ebade_window and self.hit_ebade_window) return error.EbadeWindow;
        }

        pub fn final(self: *const Self) !void {
            if (!self.d_res or !self.m_res[0] or !self.m_res[1]) return error.LapseUnbounded;
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

test "data fence model (decision 0003 §DL): the lapse as built with the metadata grace's early end — the manager re-acquires the metadata hold as soon as every live member's host key in the census is registered there (else after the grace) and re-reserves the data namespace at once when every metadata registrant is back on data — and a slow member (its data re-registration after the re-reservation) re-admitted at its refused write through its lapse, its metadata registration and its currency, before and after the register, its write retried — holds; the code before the early end holds too (the early end is liveness)" {
    try explorer.expectHolds(Lapse(.{ .slow_member = true, .x_second_write = false }), limits);
    try explorer.expectHolds(Lapse(.{ .slow_member = true, .x_second_write = false, .meta_too = false }), limits);
    try explorer.expectHolds(Lapse(.{ .meta_early = .grace_only }), limits);
    const r = try statesOf(Lapse(.{ .slow_member = true, .x_second_write = false }));
    std.debug.print("\ndata fence model (lapse, slow member): {d} states explored, depth {d}\n", .{ r.states, r.max_depth_seen });
    const r2 = try statesOf(Lapse(.{ .slow_member = true, .x_second_write = false, .meta_too = false }));
    std.debug.print("\ndata fence model (lapse, slow member, data only): {d} states explored, depth {d}\n", .{ r2.states, r2.max_depth_seen });
}

test "data fence model weakening (decision 0003 §DL): the metadata grace ended by the metadata report's own registrants (no census) — the hold returns before the live member re-registered, its metadata write refused" {
    try testing.expectEqual(error.LiveFenced, firstError(Lapse(.{ .meta_early = .registrants })));
}

test "data fence model weakening (decision 0003 §DL): a slow member whose refused data write latches the fence (the code before) is fail-stopped although it is live and current" {
    try testing.expectEqual(error.LiveFenced, firstError(Lapse(.{ .slow_member = true, .x_second_write = false, .ebade_readmit = false })));
    try testing.expectEqual(error.LiveFenced, firstError(Lapse(.{ .slow_member = true, .x_second_write = false, .ebade_readmit = false, .meta_too = false })));
}

test "data fence model weakening (decision 0003 §DL): the re-admission at a refused data write without its metadata and currency checks re-admits the dead member's host — its retried write lands on its reallocated block" {
    try testing.expectEqual(error.StaleDataWrite, firstError(Lapse(.{ .ebade_checks = false })));
    try explorer.expectFails(Lapse(.{ .ebade_checks = false, .meta_too = false }), limits, error.InvariantViolated);
}

test "data fence model, the re-admission's accepted window is reachable (decision 0003 §DL): a member frozen between its re-admission's checks and its register across its whole death admits another in-flight write before its post-check withdraws the registration — the beat window's class" {
    try testing.expectEqual(error.EbadeWindow, firstError(Lapse(.{ .strict = .{ .ebade_window = true } })));
}

/// Which targets of a two-metadata-volume set restart (decision 0003 §DN):
/// volume 1's alone, both (volume 0's carries the data namespace), volume
/// 0's alone.
const MvRestart = enum { v1_only, both, v0_only };

fn twoVolumes(comptime r: MvRestart, comptime first: u1) LapseConfig {
    var c: LapseConfig = .{ .meta_volumes = 2, .recovery_first = first, .x_second_write = false };
    switch (r) {
        .v1_only => {
            c.meta_too = false;
            c.d_lapse = false;
            c.m1_lapse = true;
        },
        .both => c.m1_lapse = true,
        .v0_only => {},
    }
    return c;
}

fn mvWeakened(comptime r: MvRestart, comptime first: u1, comptime beat: Scope, comptime currency: Scope) LapseConfig {
    var c = twoVolumes(r, first);
    c.beat_scope = beat;
    c.currency_scope = currency;
    return c;
}

test "data fence model (decision 0003 §DN): a two-metadata-volume set as built — every member's lapse beat re-registers its host on EVERY joined metadata volume (its currency: its page Live on every joined volume), each volume's guard re-acquires once every census key is registered on ITS namespace (else after the grace), a member's data checks read every joined volume's registration and page; volume 1's target restarted alone, both targets, volume 0's alone; X recovered on volume 0 first and on volume 1 first — holds: no live member's write refused on either volume, no stale data write, every namespace reserved again, X's region released" {
    inline for (.{ MvRestart.v1_only, MvRestart.both, MvRestart.v0_only }) |r| {
        inline for (.{ 0, 1 }) |first| {
            const M = Lapse(twoVolumes(r, first));
            try explorer.expectHolds(M, limits);
            const rep = try statesOf(M);
            std.debug.print("\ndata fence model (lapse, two metadata volumes, restart {t}, volume {d} recovered first): {d} states explored, depth {d}\n", .{ r, first, rep.states, rep.max_depth_seen });
        }
    }
}

test "data fence model weakening (decision 0003 §DN): a member's lapse beat on volume 0's metadata namespace only (the code before) — after a restart of volume 1's target (alone or with volume 0's) volume 1's guard never sees the live member back, takes its hold after the grace and refuses the member's next metadata write there" {
    try testing.expectEqual(error.LiveFenced, firstError(Lapse(mvWeakened(.v1_only, 0, .volume0, .every_volume))));
    try testing.expectEqual(error.LiveFenced, firstError(Lapse(mvWeakened(.both, 0, .volume0, .every_volume))));
}

test "data fence model weakening (decision 0003 §DN): a member's currency and its data checks judged on volume 0 alone — X's death recovered on volume 1 first (its data fence proven, its block reallocated) while volume 0 still reads it Live and registered; a restart of both targets lets X re-register on volume 0 and on data, the reconcile keeps it (its key on volume 0's namespace), its write lands on its reallocated block" {
    try testing.expectEqual(error.StaleDataWrite, firstError(Lapse(mvWeakened(.both, 1, .every_volume, .volume0))));
}

test "data fence model weakening (decision 0003 §DN): volume 1's grace ended by volume 0's registrations — a restart of both targets; the live member back on volume 0 but not yet on volume 1, volume 1's guard takes its hold and refuses its metadata write there" {
    const c = comptime blk: {
        var c = twoVolumes(.both, 0);
        c.early_from = .volume0;
        break :blk c;
    };
    try testing.expectEqual(error.LiveFenced, firstError(Lapse(c)));
}
