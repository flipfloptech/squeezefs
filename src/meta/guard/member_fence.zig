//! Member re-admission at a metadata refusal (decision 0004 §4.5, PR-4).
//! The pure contract: which refusal latches, which retries, which waits,
//! and when a reservation command may be submitted (DO.33). The journal
//! lane and the checkpoint binding consult a `readmit` hook before their
//! fence rungs. Page writers keep `Held` distinct from `Io`. A joined
//! mount installs `Readmit`. Its beat, on the checkpoint tick, fills the
//! observation from the appender pages and one reservation report: a
//! successor holds, and a new recovering sibling or this incarnation's
//! page leaving Live latches. A registered report clears the word. An
//! unregistered current member re-registers through the stale-sibling
//! rule once writes already submitted on that controller have completions
//! (DO.33); a fresh registration whose page is no longer Live is withdrawn.

const std = @import("std");

pub const Verdict = enum { retry, latch, held };

pub const Op = enum { read, write };

/// A device error the completion hook classifies. Anything else is not a
/// refusal and stays on today's path.
pub const Class = enum { conflict, eio };

pub const Hook = *const fn (ctx: ?*anyopaque, op: Op, class: Class) Verdict;

/// True when a completion hook answers `.held` for this device error.
pub fn isHeld(hook: ?Hook, ctx: ?*anyopaque, err: anyerror, op: Op) bool {
    const class: Class = if (err == error.ReservationConflict)
        .conflict
    else if (err == error.InputOutput)
        .eio
    else
        return false;
    const f = hook orelse return false;
    return f(ctx, op, class) == .held;
}

/// A metadata write that used to collapse every device class into `Io`
/// (decision 0004 §4.16). `Held` stays itself; everything else is `Io`.
pub fn mapDeviceWrite(err: anyerror) error{ Io, Held } {
    return if (err == error.Held) error.Held else error.Io;
}

pub const PageState = enum { free, live, recovering, recovered };

pub const PageView = struct {
    appender_id: u32,
    state: PageState,
    writer_id: u64,
    node_token: u64,
    term: u64,
};

pub const SelfView = struct {
    appender_id: u32,
    writer_id: u64,
    node_token: u64,
    term: u64,
    /// Appender 0's writer the last time this host saw itself registered.
    mgr_writer: u64,
    meta_rtype: u8 = 3,
};

pub const ReportView = struct {
    readable: bool = false,
    rtype: u8 = 0,
    host_registered: bool = false,
};

/// Recovering pages observed while this host was registered. A later
/// refusal treats only a page outside this set as a sibling death.
pub const Seen = struct {
    const cap = 32;
    n: u8 = 0,
    ids: [cap]u32 = @splat(0),
    terms: [cap]u64 = @splat(0),

    pub fn contains(self: Seen, id: u32, term: u64) bool {
        for (self.ids[0..self.n], self.terms[0..self.n]) |i, t| {
            if (i == id and t == term) return true;
        }
        return false;
    }

    pub fn note(self: *Seen, id: u32, term: u64) void {
        if (self.contains(id, term) or self.n == cap) return;
        self.ids[self.n] = id;
        self.terms[self.n] = term;
        self.n += 1;
    }
};

pub const PageFacts = struct {
    own_page_live: bool,
    successor_page: bool,
    sibling_death: bool,
};

/// Page facts for one beat. `seen` is the set from before this beat.
pub fn pageFacts(who: SelfView, pages: []const PageView, seen: Seen) PageFacts {
    var own_live = false;
    var successor = false;
    var sibling = false;
    for (pages) |p| {
        if (p.appender_id == 0 and p.state == .live and p.writer_id != who.mgr_writer) successor = true;
        if (p.appender_id == who.appender_id) {
            own_live = p.state == .live and p.writer_id == who.writer_id and p.node_token == who.node_token and p.term == who.term;
        }
        if (p.appender_id != who.appender_id and p.state == .recovering and p.node_token == who.node_token and !seen.contains(p.appender_id, p.term)) sibling = true;
    }
    return .{ .own_page_live = own_live, .successor_page = successor, .sibling_death = sibling };
}

