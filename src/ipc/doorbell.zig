//! The two doorbells of a session (charter §7; spec
//! `docs/porting/il_session.md` §4–§5; decision 0003 §F):
//!
//!   - the COMPLETION doorbell `{seq, parked, wake_at, wake_paid_mark}` on
//!     header line 2: any completing thread bumps `seq` and wakes iff a
//!     client is parked, the batch mark is reached and this mark was not
//!     already paid — the mark-valued collapse latch, ≤ 1 wake per park
//!     era; the client parks register → snapshot → re-scan;
//!   - the SUBMIT doorbell `{doorbell, daemon_parked}` on header line 1: the
//!     client bumps and wakes iff the daemon is parked; the service thread
//!     parks `parked := 1` → snapshot EVERY owned doorbell → re-scan →
//!     `futex_waitv` on all of them, bounded.
//!
//! Both are Dekker pairs ("I registered; did you move?" against "I moved;
//! are you registered?"). Zig 0.17 has no standalone fence builtin
//! (`@fence` left the language in 0.14) and a `seq_cst` RMW on an unrelated
//! word is not a fence on ARMv8, so — the house idiom (`fuse/transport/
//! slot.zig`) — the four accesses of each pair are THEMSELVES `seq_cst`:
//! C11's single total order over seq_cst operations forbids both sides
//! reading the pre-race value. `doorbell_model.zig` keeps the fence as an
//! explicit store-buffer drain step (an SC store IS a drained store) and
//! proves each weakening red: drop either side's drain, snapshot before
//! register, snapshot after rescan, the 0/1 flag latch.
//!
//! Trust boundary: scribbling `parked` huge costs one wake per completion
//! (self-harm); scribbling `seq`/`wake_at`/`wake_paid_mark` confuses only
//! the scribbler's own reaper, bounded by its wait bound. The daemon never
//! WAITS on any completion-doorbell word; its submit-side wait is bounded
//! by `SERVICE_PARK_MAX_NS`.

const std = @import("std");
const layout = @import("layout.zig");
const futex = @import("../sync/futex.zig");
const knobs = @import("../core/knobs.zig");
const ipc = @import("ipc.zig");

const Atomic = std.atomic.Value;
const CqeDoorbell = layout.CqeDoorbell;
const SessionHeader = layout.SessionHeader;

/// No daemon wait on a client-writable word exceeds this (the D18 ladder
/// cap; charter §7 "every wait on client memory bounded").
pub const SERVICE_PARK_MAX_NS: u64 = 5 * std.time.ns_per_ms;

// ---------------------------------------------------------------------------
// The completion doorbell.
// ---------------------------------------------------------------------------

/// The latch's initial record. `wake_at` initialises to 0 and the first mark
/// is `snapshot + k ≥ 1`, so a completer reading the mark word before the
/// first store (a stale-init read, which reads as reached) must find a
/// record that is NOT that stale value — it over-pays one wake (benign)
/// instead of collapsing against a pay nobody made.
pub const NO_PAY_RECORDED: u32 = std.math.maxInt(u32);

/// `seq` has reached the mark `at` (wrap-safe: a mark up to 2³¹ ahead is
/// "not yet"; a stale behind-the-seq mark reads as reached — over-wake,
/// never a strand).
pub fn reached(seq: u32, at: u32) bool {
    return seq -% at < (1 << 31);
}

pub const Outcome = enum {
    /// Nobody parked, or below the batch mark.
    elided,
    /// A wake for this mark is already paid or in flight.
    collapsed,
    /// This era's ONE syscall: the caller wakes the `seq` word (breadth
    /// INT_MAX — every parker of the era).
    wake,
};

