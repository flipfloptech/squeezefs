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
        pub const thread_count: usize = if (cfg.colocated) 6 else 5;
        const SEQ = mgSequence(cfg);
        const P0 = indexOf(SEQ, .p0);
        // The next poll re-runs: one round per registration a sibling can
        // slip between a preempt and its proof (two per namespace), plus one.
        const ROUNDS = 5;
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
        pc: [6]u8 = @splat(0),
        stale_write: bool = false,
        realloc_unfenced: bool = false,
        killed_current: bool = false,

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
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
        /// admitted there (`abort`).
        fn preempt(self: *Self, i: usize) bool {
            if (cfg.data_rtype == .none) return true;
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
            self.fenced[i] = cfg.data_rtype == .none or (self.reserved[i] and !self.reg_b[i]);
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
                    const proven = self.fenced[0] and self.fenced[1];
                    if (cfg.order == .after_release or proven or cfg.proceed_unproven) {
                        self.released = true;
                        self.x_page = .released;
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
};

const NONE: u8 = 3;

pub fn Succession(comptime cfg: SuccessionConfig) type {
    return extern struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        /// M's steps: the admission (check, register, re-check), then the
        /// write (issue, arrival, landing).
        const M_WRITE = 3;
        const M_END = 6;
        /// A successor's steps: the metadata register, the hold, the data
        /// register, its re-check, the data report, the metadata report, the
        /// preempts; S2 then reallocates.
        const S_END = 7;

        reserved: bool = cfg.data_rtype == .wear,
        dreg: [3]bool = .{ cfg.data_rtype == .wear and !cfg.late_admission, false, false },
        mreg: [3]bool = .{ true, false, false },
        holder: u8 = A,
        m_admitted: bool = !cfg.late_admission,
        m_refused: bool = false,
        mw: Wr = .idle,
        realloc: bool = false,
        victim: [2]u8 = @splat(NONE),
        dsnap: [2][3]bool = @splat(@splat(false)),
        msnap: [2][3]bool = @splat(@splat(false)),
        stopped: [2]bool = @splat(false),
        pc: [3]u8 = .{ if (cfg.late_admission) 0 else M_WRITE, 0, 0 },
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
                else => unreachable,
            }
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
                    if (cfg.data_rtype == .wear and self.dreg[h]) for (0..3) |k| {
                        if (k == h or !self.dreg[k]) continue;
                        const fence = switch (cfg.reconcile) {
                            .unregistered_on_meta => self.dsnap[s][k] and !self.msnap[s][k],
                            .victim_only => k == self.victim[s],
                            .none => false,
                        };
                        if (fence) self.dreg[k] = false;
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