fn noteRecovering(seen: *Seen, who: SelfView, pages: []const PageView) void {
    for (pages) |p| {
        if (p.state == .recovering and p.node_token == who.node_token) seen.note(p.appender_id, p.term);
    }
}

/// A joined member's readmit state. The default observation is undecided
/// (unregistered, no successor page yet), so a reservation conflict is
/// held rather than latched. A manager sets `obs.manages` and latches.
/// `beat` fills `obs` from the appender pages and, when the report is
/// readable, from the reservation.
pub const Readmit = struct {
    obs: Observation = .{
        .manages = false,
        .host_registered = false,
        .own_page_live = true,
        .rtype = 3,
        .meta_rtype = 3,
    },
    seen: Seen = .{},
    word: ?*Resolution = null,
    on_held: ?*const fn (ctx: ?*anyopaque) void = null,
    on_held_ctx: ?*anyopaque = null,

    pub fn joiner() Readmit {
        return .{};
    }

    pub fn answer(self: *Readmit, op: Op, class: Class) Verdict {
        if (op == .read and class == .eio) return .retry;
        if (self.obs.manages) return .latch;
        if (!self.obs.own_page_live) return .latch;
        const verdict = classify(self.obs).verdict;
        if (verdict == .held) if (self.on_held) |f| f(self.on_held_ctx);
        return verdict;
    }

    pub fn hook(ctx: ?*anyopaque, op: Op, class: Class) Verdict {
        const self: *Readmit = @ptrCast(@alignCast(ctx.?));
        return self.answer(op, class);
    }

    /// One off-I/O-path pass. An unreadable report does not by itself park
    /// a member whose pages are still its own. A new recovering sibling, or
    /// this incarnation's page leaving Live, latches. A successor page, or
    /// a readable report that finds the host unregistered, holds.
    pub fn beat(self: *Readmit, who: SelfView, pages: []const PageView, report: ReportView) Verdict {
        const facts = pageFacts(who, pages, self.seen);
        self.obs.own_page_live = facts.own_page_live;
        self.obs.sibling_death = facts.sibling_death;
        self.obs.successor_page = facts.successor_page;
        self.obs.meta_rtype = who.meta_rtype;
        if (report.readable) {
            self.obs.rtype = report.rtype;
            self.obs.host_registered = report.host_registered;
            if (report.host_registered) noteRecovering(&self.seen, who, pages);
        }
        const verdict = classify(self.obs).verdict;
        const fence = verdict == .latch and (report.readable or !facts.own_page_live or facts.sibling_death or facts.successor_page);
        const park = verdict == .held and (report.readable or facts.successor_page);
        if (self.word) |w| {
            if (fence) w.terminate();
            if (park) _ = w.reopen();
            if (verdict == .retry and report.readable and report.host_registered and facts.own_page_live) _ = w.clear();
        }
        if (park) if (self.on_held) |f| f(self.on_held_ctx);
        if (fence) return .latch;
        if (park) return .held;
        if (report.readable and verdict == .retry) return .retry;
        return .retry;
    }
};

/// Whether `func`'s error union names `name` (the §4.16 explicit-set rail).
pub fn fnErrorSetHas(comptime func: anytype, comptime name: []const u8) bool {
    const Ret = @typeInfo(@TypeOf(func)).@"fn".return_type.?;
    const set = @typeInfo(Ret).error_union.error_set;
    const names = @typeInfo(set).error_set.error_names orelse return true;
    inline for (names) |n| {
        if (std.mem.eql(u8, n, name)) return true;
    }
    return false;
}

pub const Arm = enum {
    manager,
    own_death,
    lapse,
    registered,
    budget,
    foreign_rtype,
    succession,
    sibling_death,
    undecided,
};