/// The completer, called by ANY completing thread AFTER the slot's DONE
/// publish. `latch = false` is the A/B control arm (wake on every
/// mark-passed completion).
pub fn complete(d: *CqeDoorbell, latch: bool) Outcome {
    // seq' = seq + 1 — SeqCst: the Dekker "I moved".
    const seq = d.seq.fetchAdd(1, .seq_cst) +% 1;
    // "Are you registered?" — SeqCst: in the total order after our bump, so
    // a parker whose registration precedes its snapshot is seen here or its
    // snapshot sees our bump (the admission fails). The model's fence step.
    if (d.parked.load(.seq_cst) == 0) return .elided;
    const at = d.wake_at.load(.seq_cst);
    if (!reached(seq, at)) return .elided;
    if (!latch) return .wake;
    // The mark-valued latch: collapse iff the record equals the mark just
    // read; else record it and pay.
    var paid = d.wake_paid_mark.load(.seq_cst);
    while (true) {
        if (paid == at) return .collapsed;
        if (d.wake_paid_mark.cmpxchgWeak(paid, at, .seq_cst, .seq_cst)) |seen| {
            paid = seen;
            continue;
        }
        return .wake;
    }
}

/// `complete` + the syscall on `.wake`, counted (`ipc_cqe_wake_*`).
pub fn completeAndWake(d: *CqeDoorbell, latch: bool) Outcome {
    const o = complete(d, latch);
    ipc.countWake(o);
    if (o == .wake) futex.wakeSharedAll(&d.seq);
    return o;
}

/// The parker: REGISTER FIRST (`parked += 1`, SeqCst — the Dekker "I
/// registered"), THEN SNAPSHOT `seq` (SeqCst — "did you move?"), then
/// compose the batch mark `snapshot + k`. Returns the snapshot: the futex
/// `expected`. The caller MUST re-scan its pending set before sleeping (a
/// completion that beat the registration is found there; one landing after
/// fails the futex admission on the moved seq or counts toward the mark),
/// then `futex.waitSharedTimeout(&d.seq, snapshot, bound)` /
/// `futex.waitMany`, then `parkEnd`. No parker-side latch write exists.
pub fn parkBeginBatch(d: *CqeDoorbell, k_raw: u32) u32 {
    const k = @max(k_raw, 1);
    const prior = d.parked.fetchAdd(1, .seq_cst);
    const snap = d.seq.load(.seq_cst);
    const target = snap +% k;
    if (prior == 0) {
        // The first parker of the era owns the mark. A racing second
        // parker's earlier min may be overwritten: its wake is DELAYED to
        // our mark or its own bounded timeout, never lost.
        d.wake_at.store(target, .seq_cst);
        return snap;
    }
    var cur = d.wake_at.load(.seq_cst);
    while (true) {
        const next = composeMark(cur, snap, target, d.wake_paid_mark.load(.seq_cst)) orelse return snap;
        if (d.wake_at.cmpxchgWeak(cur, next, .seq_cst, .seq_cst)) |seen| {
            cur = seen;
            continue;
        }
        return snap;
    }
}

/// The batch-mark compose rule for a non-first parker (pure):
///   - a mark AT the snapshot: SPENT (its pay recorded) ⇒ replace; UNPAID ⇒
///     keep (its completer may still be in flight before its mark read);
///   - a live earlier mark (≤ ours ahead of the snapshot) governs;
///   - a stale or later mark is replaced.
/// `null` = keep `cur`.
pub fn composeMark(cur: u32, snap: u32, target: u32, paid: u32) ?u32 {
    const ahead = cur -% snap;
    if (ahead == 0) return if (paid == cur) target else null;
    if (ahead <= target -% snap) return null;
    return target;
}

pub fn parkBegin(d: *CqeDoorbell) u32 {
    return parkBeginBatch(d, 1);
}

pub fn parkEnd(d: *CqeDoorbell) void {
    _ = d.parked.fetchSub(1, .seq_cst);
}

/// `k = clamp(pending/4, 2, pending)` — FLOOR division deliberately (a wake
/// threshold rounded up is a LATER wake, the unsafe direction for delivery
/// latency). `pending/4` amortises the wake + reaper wakeup over a quarter
/// of the visible depth (~4× below the flat park's 0.8 wakes/op at 32×8)
/// while a completion BURST still trips the mark inside the age bound;
/// floor 2 = the first value distinguishable from the flat event park (k=1
/// IS the sparse regime); ceiling `pending` = the LIVENESS cap (only the
/// caller's own ops are guaranteed to produce completions), which DOMINATES
/// the floor. Measured herd it replaced: 17.8 M wakes / 22.2 M ops.
pub fn reapBatchWakeThreshold(pending: u32) u32 {
    return @min(@max(pending / 4, 2), @max(pending, 1));
}