pub const Classified = struct { verdict: Verdict, arm: Arm };

/// One refusal's inputs, already read. Arm order is R-0, R-1, R-2, R-3
/// (decision 0004 §4.5.3): own death before any retry, succession before a
/// sibling-death latch.
pub const Observation = struct {
    manages: bool = false,
    own_page_live: bool = true,
    /// Report rtype. 0 is a lapse.
    rtype: u8 = 3,
    /// 3 on today's WERO code, 5 once metadata is WEAR.
    meta_rtype: u8 = 3,
    host_registered: bool = true,
    retry_credits: u32 = 1,
    /// Appender 0's Live page names a writer other than `mgr_writer`.
    successor_page: bool = false,
    /// A sibling page turned Recovering after the last confirmed
    /// registration, and no `r` line of this process covers it.
    sibling_death: bool = false,
};

pub const BarrierAnswer = enum { registered, unregistered, unknown };

pub const ReadStep = enum { retry_read, reissue_barrier, surface_eio };

pub const WriteEio = enum { retry_once, refused, held };

/// Read-EIO barrier attempts before the original EIO surfaces
/// (decision 0004 §4.5.3: 5 attempts, and one claim TTL, whichever first).
pub const READ_BARRIER_ATTEMPTS: u32 = 5;

pub fn classify(o: Observation) Classified {
    if (o.manages) return .{ .verdict = .latch, .arm = .manager };
    if (!o.own_page_live) return .{ .verdict = .latch, .arm = .own_death };
    if (o.rtype == 0) return budgeted(o, .lapse);
    if (o.rtype == o.meta_rtype and o.host_registered) return budgeted(o, .registered);
    if (o.rtype != o.meta_rtype) return .{ .verdict = .latch, .arm = .foreign_rtype };
    if (o.successor_page) return .{ .verdict = .held, .arm = .succession };
    if (o.sibling_death) return .{ .verdict = .latch, .arm = .sibling_death };
    return .{ .verdict = .held, .arm = .undecided };
}

fn budgeted(o: Observation, arm: Arm) Classified {
    if (o.retry_credits == 0) return .{ .verdict = .latch, .arm = .budget };
    return .{ .verdict = .retry, .arm = arm };
}

pub fn readAfterEio(answer: BarrierAnswer, unknowns_already: u32, within_claim_ttl: bool) ReadStep {
    return switch (answer) {
        .registered, .unregistered => .retry_read,
        .unknown => if (!within_claim_ttl or unknowns_already >= READ_BARRIER_ATTEMPTS - 1)
            .surface_eio
        else
            .reissue_barrier,
    };
}

pub fn writeAfterEio(answer: BarrierAnswer) WriteEio {
    return switch (answer) {
        .registered => .retry_once,
        .unregistered => .refused,
        .unknown => .held,
    };
}

pub const Gen = u62;

pub const Resolution = struct {
    word: std.atomic.Value(u64) = .init(0),

    pub const State = enum(u2) { pending, cleared, terminal };
    pub const Snapshot = struct { state: State, gen: Gen };

    const gen_mask: u64 = (@as(u64, 1) << 62) - 1;

    pub fn snapshot(self: *const Resolution) Snapshot {
        return unpack(self.word.load(.acquire));
    }

    /// Publish `cleared` and bump the generation. False when `terminal`
    /// already absorbed the word, or the generation cannot advance inside
    /// its 62 bits.
    pub fn clear(self: *Resolution) bool {
        while (true) {
            const cur = self.word.load(.acquire);
            const s = unpack(cur);
            if (s.state == .terminal) return false;
            const next_gen, const overflow = @addWithOverflow(s.gen, 1);
            if (overflow == 1) return false;
            const next = pack(.cleared, next_gen);
            if (self.word.cmpxchgWeak(cur, next, .release, .acquire) == null) return true;
        }
    }

    /// Absorbing. A later `clear` or `reopen` leaves the word here.
    pub fn terminate(self: *Resolution) void {
        while (true) {
            const cur = self.word.load(.acquire);
            const s = unpack(cur);
            if (s.state == .terminal) return;
            const next = pack(.terminal, s.gen);
            if (self.word.cmpxchgWeak(cur, next, .release, .acquire) == null) return;
        }
    }

    /// `cleared` → `pending` for a new succession, generation unchanged.
    /// False when `terminal`.
    pub fn reopen(self: *Resolution) bool {
        while (true) {
            const cur = self.word.load(.acquire);
            const s = unpack(cur);
            if (s.state == .terminal) return false;
            if (s.state == .pending) return true;
            const next = pack(.pending, s.gen);
            if (self.word.cmpxchgWeak(cur, next, .release, .acquire) == null) return true;
        }
    }

    fn pack(state: State, gen: Gen) u64 {
        return (@as(u64, @backingInt(state)) << 62) | @as(u64, gen);
    }

    fn unpack(w: u64) Snapshot {
        return .{
            .state = @fromBackingInt(@intCast(@as(u2, @truncate(w >> 62)))),
            .gen = @intCast(w & gen_mask),
        };
    }
};

/// DO.33. Register, Replace, Acquire, Release and Preempt take `pr_sem`
/// inside the TCP queue worker. One of them may be submitted on a
/// controller only once every write already submitted there has a completion.
pub const PrSubmit = struct {
    writes_awaiting_completion: u64 = 0,

    pub fn noteWriteSubmitted(self: *PrSubmit) void {
        self.writes_awaiting_completion += 1;
    }

    pub fn noteWriteCompleted(self: *PrSubmit) void {
        if (self.writes_awaiting_completion == 0) return;
        self.writes_awaiting_completion -= 1;
    }

    pub fn prSemAllowed(self: *const PrSubmit) bool {
        return self.writes_awaiting_completion == 0;
    }
};

/// C1's page evidence (a successor) and C2 (this incarnation still Live):
/// re-register through the rule once writes already submitted on this
/// controller have completions (DO.33). An unreadable report, a lapse, a
/// sibling death, a manager, or a terminal word does not register.
pub fn wantsRegister(o: Observation, report_readable: bool, pr_sem_allowed: bool, word: ?*const Resolution) bool {
    if (!report_readable or !pr_sem_allowed) return false;
    if (word) |w| if (w.snapshot().state == .terminal) return false;
    if (o.host_registered or !o.own_page_live) return false;
    const c = classify(o);
    return c.verdict == .held and c.arm == .succession;
}

/// Withdrawal applies only to this beat's fresh registration (decision
/// 0004 §4.5.4). An adopted registration is left standing.
pub fn shouldWithdraw(own_page_live: bool, word: ?*const Resolution, fresh: bool) bool {
    if (!fresh) return false;
    if (!own_page_live) return true;
    if (word) |w| return w.snapshot().state == .terminal;
    return false;
}

const testing = std.testing;

test "member fence: a manager refusal latches before own death or a lapse retry" {
    const c = classify(.{ .manages = true, .own_page_live = false, .rtype = 0 });
    try testing.expectEqual(Arm.manager, c.arm);
    try testing.expectEqual(Verdict.latch, c.verdict);
}

test "member fence: own death latches before a lapse retry" {
    const c = classify(.{ .own_page_live = false, .rtype = 0, .retry_credits = 1 });
    try testing.expectEqual(Arm.own_death, c.arm);
    try testing.expectEqual(Verdict.latch, c.verdict);
}

test "member fence: a lapse retries while a credit remains, and latches when it does not" {
    const open = classify(.{ .rtype = 0, .retry_credits = 1, .meta_rtype = 5 });
    try testing.expectEqual(Arm.lapse, open.arm);
    try testing.expectEqual(Verdict.retry, open.verdict);
    const spent = classify(.{ .rtype = 0, .retry_credits = 0 });
    try testing.expectEqual(Arm.budget, spent.arm);
    try testing.expectEqual(Verdict.latch, spent.verdict);
}