// ---------------------------------------------------------------------------
// The submit doorbell.
// ---------------------------------------------------------------------------

/// The client's submit-side ring after its ring push: bump (SeqCst), then
/// read the daemon's parked flag (SeqCst) — the same Dekker as the
/// completion side (decision 0003 §F: the reference's `fetch_add(Release)`
/// with no fence was correct only on x86 and only with its 5 ms timeout).
/// Returns whether a wake syscall was paid.
pub fn ringSubmit(h: *SessionHeader) bool {
    _ = h.doorbell.fetchAdd(1, .seq_cst);
    if (h.daemon_parked.load(.seq_cst) == 0) return false;
    futex.wakeSharedAll(&h.doorbell);
    return true;
}

/// The service thread's park over every session it owns (≤
/// `futex.wait_many_max`; a session is pinned to ONE thread for life):
///
///     arm()      parked := 1 on every owned header (SeqCst), THEN snapshot
///                every doorbell (SeqCst) — snapshot BEFORE the caller's
///                rescan (snapshotting after folded a submission that raced
///                the rescan's tail INTO the expected value: 13 % of parks
///                expired by timeout under load);
///     <rescan>   the caller drains every session once more + flushes;
///     wait()     iff the rescan served nothing: `futex_waitv` on ALL owned
///                doorbells against the snapshots, bounded 5 ms (a single-
///                doorbell wait deafened the thread to its other sessions:
///                the sessions=8 < sessions=4 inversion);
///     disarm()   parked := 0.
pub const ServicePark = struct {
    headers: [futex.wait_many_max]*SessionHeader = undefined,
    entries: [futex.wait_many_max]futex.Waiter = undefined,
    n: usize = 0,

    pub fn arm(self: *ServicePark, owned: []*SessionHeader) void {
        std.debug.assert(owned.len <= futex.wait_many_max);
        self.n = owned.len;
        for (owned, 0..) |h, i| {
            self.headers[i] = h;
            h.daemon_parked.store(1, .seq_cst);
        }
        for (owned, 0..) |h, i| {
            self.entries[i] = .{ .word = &h.doorbell, .expect = h.doorbell.load(.seq_cst) };
        }
    }

    /// True when some doorbell moved since `arm` (a submission landed: the
    /// caller drains instead of waiting).
    pub fn anyMoved(self: *const ServicePark) bool {
        for (self.entries[0..self.n]) |e| if (e.word.load(.seq_cst) != e.expect) return true;
        return false;
    }

    pub fn wait(self: *const ServicePark) futex.WaitManyVerdict {
        ipc.stats.ipc_service_parks.inc();
        return futex.waitMany(self.entries[0..self.n], SERVICE_PARK_MAX_NS);
    }

    pub fn disarm(self: *const ServicePark) void {
        for (self.headers[0..self.n]) |h| h.daemon_parked.store(0, .seq_cst);
    }
};

// ---------------------------------------------------------------------------
// The service thread's spin-before-park governor (spec §4.4). Regime
// membership is derived; the magnitude is MEASURED.
// ---------------------------------------------------------------------------