test "member fence: a registered host retries under WERO and under WEAR, and latches with no credit" {
    const wero = classify(.{ .meta_rtype = 3, .rtype = 3, .host_registered = true, .retry_credits = 1 });
    try testing.expectEqual(Arm.registered, wero.arm);
    const wear = classify(.{ .meta_rtype = 5, .rtype = 5, .host_registered = true, .retry_credits = 2 });
    try testing.expectEqual(Arm.registered, wear.arm);
    try testing.expectEqual(Verdict.retry, wear.verdict);
    const spent = classify(.{ .meta_rtype = 5, .rtype = 5, .host_registered = true, .retry_credits = 0 });
    try testing.expectEqual(Arm.budget, spent.arm);
    try testing.expectEqual(Verdict.latch, spent.verdict);
}

test "member fence: a foreign rtype latches, including WEAR while metadata is still WERO" {
    const c = classify(.{ .meta_rtype = 3, .rtype = 5, .host_registered = false, .successor_page = true });
    try testing.expectEqual(Arm.foreign_rtype, c.arm);
    try testing.expectEqual(Verdict.latch, c.verdict);
}

test "member fence: an unregistered host holds for a successor before it latches a sibling death" {
    const both = classify(.{
        .meta_rtype = 3,
        .rtype = 3,
        .host_registered = false,
        .successor_page = true,
        .sibling_death = true,
    });
    try testing.expectEqual(Arm.succession, both.arm);
    try testing.expectEqual(Verdict.held, both.verdict);
    const sibling = classify(.{
        .rtype = 3,
        .host_registered = false,
        .sibling_death = true,
    });
    try testing.expectEqual(Arm.sibling_death, sibling.arm);
    try testing.expectEqual(Verdict.latch, sibling.verdict);
    const waiting = classify(.{ .rtype = 5, .meta_rtype = 5, .host_registered = false });
    try testing.expectEqual(Arm.undecided, waiting.arm);
    try testing.expectEqual(Verdict.held, waiting.verdict);
}

test "member fence: read EIO retries after a finished barrier and surfaces at the bound" {
    try testing.expectEqual(ReadStep.retry_read, readAfterEio(.registered, 0, true));
    try testing.expectEqual(ReadStep.retry_read, readAfterEio(.unregistered, 0, true));
    try testing.expectEqual(ReadStep.reissue_barrier, readAfterEio(.unknown, 0, true));
    try testing.expectEqual(ReadStep.reissue_barrier, readAfterEio(.unknown, READ_BARRIER_ATTEMPTS - 2, true));
    try testing.expectEqual(ReadStep.surface_eio, readAfterEio(.unknown, READ_BARRIER_ATTEMPTS - 1, true));
    try testing.expectEqual(ReadStep.surface_eio, readAfterEio(.unknown, 0, false));
}

test "member fence: write EIO retries once, refuses when unregistered, and holds when unknown" {
    try testing.expectEqual(WriteEio.retry_once, writeAfterEio(.registered));
    try testing.expectEqual(WriteEio.refused, writeAfterEio(.unregistered));
    try testing.expectEqual(WriteEio.held, writeAfterEio(.unknown));
    const refused = classify(.{ .rtype = 3, .host_registered = false, .successor_page = true });
    try testing.expectEqual(Arm.succession, refused.arm);
}