pub const SpinGovernor = struct {
    /// Idle / device-RTT-spaced parks: spinning cannot absorb them.
    pub const SPIN_RAIL_US: u64 = 200;
    /// MEASURED (A-B-B-A at 32×8): 100 µs → +6.7 % median, 20 µs → +2 %,
    /// 200 µs → +4 %, 400+ flat; the first governor's `2 × EWMA ≈ 10–20 µs`
    /// counted a WASH — the win spans inter-burst TAIL gaps.
    pub const SPIN_WINDOW_US: u64 = 100;
    /// α = 1/8: the standing smoothing class.
    pub const EWMA_SHIFT: u6 = 3;

    pub const Config = struct {
        /// `SQUEEZEFS_IPC_SPIN_US` verbatim (governor OFF; 0 = the
        /// pre-campaign posture).
        explicit_us: ?u32,
        /// `SQUEEZEFS_IPC_SPIN_ADAPTIVE` (0 = window 0).
        adaptive: bool,

        pub fn fromEnv(env: *const std.process.Environ.Map) Config {
            return .{
                .explicit_us = knobs.optIntKnob("SQUEEZEFS_IPC_SPIN_US", u32, env),
                .adaptive = knobs.boolKnob("SQUEEZEFS_IPC_SPIN_ADAPTIVE", env),
            };
        }
    };

    /// `100 − ceil(100 × lanes / cores)`: 12 lanes / 32 cores → 62. A 2×
    /// margin disengaged exactly where the win lived because `/proc/stat`
    /// busy includes the workload's own nvme-tcp softirq.
    pub fn busyCeilingPct(lanes: u32, cores: u32) u32 {
        if (cores == 0) return 0;
        const used = (100 * @as(u64, lanes) + cores - 1) / cores;
        return @intCast(100 -| used);
    }

    /// `obs` if unseeded, else `ewma − ewma/8 + obs/8`.
    pub fn foldParkNs(ewma: u64, obs: u64) u64 {
        if (ewma == 0) return obs;
        return ewma - (ewma >> EWMA_SHIFT) + (obs >> EWMA_SHIFT);
    }

    /// The window in µs for one empty drain pass.
    pub fn windowUs(cfg: Config, ewma_park_ns: u64, busy_pct: u32, lanes: u32, cores: u32, lane_sessions: u32) u64 {
        if (cfg.explicit_us) |us| return us;
        if (!cfg.adaptive) return 0;
        // A single-session lane is the sync-RTT regime: parks are
        // PRODUCTIVE waits; the engaged spin cost qd1 2.5× (p99 70 → 157
        // µs) — the structural guard; the temporal test alone was falsified.
        if (lane_sessions < 2) return 0;
        // Engage on OBSERVED churn only.
        if (ewma_park_ns == 0) return 0;
        if (ewma_park_ns > SPIN_RAIL_US * std.time.ns_per_us) return 0;
        if (busy_pct > busyCeilingPct(lanes, cores)) return 0;
        return SPIN_WINDOW_US;
    }
};

/// `SQUEEZEFS_IPC_CQE_WAKE_LATCH` (0 = the wake-per-mark-passed control arm).
pub fn latchFromEnv(env: *const std.process.Environ.Map) bool {
    return knobs.boolKnob("SQUEEZEFS_IPC_CQE_WAKE_LATCH", env);
}

// ---------------------------------------------------------------------------
// Tests (spec §8: the cqe_core rows, the threshold tie, the governor rows).
// ---------------------------------------------------------------------------

const testing = std.testing;

test "wake gated on the parked count: nobody parked ⇒ elided whatever seq does" {
    var d: CqeDoorbell = .{};
    try testing.expectEqual(Outcome.elided, complete(&d, true));
    try testing.expectEqual(Outcome.elided, complete(&d, false));
    try testing.expectEqual(@as(u32, 2), d.seq.load(.monotonic));
}

test "batch park wakes at the k-th completion; parkBegin is batch one; the first completion of a qd1 era still pays immediately" {
    var d: CqeDoorbell = .{};
    const snap = parkBeginBatch(&d, 3);
    try testing.expectEqual(@as(u32, 0), snap);
    try testing.expectEqual(@as(u32, 3), d.wake_at.load(.monotonic));
    try testing.expectEqual(Outcome.elided, complete(&d, true));
    try testing.expectEqual(Outcome.elided, complete(&d, true));
    try testing.expectEqual(Outcome.wake, complete(&d, true));
    parkEnd(&d);
    try testing.expectEqual(@as(u32, 0), d.parked.load(.monotonic));
    // qd1: k = 1, the first completion pays.
    var e: CqeDoorbell = .{};
    const s = parkBegin(&e);
    try testing.expectEqual(s +% 1, e.wake_at.load(.monotonic));
    try testing.expectEqual(Outcome.wake, complete(&e, true));
    parkEnd(&e);
}

test "the earliest live mark governs across parkers; a stale or later mark is replaced; an at-snapshot mark is kept while unpaid and replaced once spent" {
    // compose(cur, snap, target, paid)
    try testing.expectEqual(@as(?u32, null), composeMark(5, 3, 7, NO_PAY_RECORDED)); // cur 2 ahead ≤ our 4: keep
    try testing.expectEqual(@as(?u32, 7), composeMark(12, 3, 7, NO_PAY_RECORDED)); // cur 9 ahead > 4: replace
    try testing.expectEqual(@as(?u32, 7), composeMark(1, 3, 7, NO_PAY_RECORDED)); // stale (behind): wraps huge: replace
    try testing.expectEqual(@as(?u32, null), composeMark(3, 3, 7, NO_PAY_RECORDED)); // at snapshot, unpaid: keep
    try testing.expectEqual(@as(?u32, 7), composeMark(3, 3, 7, 3)); // at snapshot, spent: replace
    // Through the words: a second parker with a nearer mark.
    var d: CqeDoorbell = .{};
    _ = parkBeginBatch(&d, 8);
    _ = parkBeginBatch(&d, 2);
    try testing.expectEqual(@as(u32, 2), d.wake_at.load(.monotonic));
    try testing.expectEqual(@as(u32, 2), d.parked.load(.monotonic));
    try testing.expectEqual(Outcome.elided, complete(&d, true));
    try testing.expectEqual(Outcome.wake, complete(&d, true));
    parkEnd(&d);
    parkEnd(&d);
}

test "the latch collapses an era to one wake; a new era is payable; a pay for a stale mark never covers the era; the initial record never collapses" {
    var d: CqeDoorbell = .{};
    try testing.expectEqual(NO_PAY_RECORDED, d.wake_paid_mark.load(.monotonic));
    _ = parkBegin(&d);
    try testing.expectEqual(Outcome.wake, complete(&d, true));
    // The same mark, still parked: collapsed.
    try testing.expectEqual(Outcome.collapsed, complete(&d, true));
    try testing.expectEqual(Outcome.collapsed, complete(&d, true));
    parkEnd(&d);
    // A new era: a new mark ⇒ payable again.
    _ = parkBegin(&d);
    try testing.expectEqual(Outcome.wake, complete(&d, true));
    parkEnd(&d);
    // A late completer that paid for a STALE mark (it read the mark word
    // before the parker's store): its record covers nothing of the new era.
    var e: CqeDoorbell = .{};
    e.wake_paid_mark.store(0, .monotonic); // "paid for mark 0" from a bygone era
    _ = parkBegin(&e); // mark = 1
    try testing.expectEqual(Outcome.wake, complete(&e, true));
    parkEnd(&e);
    // The control arm: every mark-passed completion wakes.
    var f: CqeDoorbell = .{};
    _ = parkBegin(&f);
    try testing.expectEqual(Outcome.wake, complete(&f, false));
    try testing.expectEqual(Outcome.wake, complete(&f, false));
    parkEnd(&f);
}

test "wake covers every parker: breadth is the whole seq word (two parkers, one era, one wake)" {
    var d: CqeDoorbell = .{};
    _ = parkBegin(&d);
    _ = parkBegin(&d);
    try testing.expectEqual(@as(u32, 2), d.parked.load(.monotonic));
    try testing.expectEqual(Outcome.wake, complete(&d, true));
    try testing.expectEqual(Outcome.collapsed, complete(&d, true));
    parkEnd(&d);
    parkEnd(&d);
}

test "reached is wrap-safe: a mark 2^31−1 ahead is not reached, a mark behind is" {
    try testing.expect(reached(5, 5));
    try testing.expect(reached(6, 5));
    try testing.expect(!reached(5, 6));
    try testing.expect(reached(1, std.math.maxInt(u32)));
    try testing.expect(!reached(0, (1 << 31) - 1));
    try testing.expect(reached((1 << 31) - 1, 0));
    try testing.expect(!reached(1 << 31, 0));
}