test "member fence: clear publishes a new generation and terminal absorbs" {
    var w: Resolution = .{};
    try testing.expectEqual(Resolution.State.pending, w.snapshot().state);
    try testing.expectEqual(@as(Gen, 0), w.snapshot().gen);
    try testing.expect(w.clear());
    try testing.expectEqual(Resolution.State.cleared, w.snapshot().state);
    try testing.expectEqual(@as(Gen, 1), w.snapshot().gen);
    try testing.expect(w.clear());
    try testing.expectEqual(@as(Gen, 2), w.snapshot().gen);
    try testing.expect(w.reopen());
    try testing.expectEqual(Resolution.State.pending, w.snapshot().state);
    try testing.expectEqual(@as(Gen, 2), w.snapshot().gen);
    w.terminate();
    try testing.expectEqual(Resolution.State.terminal, w.snapshot().state);
    try testing.expectEqual(@as(Gen, 2), w.snapshot().gen);
    try testing.expect(!w.clear());
    try testing.expect(!w.reopen());
    try testing.expectEqual(Resolution.State.terminal, w.snapshot().state);
    try testing.expectEqual(@as(Gen, 2), w.snapshot().gen);
}

test "member fence: a reservation command waits for writes already submitted on this controller" {
    var g: PrSubmit = .{};
    try testing.expect(g.prSemAllowed());
    g.noteWriteSubmitted();
    g.noteWriteSubmitted();
    try testing.expect(!g.prSemAllowed());
    g.noteWriteCompleted();
    try testing.expect(!g.prSemAllowed());
    g.noteWriteCompleted();
    try testing.expect(g.prSemAllowed());
    g.noteWriteCompleted();
    g.noteWriteSubmitted();
    try testing.expect(!g.prSemAllowed());
}

test "member fence: an unregistered successor wants to re-register only when writes have completed" {
    const succ = Observation{
        .host_registered = false,
        .own_page_live = true,
        .rtype = 3,
        .meta_rtype = 3,
        .successor_page = true,
    };
    try testing.expect(wantsRegister(succ, true, true, null));
    try testing.expect(!wantsRegister(succ, true, false, null));
    try testing.expect(!wantsRegister(succ, false, true, null));
    try testing.expect(!wantsRegister(.{ .host_registered = true, .own_page_live = true, .rtype = 3, .successor_page = true }, true, true, null));
    try testing.expect(!wantsRegister(.{ .host_registered = false, .own_page_live = false, .rtype = 3, .successor_page = true }, true, true, null));
    try testing.expect(!wantsRegister(.{ .host_registered = false, .own_page_live = true, .rtype = 3, .sibling_death = true }, true, true, null));
    try testing.expect(!wantsRegister(.{ .host_registered = false, .own_page_live = true, .rtype = 3 }, true, true, null));
    try testing.expect(!wantsRegister(.{ .host_registered = false, .own_page_live = true, .rtype = 0, .successor_page = true }, true, true, null));
    try testing.expect(!wantsRegister(.{ .manages = true, .host_registered = false, .own_page_live = true, .rtype = 3, .successor_page = true }, true, true, null));
    var term: Resolution = .{};
    term.terminate();
    try testing.expect(!wantsRegister(succ, true, true, &term));
}

test "member fence: a fresh registration is withdrawn when the page leaves Live or the word is terminal" {
    try testing.expect(shouldWithdraw(false, null, true));
    try testing.expect(!shouldWithdraw(true, null, true));
    try testing.expect(!shouldWithdraw(false, null, false));
    var w: Resolution = .{};
    try testing.expect(!shouldWithdraw(true, &w, true));
    w.terminate();
    try testing.expect(shouldWithdraw(true, &w, true));
    try testing.expect(!shouldWithdraw(true, &w, false));
}

fn holdConflict(_: ?*anyopaque, _: Op, class: Class) Verdict {
    return if (class == .conflict) .held else .latch;
}

const me = SelfView{ .appender_id = 2, .writer_id = 9, .node_token = 4, .term = 1, .mgr_writer = 1 };

fn live(id: u32, writer: u64) PageView {
    return .{ .appender_id = id, .state = .live, .writer_id = writer, .node_token = if (id == 0) 1 else 4, .term = 1 };
}