test "reap_batch_wake_threshold = clamp(pending/4, 2, pending): floor division; the liveness ceiling dominates the floor" {
    const rows = [_][2]u32{ .{ 0, 1 }, .{ 1, 1 }, .{ 2, 2 }, .{ 3, 2 }, .{ 8, 2 }, .{ 9, 2 }, .{ 12, 3 }, .{ 16, 4 }, .{ 32, 8 }, .{ 128, 32 }, .{ 1023, 255 } };
    for (rows) |r| try testing.expectEqual(r[1], reapBatchWakeThreshold(r[0]));
    var p: u32 = 0;
    while (p < 4096) : (p += 1) {
        const k = reapBatchWakeThreshold(p);
        try testing.expect(k >= 1);
        try testing.expect(k <= @max(p, 1));
        if (p >= 8) try testing.expectEqual(p / 4, k);
    }
}

test "submit doorbell: the bump is paid every time; the wake only while the daemon is parked" {
    var h: SessionHeader = .{ .geometry = layout.Geometry.default };
    try testing.expect(!ringSubmit(&h));
    try testing.expectEqual(@as(u32, 1), h.doorbell.load(.monotonic));
    h.daemon_parked.store(1, .seq_cst);
    try testing.expect(ringSubmit(&h));
    try testing.expectEqual(@as(u32, 2), h.doorbell.load(.monotonic));
}

test "service park: arm sets every parked flag then snapshots; a submission after the snapshot is seen by anyMoved; disarm clears every flag" {
    var a: SessionHeader = .{ .geometry = layout.Geometry.default };
    var b: SessionHeader = .{ .geometry = layout.Geometry.default };
    var owned = [_]*SessionHeader{ &a, &b };
    var park: ServicePark = .{};
    park.arm(&owned);
    try testing.expectEqual(@as(u32, 1), a.daemon_parked.load(.monotonic));
    try testing.expectEqual(@as(u32, 1), b.daemon_parked.load(.monotonic));
    try testing.expect(!park.anyMoved());
    try testing.expect(ringSubmit(&b));
    try testing.expect(park.anyMoved());
    park.disarm();
    try testing.expectEqual(@as(u32, 0), a.daemon_parked.load(.monotonic));
    try testing.expectEqual(@as(u32, 0), b.daemon_parked.load(.monotonic));
    try testing.expectEqual(@as(u64, 5 * std.time.ns_per_ms), SERVICE_PARK_MAX_NS);
}

test "service park on Linux: a parked owner is woken by a client's submit on ANY owned session within the bound" {
    if (@import("builtin").os.tag != .linux) return error.SkipZigTest;
    // The words must live in shared memory for the shared futex face; a
    // memfd page stands in for the session header's line 1.
    const linux = std.os.linux;
    const rc = linux.memfd_create("sqz-doorbell-test", linux.MFD.CLOEXEC);
    if (linux.errno(rc) != .SUCCESS) return error.MemfdFailed;
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    if (linux.errno(linux.ftruncate(fd, 2 * layout.PAGE_BYTES)) != .SUCCESS) return error.MemfdFailed;
    const m = linux.mmap(null, 2 * layout.PAGE_BYTES, .{ .READ = true, .WRITE = true }, .{ .TYPE = .SHARED }, fd, 0);
    if (linux.errno(m) != .SUCCESS) return error.MapFailed;
    defer _ = linux.munmap(@ptrFromInt(m), 2 * layout.PAGE_BYTES);
    const a: *SessionHeader = @ptrFromInt(m);
    const b: *SessionHeader = @ptrFromInt(m + layout.PAGE_BYTES);
    a.* = .{ .geometry = layout.Geometry.default };
    b.* = .{ .geometry = layout.Geometry.default };
    var owned = [_]*SessionHeader{ a, b };
    var park: ServicePark = .{};
    park.arm(&owned);
    const Client = struct {
        fn run(h: *SessionHeader) void {
            while (h.daemon_parked.load(.seq_cst) == 0) std.atomic.spinLoopHint();
            _ = ringSubmit(h);
        }
    };
    const t = try std.Thread.spawn(.{}, Client.run, .{b});
    const verdict = park.wait();
    t.join();
    park.disarm();
    try testing.expect(verdict != .unsupported);
    try testing.expect(park.anyMoved());
    // An idle park expires at the bound, never before the flag is cleared.
    park.arm(&owned);
    try testing.expectEqual(futex.WaitManyVerdict.timed_out, park.wait());
    park.disarm();
}