test "member fence: the beat holds a successor, latches a new recovering sibling, and clears once registered" {
    var parks: u32 = 0;
    var word: Resolution = .{};
    var r = Readmit.joiner();
    r.word = &word;
    r.on_held_ctx = &parks;
    r.on_held = struct {
        fn bump(ctx: ?*anyopaque) void {
            const p: *u32 = @ptrCast(@alignCast(ctx.?));
            p.* += 1;
        }
    }.bump;
    const ours = live(2, 9);
    const mgr = live(0, 1);
    const next = live(0, 7);
    var sibling = live(3, 8);
    sibling.state = .recovering;
    sibling.term = 2;

    try testing.expectEqual(Verdict.held, r.beat(me, &.{ mgr, ours, next }, .{ .readable = false }));
    try testing.expect(r.obs.successor_page);
    try testing.expectEqual(@as(u32, 1), parks);
    try testing.expectEqual(Resolution.State.pending, word.snapshot().state);

    var quiet = Readmit.joiner();
    try testing.expectEqual(Verdict.retry, quiet.beat(me, &.{ mgr, ours }, .{ .readable = false }));
    try testing.expectEqual(@as(u32, 1), parks);

    try testing.expectEqual(Verdict.latch, r.beat(me, &.{ mgr, ours, sibling }, .{ .readable = true, .rtype = 3, .host_registered = false }));
    try testing.expect(r.obs.sibling_death);
    try testing.expectEqual(Resolution.State.terminal, word.snapshot().state);
    try testing.expectEqual(@as(u32, 1), parks);

    var noted = Readmit.joiner();
    try testing.expectEqual(Verdict.retry, noted.beat(me, &.{ mgr, ours, sibling }, .{ .readable = true, .rtype = 3, .host_registered = true }));
    try testing.expect(noted.seen.contains(3, 2));
    try testing.expectEqual(Verdict.held, noted.beat(me, &.{ next, ours, sibling }, .{ .readable = true, .rtype = 3, .host_registered = false }));
    try testing.expect(!noted.obs.sibling_death);
    try testing.expect(noted.obs.successor_page);

    var back = Readmit.joiner();
    var cleared: Resolution = .{};
    _ = cleared.reopen();
    back.word = &cleared;
    try testing.expectEqual(Verdict.retry, back.beat(me, &.{ mgr, ours }, .{ .readable = true, .rtype = 3, .host_registered = true }));
    try testing.expectEqual(Resolution.State.cleared, cleared.snapshot().state);

    var dead = Readmit.joiner();
    var recovering_self = ours;
    recovering_self.state = .recovering;
    try testing.expectEqual(Verdict.latch, dead.beat(me, &.{ mgr, recovering_self, next }, .{ .readable = false }));
    try testing.expect(!dead.obs.own_page_live);
}

test "member fence: a joiner's undecided conflict is held and notifies the park once" {
    var n: u32 = 0;
    var r = Readmit.joiner();
    r.on_held = struct {
        fn bump(ctx: ?*anyopaque) void {
            const p: *u32 = @ptrCast(@alignCast(ctx.?));
            p.* += 1;
        }
    }.bump;
    r.on_held_ctx = &n;
    try testing.expectEqual(Verdict.held, r.answer(.write, .conflict));
    try testing.expectEqual(@as(u32, 1), n);
    r.obs.manages = true;
    try testing.expectEqual(Verdict.latch, r.answer(.write, .conflict));
    try testing.expectEqual(@as(u32, 1), n);
    r.obs.manages = false;
    try testing.expectEqual(Verdict.retry, r.answer(.read, .eio));
}

test "member fence: a collapsed write keeps Held and maps every other class to Io" {
    try testing.expectEqual(error.Held, mapDeviceWrite(error.Held));
    try testing.expectEqual(error.Io, mapDeviceWrite(error.ReservationConflict));
    try testing.expectEqual(error.Io, mapDeviceWrite(error.InputOutput));
    try testing.expect(isHeld(holdConflict, null, error.ReservationConflict, .write));
    try testing.expect(!isHeld(null, null, error.ReservationConflict, .write));
    try testing.expect(!isHeld(holdConflict, null, error.InputOutput, .write));
}