test "spin governor: the churn regime engages the measured window; idle, saturation, unseeded and single-session lanes disengage; the explicit knob wins; the ceiling derives from lanes and cores; the EWMA folds toward observations" {
    const adaptive: SpinGovernor.Config = .{ .explicit_us = null, .adaptive = true };
    try testing.expectEqual(@as(u64, 100), SpinGovernor.windowUs(adaptive, 50 * std.time.ns_per_us, 40, 12, 32, 4));
    try testing.expectEqual(@as(u64, 0), SpinGovernor.windowUs(adaptive, 300 * std.time.ns_per_us, 40, 12, 32, 4));
    try testing.expectEqual(@as(u64, 0), SpinGovernor.windowUs(adaptive, 50 * std.time.ns_per_us, 63, 12, 32, 4));
    try testing.expectEqual(@as(u64, 100), SpinGovernor.windowUs(adaptive, 50 * std.time.ns_per_us, 62, 12, 32, 4));
    try testing.expectEqual(@as(u64, 0), SpinGovernor.windowUs(adaptive, 0, 0, 12, 32, 4));
    try testing.expectEqual(@as(u64, 0), SpinGovernor.windowUs(adaptive, 50 * std.time.ns_per_us, 0, 12, 32, 1));
    try testing.expectEqual(@as(u64, 0), SpinGovernor.windowUs(.{ .explicit_us = null, .adaptive = false }, 50 * std.time.ns_per_us, 0, 12, 32, 4));
    try testing.expectEqual(@as(u64, 7), SpinGovernor.windowUs(.{ .explicit_us = 7, .adaptive = true }, 0, 0, 12, 32, 1));
    try testing.expectEqual(@as(u64, 0), SpinGovernor.windowUs(.{ .explicit_us = 0, .adaptive = true }, 50 * std.time.ns_per_us, 0, 12, 32, 4));
    try testing.expectEqual(@as(u32, 62), SpinGovernor.busyCeilingPct(12, 32));
    try testing.expectEqual(@as(u32, 50), SpinGovernor.busyCeilingPct(16, 32));
    try testing.expectEqual(@as(u32, 0), SpinGovernor.busyCeilingPct(64, 32));
    try testing.expectEqual(@as(u32, 0), SpinGovernor.busyCeilingPct(1, 0));
    try testing.expectEqual(@as(u64, 800), SpinGovernor.foldParkNs(0, 800));
    try testing.expectEqual(@as(u64, 800 - 100 + 50), SpinGovernor.foldParkNs(800, 400));
    var ewma: u64 = 0;
    var i: u32 = 0;
    while (i < 64) : (i += 1) ewma = SpinGovernor.foldParkNs(ewma, 1600);
    try testing.expectEqual(@as(u64, 1600), ewma);
}

test "knob faces: the spin levers and the latch read their rows" {
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    const c0 = SpinGovernor.Config.fromEnv(&env);
    try testing.expectEqual(@as(?u32, null), c0.explicit_us);
    try testing.expect(c0.adaptive);
    try testing.expect(latchFromEnv(&env));
    try env.put("SQUEEZEFS_IPC_SPIN_US", "0");
    try env.put("SQUEEZEFS_IPC_SPIN_ADAPTIVE", "0");
    try env.put("SQUEEZEFS_IPC_CQE_WAKE_LATCH", "0");
    const c1 = SpinGovernor.Config.fromEnv(&env);
    try testing.expectEqual(@as(?u32, 0), c1.explicit_us);
    try testing.expect(!c1.adaptive);
    try testing.expect(!latchFromEnv(&env));
}
