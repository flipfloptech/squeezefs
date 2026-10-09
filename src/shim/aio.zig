//! libaio interposition (spec `docs/porting/il_shim.md` §6): `io_setup`,
//! `io_destroy`, `io_submit`, `io_getevents`, `io_pgetevents`, `io_cancel`.
//! A batch is decomposed per iocb: a PREAD/PWRITE on a bound fd that fits
//! one slot rides the ring as a no-wait ticket; everything else (vectored,
//! fsync, poll, RESFD, foreign RWF flags, unbound fds, over-slab sizes, the
//! lane gate's routes) goes to the REAL libaio in contiguous runs that keep
//! the batch's order (the prefix law: nothing later ever dispatches first;
//! a partial kernel run ends the prefix; a ring refusal REROUTES the iocb
//! into the open kernel run — never −EAGAIN, never a truncated prefix).
//!
//! The reap merges ring completions first (written straight into the
//! caller's array), then the kernel lane's (non-blocking while ring ops are
//! pending; the remaining budget when only kernel ops are), and parks on
//! the sessions' completion doorbells (register → snapshot → re-scan) with
//! `k = clamp(pending/4, 2, pending)` in the deep regime and k = 1 in the
//! sparse one, bounded 1 ms when kernel ops are pending (their completions
//! cannot wake a futex), 5 ms ring-only.
//!
//! Per context ONE futex mutex over its ticket table (sized by `nr_events`
//! at `io_setup`: one allocation per context life, decision 0002 ledger
//! S9); the table is static so a data-path reference never dangles.
//! Return convention is libaio's: 0-or-count, NEGATIVE errno.

const std = @import("std");
const linux = std.os.linux;
const Atomic = std.atomic.Value;

const sys = @import("sys.zig");
const fd_table = @import("fd_table.zig");
const classify_mod = @import("classify.zig");
const ops = @import("ops.zig");
const client = @import("../ipc/client.zig");
const layout = @import("../ipc/layout.zig");
const doorbell = @import("../ipc/doorbell.zig");
const knobs = @import("../core/knobs.zig");
const clock = @import("../core/clock.zig");

pub const State = classify_mod.State;
pub const Session = client.Session;

// ---------------------------------------------------------------------------
// Userspace ABI mirrors (libaio.h's shapes, NOT the kernel aio_abi.h's).
// ---------------------------------------------------------------------------

pub const io_context_t = usize;

pub const Iocb = extern struct {
    data: u64,
    key: u32,
    aio_rw_flags: u32,
    aio_lio_opcode: u16,
    aio_reqprio: u16,
    aio_fildes: i32,
    buf: u64,
    nbytes: u64,
    offset: i64,
    __pad3: i64,
    flags: u32,
    resfd: u32,
};

pub const IoEvent = extern struct {
    data: u64,
    /// The ORIGINAL iocb pointer.
    obj: u64,
    res: i64,
    res2: i64,
};

comptime {
    std.debug.assert(@sizeOf(Iocb) == 64);
    std.debug.assert(@offsetOf(Iocb, "aio_fildes") == 20);
    std.debug.assert(@offsetOf(Iocb, "buf") == 24);
    std.debug.assert(@offsetOf(Iocb, "offset") == 40);
    std.debug.assert(@offsetOf(Iocb, "flags") == 56);
    std.debug.assert(@sizeOf(IoEvent) == 32);
}

pub const IOCB_CMD_PREAD: u16 = 0;
pub const IOCB_CMD_PWRITE: u16 = 1;
pub const IOCB_FLAG_RESFD: u32 = 1;

/// The libaio symbol versions (`dlvsym` targets; §6.1).
pub const Versions = struct {
    pub const io_setup = "LIBAIO_0.4";
    pub const io_destroy = "LIBAIO_0.4";
    pub const io_submit = "LIBAIO_0.1";
    pub const io_getevents = "LIBAIO_0.4";
    pub const io_pgetevents = "LIBAIO_0.5";
    pub const io_cancel = "LIBAIO_0.4";
};

// ---------------------------------------------------------------------------
// Constants with WHY.
// ---------------------------------------------------------------------------

/// Contexts tracked at once: fio holds one ctx per job.
pub const CTX_CAP: usize = 256;
/// The deep/sparse regime boundary (default): re-counted A-B-B-A, write
/// 32×8 +14 % at 24 vs 2.
pub const REAP_EVENT_PARK_MAX_DEFAULT: u32 = 24;
/// The park's age bound in the deep regime = the shipped blind-sleep posture.
pub const REAP_BATCH_QUANTUM_US_DEFAULT: u32 = 50;
/// Kernel completions cannot wake a futex: a reap with kernel ops pending
/// re-checks every 1 ms; ring-only parks re-check at 5 ms (cross-thread
/// submits on sessions this reaper has not registered on).
pub const KERNEL_LANE_SLICE_NS: u64 = 1 * std.time.ns_per_ms;
pub const RING_PARK_RECHECK_NS: u64 = 5 * std.time.ns_per_ms;
/// `timeout == NULL` means forever in libaio; the reap bounds it so a dead
/// daemon (poison → −EIO events) can surface — the op deadline's shape.
pub const GETEVENTS_FOREVER_NS: u64 = 30 * std.time.ns_per_s;
/// Sweeps of `ticketDone` before parking in the sparse regime (sweep-
/// bounded, not iteration-bounded: 64 sweeps over qd32 was 2048 remote-
/// line probes per empty pass).
pub const SPARSE_SWEEPS: u32 = 4;
/// A racing submitter is the only thing that can change an empty pending
/// set: sleep this long before re-reading it.
pub const EMPTY_SLEEP_NS: u64 = 200 * std.time.ns_per_us;

pub const Knobs = struct {
    park_max: u32,
    quantum_ns: u64,
    sparse_marks: bool,

    pub fn fromEnv(env: *const std.process.Environ.Map) Knobs {
        return .{
            .park_max = knobs.intKnob("SQUEEZEFS_IL_REAP_PARK_MAX", u32, env),
            .quantum_ns = @as(u64, knobs.intKnob("SQUEEZEFS_IL_REAP_QUANTUM_US", u32, env)) * std.time.ns_per_us,
            .sparse_marks = knobs.boolKnob("SQUEEZEFS_IL_SPARSE_BATCH_MARKS", env),
        };
    }
};

/// k for the sparse regime: 1 for `pending ≤ 1` or marks off, else derived.
pub fn sparseParkK(pending: u32, marks: bool) u32 {
    if (pending <= 1 or !marks) return 1;
    return doorbell.reapBatchWakeThreshold(pending);
}

// ---------------------------------------------------------------------------
// The ring ticket a live iocb became.
// ---------------------------------------------------------------------------

pub const Live = struct {
    used: bool = false,
    session: u32 = 0,
    ticket: client.Ticket = .{ .slot = 0, .generation = 0 },
    /// `IoEvent.obj` / `.data`.
    iocb: u64 = 0,
    data: u64 = 0,
    buf: ?[*]u8 = null,
    len: u32 = 0,
    is_read: bool = false,
};

/// A ring-eligible iocb's target (resolved by the lanes' classification).
pub const RingTarget = struct {
    session: u32,
    binding: u64,
    is_read: bool,
};

// ---------------------------------------------------------------------------
// The context and its registry.
// ---------------------------------------------------------------------------

pub const Ctx = struct {
    lock: fd_table.Lock = .{},
    /// The `io_context_t` value; 0 = empty slot.
    id: Atomic(u64) = .init(0),
    destroyed: bool = false,
    live: []Live = &.{},
    ring_pending: u32 = 0,
    kernel_pending: u32 = 0,
    gpa: std.mem.Allocator = undefined,

    fn freeSlot(self: *Ctx) ?*Live {
        for (self.live) |*l| if (!l.used) return l;
        return null;
    }
};

pub const Registry = struct {
    /// The claim sentinel while a slot is being filled.
    const RESERVED: u64 = std.math.maxInt(u64);

    ctxs: [CTX_CAP]Ctx = @splat(.{}),
    /// Contexts the real libaio alone serves (registry full).
    overflowed: Atomic(u64) = .init(0),

    pub fn lookup(self: *Registry, id: io_context_t) ?*Ctx {
        if (id == 0) return null;
        for (&self.ctxs) |*c| if (c.id.load(.acquire) == id) return c;
        return null;
    }

    /// Vacate every entry for `id` (collision retro-neutralization: the
    /// kernel never hands out one live ctx twice, so a colliding register
    /// is a recycled ring address whose `io_destroy` bookkeeping was
    /// missed; the corpse must never own the new real context).
    pub fn remove(self: *Registry, id: io_context_t, lanes: anytype) void {
        for (&self.ctxs) |*c| {
            if (c.id.load(.acquire) != id) continue;
            c.lock.lock();
            defer c.lock.unlock();
            if (c.id.load(.acquire) != id) continue;
            destroyLocked(c, lanes);
            c.id.store(0, .release);
        }
    }

    /// Register a fresh context of `nr_events` tickets. False = at
    /// capacity or the table could not be allocated: the ctx passthroughs
    /// wholesale.
    pub fn register(self: *Registry, gpa: std.mem.Allocator, id: io_context_t, nr_events: u32, lanes: anytype) bool {
        self.remove(id, lanes);
        for (&self.ctxs) |*c| {
            if (c.id.cmpxchgStrong(0, RESERVED, .acq_rel, .acquire) != null) continue;
            c.lock.lock();
            defer c.lock.unlock();
            const live = gpa.alloc(Live, @max(nr_events, 1)) catch {
                c.id.store(0, .release);
                return false;
            };
            @memset(live, .{});
            c.destroyed = false;
            c.live = live;
            c.ring_pending = 0;
            c.kernel_pending = 0;
            c.gpa = gpa;
            c.id.store(id, .release);
            return true;
        }
        _ = self.overflowed.fetchAdd(1, .monotonic);
        return false;
    }

    pub fn count(self: *Registry) usize {
        var n: usize = 0;
        for (&self.ctxs) |*c| {
            const id = c.id.load(.acquire);
            if (id != 0 and id != RESERVED) n += 1;
        }
        return n;
    }
};

/// Destroy under the lock: abandon every ring pending (its slot is GC'd
/// with the session), free the table, mark destroyed.
fn destroyLocked(c: *Ctx, lanes: anytype) void {
    for (c.live) |*l| {
        if (l.used) {
            lanes.abandon(l);
            l.used = false;
        }
    }
    c.ring_pending = 0;
    if (c.live.len != 0) c.gpa.free(c.live);
    c.live = &.{};
    c.destroyed = true;
}

// ---------------------------------------------------------------------------
// The mixed-batch core, generic over the two lanes (tests inject fakes):
//   lanes.classify(iocb) ?RingTarget        — ring-eligible?
//   lanes.trySubmit(iocb, target, *Live) bool — the no-wait ticket (false = reroute)
//   lanes.submitRun(id, ios) c_int           — the real io_submit over a run
//   lanes.poll(*Live) ?i64                   — consumes a DONE ticket
//   lanes.done(*Live) bool                   — non-consuming probe
//   lanes.kernelReap(id, events, min, timeout_ns) c_int — the real io_getevents
//   lanes.park(ctx, pending, kernel_pending, bound_ns) void — the doorbell park
//   lanes.abandon(*Live) void
// ---------------------------------------------------------------------------

pub const SubmitVerdict = union(enum) {
    /// Every iocb is kernel-class: the real call verbatim.
    all_kernel,
    /// The batch's accepted prefix.
    accepted: c_int,
    /// A kernel errno on the FIRST op of the batch.
    errno: c_int,
};

pub fn submitBatch(c: *Ctx, id: io_context_t, ios: [*]const ?*Iocb, nr: usize, lanes: anytype) SubmitVerdict {
    // Classify first: a pure-kernel batch never takes the lock.
    var any_ring = false;
    var i: usize = 0;
    while (i < nr) : (i += 1) {
        const iocb = ios[i] orelse continue;
        if (lanes.classify(iocb) != null) {
            any_ring = true;
            break;
        }
    }
    if (!any_ring) return .all_kernel;
    c.lock.lock();
    defer c.lock.unlock();
    if (c.destroyed) return .all_kernel;
    var accepted: c_int = 0;
    var run_start: ?usize = null;
    i = 0;
    while (i < nr) : (i += 1) {
        const iocb = ios[i] orelse {
            // A NULL entry: the real call faults it — kernel class.
            if (run_start == null) run_start = i;
            continue;
        };
        const target = lanes.classify(iocb);
        var ring_ok = false;
        if (target) |t| {
            if (c.freeSlot()) |slot| {
                // The run before this ring op dispatches FIRST.
                if (run_start) |rs| {
                    switch (flushRun(c, id, ios, rs, i, accepted, lanes)) {
                        .continue_at => |acc| accepted = acc,
                        .stop => |v| return v,
                    }
                    run_start = null;
                }
                if (lanes.trySubmit(iocb, t, slot)) {
                    slot.used = true;
                    slot.iocb = @intFromPtr(iocb);
                    slot.data = iocb.data;
                    c.ring_pending += 1;
                    accepted += 1;
                    ring_ok = true;
                }
            }
        }
        if (!ring_ok) {
            // Kernel-class, or a ring refusal REROUTED at its batch position.
            if (run_start == null) run_start = i;
        }
    }
    if (run_start) |rs| {
        switch (flushRun(c, id, ios, rs, nr, accepted, lanes)) {
            .continue_at => |acc| accepted = acc,
            .stop => |v| return v,
        }
    }
    return .{ .accepted = accepted };
}

const RunOutcome = union(enum) { continue_at: c_int, stop: SubmitVerdict };

fn flushRun(c: *Ctx, id: io_context_t, ios: [*]const ?*Iocb, from: usize, to: usize, accepted: c_int, lanes: anytype) RunOutcome {
    const len = to - from;
    const r = lanes.submitRun(id, ios[from..to]);
    if (r < 0) {
        if (accepted == 0) return .{ .stop = .{ .errno = r } };
        return .{ .stop = .{ .accepted = accepted } };
    }
    c.kernel_pending += @intCast(r);
    const acc = accepted + r;
    if (@as(usize, @intCast(r)) < len) return .{ .stop = .{ .accepted = acc } };
    return .{ .continue_at = acc };
}

/// Harvest every DONE ring ticket into `events[got..]` under the held lock.
fn harvestLocked(c: *Ctx, events: []IoEvent, got: usize, lanes: anytype) usize {
    var n = got;
    for (c.live) |*l| {
        if (n >= events.len) break;
        if (!l.used) continue;
        const res = lanes.poll(l) orelse continue;
        events[n] = .{ .data = l.data, .obj = l.iocb, .res = res, .res2 = 0 };
        n += 1;
        l.used = false;
        c.ring_pending -= 1;
    }
    return n;
}

pub const ReapVerdict = union(enum) {
    /// Nothing tracked / destroyed: the real call verbatim.
    real,
    got: c_int,
};

/// `io_getevents` over a registered context (§6.6).
pub fn reap(c: *Ctx, id: io_context_t, min_nr: usize, events: []IoEvent, timeout_ns: u64, k: Knobs, lanes: anytype) ReapVerdict {
    const deadline = clock.monotonicNs() + timeout_ns;
    var got: usize = 0;
    while (true) {
        var ring_pending: u32 = 0;
        var kernel_pending: u32 = 0;
        {
            c.lock.lock();
            defer c.lock.unlock();
            if (c.destroyed) return if (got == 0) .real else .{ .got = @intCast(got) };
            if (got == 0 and c.ring_pending == 0 and c.kernel_pending == 0) return .real;
            got = harvestLocked(c, events, got, lanes);
            if (c.kernel_pending > 0 and got < events.len) {
                // Non-blocking kernel drain while ring ops are pending; the
                // remaining budget when only kernel ops are.
                const now = clock.monotonicNs();
                const budget: u64 = if (c.ring_pending == 0 and got < min_nr) (if (deadline > now) @min(deadline - now, KERNEL_LANE_SLICE_NS * 50) else 0) else 0;
                const kmin: usize = if (c.ring_pending == 0) @min(min_nr -| got, @min(c.kernel_pending, events.len - got)) else 0;
                const r = lanes.kernelReap(id, events[got..], kmin, budget);
                if (r > 0) {
                    got += @intCast(r);
                    c.kernel_pending -|= @intCast(r);
                } else if (r < 0 and got == 0 and c.ring_pending == 0) {
                    return .{ .got = r };
                }
            }
            ring_pending = c.ring_pending;
            kernel_pending = c.kernel_pending;
        }
        if (got >= min_nr or got >= events.len) return .{ .got = @intCast(got) };
        const now = clock.monotonicNs();
        if (now >= deadline) return .{ .got = @intCast(got) };
        const remaining = deadline - now;
        if (ring_pending == 0 and kernel_pending == 0) {
            sys.nanosleep(@min(EMPTY_SLEEP_NS, remaining));
            continue;
        }
        if (ring_pending == 0) continue; // the kernel lane waited inside the pass
        // The park: deep regime by the batch mark; sparse by sweeps then k.
        var bound: u64 = if (kernel_pending > 0) KERNEL_LANE_SLICE_NS else RING_PARK_RECHECK_NS;
        var mark: u32 = 1;
        if (ring_pending > k.park_max) {
            mark = doorbell.reapBatchWakeThreshold(ring_pending);
        } else {
            var sweep: u32 = 0;
            var ready = false;
            while (sweep < SPARSE_SWEEPS and !ready) : (sweep += 1) {
                c.lock.lock();
                for (c.live) |*l| {
                    if (l.used and lanes.done(l)) {
                        ready = true;
                        break;
                    }
                }
                c.lock.unlock();
                std.atomic.spinLoopHint();
            }
            if (ready) continue;
            mark = sparseParkK(ring_pending, k.sparse_marks);
        }
        if (mark > 1) bound = @min(bound, k.quantum_ns);
        lanes.park(c, mark, @min(bound, remaining));
    }
}

// ---------------------------------------------------------------------------
// The real lanes: the fd table + sessions on the ring side, libaio's own
// functions on the kernel side.
// ---------------------------------------------------------------------------

pub const RealFns = struct {
    io_submit: ?*const fn (io_context_t, c_long, [*]const ?*Iocb) callconv(.c) c_int,
    io_getevents: ?*const fn (io_context_t, c_long, c_long, [*]IoEvent, ?*linux.timespec) callconv(.c) c_int,
};

pub const RealLanes = struct {
    st: *State,
    fns: RealFns,

    pub fn classify(self: *RealLanes, iocb: *const Iocb) ?RingTarget {
        if (iocb.flags & IOCB_FLAG_RESFD != 0) return null;
        if (ops.rwfPassthrough(iocb.aio_rw_flags)) return null;
        if (iocb.nbytes == 0 or iocb.offset < 0 or iocb.buf == 0) return null;
        const is_read = switch (iocb.aio_lio_opcode) {
            IOCB_CMD_PREAD => true,
            IOCB_CMD_PWRITE => false,
            else => return null,
        };
        const pin = self.st.table.lookup(iocb.aio_fildes) orelse return null;
        defer if (pin.unpin()) |d| self.st.release(d);
        const b = pin.cell.binding;
        if ((is_read and !b.rights.read) or (!is_read and !b.rights.write)) return null;
        const s = self.st.sessionAt(b.session);
        // One async op = one slot (a multi-slot op would hold slots hostage);
        // the hybrid gate applies per op like every positional form.
        if (iocb.nbytes > s.slab()) return null;
        if (s.routesKernel(iocb.nbytes)) {
            s.noteKernelRoute(iocb.nbytes);
            classify_mod.note(.lane_gate, s);
            return null;
        }
        return .{ .session = b.session, .binding = b.id, .is_read = is_read };
    }

    pub fn trySubmit(self: *RealLanes, iocb: *const Iocb, t: RingTarget, slot: *Live) bool {
        const s = self.st.sessionAt(t.session);
        const len: u32 = @intCast(iocb.nbytes);
        const buf: [*]u8 = @ptrFromInt(@as(usize, @intCast(iocb.buf)));
        const ticket = s.submitNoWait(if (t.is_read) .read else .write, t.binding, @intCast(iocb.offset), len, if (t.is_read) null else buf[0..len]) orelse {
            if (!s.poisoned()) classify_mod.note(.no_slot_or_shape, s) else classify_mod.note(.poisoned, s);
            return false;
        };
        slot.* = .{ .used = false, .session = t.session, .ticket = ticket, .buf = buf, .len = len, .is_read = t.is_read };
        return true;
    }

    pub fn submitRun(self: *RealLanes, id: io_context_t, ios: []const ?*Iocb) c_int {
        const f = self.fns.io_submit orelse return -@as(c_int, @backingInt(linux.E.NOSYS));
        return f(id, @intCast(ios.len), ios.ptr);
    }

    pub fn poll(self: *RealLanes, l: *Live) ?i64 {
        const s = self.st.sessionAt(l.session);
        if (s.poisoned() and !s.ticketDone(l.ticket)) {
            // A poisoned session resolves in-flight tickets as −EIO events
            // (never stranded); the slot is never recycled.
            s.abandonTicket(l.ticket);
            return -@as(i64, @backingInt(linux.E.IO));
        }
        return s.pollTicket(l.ticket, if (l.is_read) (if (l.buf) |b| b[0..l.len] else null) else null);
    }

    pub fn done(self: *RealLanes, l: *Live) bool {
        const s = self.st.sessionAt(l.session);
        return s.ticketDone(l.ticket) or s.poisoned();
    }

    pub fn kernelReap(self: *RealLanes, id: io_context_t, events: []IoEvent, min: usize, timeout_ns: u64) c_int {
        const f = self.fns.io_getevents orelse return -@as(c_int, @backingInt(linux.E.NOSYS));
        var ts: linux.timespec = .{ .sec = @intCast(timeout_ns / std.time.ns_per_s), .nsec = @intCast(timeout_ns % std.time.ns_per_s) };
        return f(id, @intCast(min), @intCast(events.len), events.ptr, &ts);
    }

    /// Register on every distinct session with `mark`, re-scan, wait, end.
    pub fn park(self: *RealLanes, c: *Ctx, mark: u32, bound_ns: u64) void {
        var entries: [client.WAIT_ANY_MAX]client.WaitEntry = undefined;
        var n: usize = 0;
        c.lock.lock();
        for (c.live) |*l| {
            if (!l.used) continue;
            const s = self.st.sessionAt(l.session);
            var seen = false;
            for (entries[0..n]) |e| {
                if (e.session == s) seen = true;
            }
            if (seen or n == entries.len) continue;
            entries[n] = .{ .session = s, .expected = s.cqeParkBeginBatch(mark) };
            n += 1;
        }
        // The mandatory re-scan before sleeping.
        var ready = false;
        for (c.live) |*l| {
            if (l.used and self.done(l)) {
                ready = true;
                break;
            }
        }
        c.lock.unlock();
        if (n == 0) {
            sys.nanosleep(@min(bound_ns, EMPTY_SLEEP_NS));
            return;
        }
        if (!ready) _ = Session.waitAny(entries[0..n], bound_ns);
        for (entries[0..n]) |e| e.session.cqeParkEnd();
    }

    pub fn abandon(self: *RealLanes, l: *Live) void {
        self.st.sessionAt(l.session).abandonTicket(l.ticket);
    }
};

// ---------------------------------------------------------------------------
// The bodies.
// ---------------------------------------------------------------------------

pub var registry: Registry = .{};

/// The reap knobs, read once per process at the first `io_setup` (the
/// reap is a hot path; the env map is not).
var knobs_cache: Knobs = undefined;
var knobs_ready: Atomic(bool) = .init(false);

fn reapKnobs(st: *State) Knobs {
    if (!knobs_ready.load(.acquire)) {
        knobs_cache = Knobs.fromEnv(&st.env);
        knobs_ready.store(true, .release);
    }
    return knobs_cache;
}

/// `io_setup` after a successful real call.
pub fn setupPost(st: *State, nr_events: c_int, ctxp: *io_context_t, fns: RealFns) void {
    var lanes: RealLanes = .{ .st = st, .fns = fns };
    _ = reapKnobs(st);
    _ = registry.register(std.heap.c_allocator, ctxp.*, @intCast(@max(nr_events, 1)), &lanes);
}

/// `io_destroy` BEFORE the real call, unconditionally (a skipped vacate is
/// the field crash class).
pub fn destroyPre(st: ?*State, id: io_context_t, fns: RealFns) void {
    var lanes: RealLanes = .{ .st = st orelse return, .fns = fns };
    registry.remove(id, &lanes);
}

pub fn submit(st: *State, id: io_context_t, nr: c_long, ios: ?[*]const ?*Iocb, fns: RealFns) ops.Verdict(c_int) {
    if (nr <= 0 or ios == null) return .real;
    const c = registry.lookup(id) orelse return .real;
    var lanes: RealLanes = .{ .st = st, .fns = fns };
    return switch (submitBatch(c, id, ios.?, @intCast(nr), &lanes)) {
        .all_kernel => .real,
        .accepted => |n| .{ .done = n },
        .errno => |e| .{ .done = e },
    };
}

/// `io_getevents`/`io_pgetevents` (the sigmask is NOT applied during the
/// shim's own waits — strictly more wakeful).
pub fn getevents(st: *State, id: io_context_t, min_nr: c_long, nr: c_long, events: ?[*]IoEvent, timeout: ?*const linux.timespec, fns: RealFns) ops.Verdict(c_int) {
    if (nr <= 0 or events == null) return .real;
    const c = registry.lookup(id) orelse return .real;
    var lanes: RealLanes = .{ .st = st, .fns = fns };
    const timeout_ns: u64 = if (timeout) |t| timespecNs(t) else GETEVENTS_FOREVER_NS;
    const k = reapKnobs(st);
    const min: usize = @intCast(@max(min_nr, 0));
    return switch (reap(c, id, min, events.?[0..@intCast(nr)], timeout_ns, k, &lanes)) {
        .real => .real,
        .got => |n| .{ .done = n },
    };
}

fn timespecNs(t: *const linux.timespec) u64 {
    if (t.sec < 0 or t.nsec < 0) return 0;
    return @as(u64, @intCast(t.sec)) *| std.time.ns_per_s +| @as(u64, @intCast(t.nsec));
}

/// `io_cancel`: a ring-pending iocb cannot be recalled ⇒ −EINPROGRESS;
/// else the real call.
pub fn cancel(id: io_context_t, iocb: ?*const Iocb) ops.Verdict(c_int) {
    const c = registry.lookup(id) orelse return .real;
    const target = @intFromPtr(iocb orelse return .real);
    c.lock.lock();
    defer c.lock.unlock();
    for (c.live) |*l| {
        if (l.used and l.iocb == target) return .{ .done = -@as(c_int, @backingInt(linux.E.INPROGRESS)) };
    }
    return .real;
}

// ---------------------------------------------------------------------------
// Tests: the core over fake lanes.
// ---------------------------------------------------------------------------

const testing = std.testing;

/// Fake lanes: fds ≥ 100 are "bound" (ring-eligible PREAD/PWRITE); the
/// kernel lane records runs and completes on demand; ring tickets complete
/// when the test says so.
const FakeLanes = struct {
    /// Per ring ticket: completed with this result (null = in flight).
    results: [64]?i64 = @splat(null),
    next_ticket: u32 = 0,
    /// The kernel lane's runs (`fd`s), its acceptance cap per run, its
    /// first-op errno.
    runs: [16][8]i32 = @splat(@splat(0)),
    run_lens: [16]usize = @splat(0),
    n_runs: usize = 0,
    accept_cap: usize = 8,
    first_errno: c_int = 0,
    kernel_done: [64]i32 = @splat(0),
    n_kernel_done: usize = 0,
    /// Ring refusals (reroute) for the next N submits.
    refuse_next: u32 = 0,
    parks: u32 = 0,
    abandoned: u32 = 0,

    pub fn classify(_: *FakeLanes, iocb: *const Iocb) ?RingTarget {
        if (iocb.aio_fildes < 100) return null;
        if (iocb.flags & IOCB_FLAG_RESFD != 0) return null;
        if (ops.rwfPassthrough(iocb.aio_rw_flags)) return null;
        if (iocb.nbytes == 0 or iocb.nbytes > 65536) return null;
        return switch (iocb.aio_lio_opcode) {
            IOCB_CMD_PREAD => .{ .session = 0, .binding = 1, .is_read = true },
            IOCB_CMD_PWRITE => .{ .session = 0, .binding = 1, .is_read = false },
            else => null,
        };
    }
    pub fn trySubmit(self: *FakeLanes, iocb: *const Iocb, t: RingTarget, slot: *Live) bool {
        if (self.refuse_next > 0) {
            self.refuse_next -= 1;
            return false;
        }
        slot.* = .{ .session = 0, .ticket = .{ .slot = self.next_ticket, .generation = 1 }, .len = @intCast(iocb.nbytes), .is_read = t.is_read };
        self.next_ticket += 1;
        return true;
    }
    pub fn submitRun(self: *FakeLanes, _: io_context_t, ios: []const ?*Iocb) c_int {
        if (self.first_errno != 0) {
            const e = self.first_errno;
            self.first_errno = 0;
            return e;
        }
        const n = @min(ios.len, self.accept_cap);
        for (ios[0..n], 0..) |io, i| self.runs[self.n_runs][i] = io.?.aio_fildes;
        self.run_lens[self.n_runs] = n;
        self.n_runs += 1;
        for (ios[0..n]) |io| {
            self.kernel_done[self.n_kernel_done] = io.?.aio_fildes;
            self.n_kernel_done += 1;
        }
        return @intCast(n);
    }
    pub fn poll(self: *FakeLanes, l: *Live) ?i64 {
        return self.results[l.ticket.slot];
    }
    pub fn done(self: *FakeLanes, l: *Live) bool {
        return self.results[l.ticket.slot] != null;
    }
    pub fn kernelReap(self: *FakeLanes, _: io_context_t, events: []IoEvent, _: usize, _: u64) c_int {
        var n: usize = 0;
        while (n < events.len and self.n_kernel_done > 0) : (n += 1) {
            self.n_kernel_done -= 1;
            events[n] = .{ .data = 7, .obj = @intCast(self.kernel_done[self.n_kernel_done]), .res = 4096, .res2 = 0 };
        }
        return @intCast(n);
    }
    pub fn park(self: *FakeLanes, _: *Ctx, _: u32, _: u64) void {
        self.parks += 1;
        // Complete every pending ring ticket on the first park.
        for (&self.results, 0..) |*r, i| {
            if (i < self.next_ticket and r.* == null) r.* = 512;
        }
    }
    pub fn abandon(self: *FakeLanes, _: *Live) void {
        self.abandoned += 1;
    }
};

fn mkIocb(fd: i32, op: u16, nbytes: u64) Iocb {
    return .{ .data = @intCast(fd), .key = 0, .aio_rw_flags = 0, .aio_lio_opcode = op, .aio_reqprio = 0, .aio_fildes = fd, .buf = 0x1000, .nbytes = nbytes, .offset = 0, .__pad3 = 0, .flags = 0, .resfd = 0 };
}

const TestCtx = struct {
    reg: Registry = .{},
    lanes: FakeLanes = .{},
    fn ctx(self: *TestCtx) *Ctx {
        return self.reg.lookup(0xC0FFEE).?;
    }
};

test "a mixed batch dispatches in order: kernel runs flush before each ring op, the whole batch is accepted, and the reap merges ring first then kernel" {
    const t = try testing.allocator.create(TestCtx);
    defer testing.allocator.destroy(t);
    t.* = .{};
    try testing.expect(t.reg.register(testing.allocator, 0xC0FFEE, 32, &t.lanes));
    defer t.reg.remove(0xC0FFEE, &t.lanes);
    var a = mkIocb(3, IOCB_CMD_PREAD, 4096); // kernel
    var b = mkIocb(4, IOCB_CMD_PWRITE, 4096); // kernel
    var r1 = mkIocb(100, IOCB_CMD_PREAD, 4096); // ring
    var k = mkIocb(5, IOCB_CMD_PREAD, 4096); // kernel
    var r2 = mkIocb(101, IOCB_CMD_PWRITE, 512); // ring
    const ios = [_]?*Iocb{ &a, &b, &r1, &k, &r2 };
    const v = submitBatch(t.ctx(), 0xC0FFEE, &ios, ios.len, &t.lanes);
    try testing.expectEqual(SubmitVerdict{ .accepted = 5 }, v);
    try testing.expectEqual(@as(usize, 2), t.lanes.n_runs);
    try testing.expectEqual(@as(usize, 2), t.lanes.run_lens[0]);
    try testing.expectEqual(@as(i32, 3), t.lanes.runs[0][0]);
    try testing.expectEqual(@as(i32, 4), t.lanes.runs[0][1]);
    try testing.expectEqual(@as(i32, 5), t.lanes.runs[1][0]);
    try testing.expectEqual(@as(u32, 2), t.ctx().ring_pending);
    try testing.expectEqual(@as(u32, 3), t.ctx().kernel_pending);
    // Reap: nothing ring-done yet ⇒ the kernel drain fills, then a park
    // completes the ring ops; min 5 is met in one call.
    var events: [8]IoEvent = undefined;
    const k_opts: Knobs = .{ .park_max = 24, .quantum_ns = 50_000, .sparse_marks = true };
    switch (reap(t.ctx(), 0xC0FFEE, 5, &events, std.time.ns_per_s, k_opts, &t.lanes)) {
        .got => |n| try testing.expectEqual(@as(c_int, 5), n),
        .real => return error.TestUnexpectedResult,
    }
    try testing.expect(t.lanes.parks >= 1);
    var ring_events: u32 = 0;
    for (events[0..5]) |e| {
        if (e.res == 512) {
            ring_events += 1;
            try testing.expect(e.obj == @intFromPtr(&r1) or e.obj == @intFromPtr(&r2));
        }
    }
    try testing.expectEqual(@as(u32, 2), ring_events);
    try testing.expectEqual(@as(u32, 0), t.ctx().ring_pending);
    // Nothing tracked ⇒ the real call.
    try testing.expect(reap(t.ctx(), 0xC0FFEE, 1, &events, 0, k_opts, &t.lanes) == .real);
}

test "a partial kernel acceptance truncates the prefix before later ring ops; a kernel errno on the first op propagates; later it answers the accepted prefix" {
    const t = try testing.allocator.create(TestCtx);
    defer testing.allocator.destroy(t);
    t.* = .{};
    try testing.expect(t.reg.register(testing.allocator, 0xC0FFEE, 32, &t.lanes));
    defer t.reg.remove(0xC0FFEE, &t.lanes);
    var a = mkIocb(3, IOCB_CMD_PREAD, 4096);
    var b = mkIocb(4, IOCB_CMD_PREAD, 4096);
    var r1 = mkIocb(100, IOCB_CMD_PREAD, 4096);
    const ios = [_]?*Iocb{ &a, &b, &r1 };
    t.lanes.accept_cap = 1;
    try testing.expectEqual(SubmitVerdict{ .accepted = 1 }, submitBatch(t.ctx(), 0xC0FFEE, &ios, ios.len, &t.lanes));
    try testing.expectEqual(@as(u32, 0), t.ctx().ring_pending);
    t.lanes.accept_cap = 8;
    t.lanes.first_errno = -22;
    try testing.expectEqual(SubmitVerdict{ .errno = -22 }, submitBatch(t.ctx(), 0xC0FFEE, &ios, ios.len, &t.lanes));
    // Ring first, then a failing kernel run: the accepted prefix.
    var r0 = mkIocb(100, IOCB_CMD_PREAD, 4096);
    const ios2 = [_]?*Iocb{ &r0, &a };
    t.lanes.first_errno = -22;
    try testing.expectEqual(SubmitVerdict{ .accepted = 1 }, submitBatch(t.ctx(), 0xC0FFEE, &ios2, ios2.len, &t.lanes));
}

test "a ring refusal reroutes the iocb into the kernel lane at its position (never −EAGAIN); a pure-kernel batch is the real call; only PREAD/PWRITE on bound fds inside the slab without RESFD or foreign RWF flags classify ring" {
    const t = try testing.allocator.create(TestCtx);
    defer testing.allocator.destroy(t);
    t.* = .{};
    try testing.expect(t.reg.register(testing.allocator, 0xC0FFEE, 32, &t.lanes));
    defer t.reg.remove(0xC0FFEE, &t.lanes);
    var r1 = mkIocb(100, IOCB_CMD_PREAD, 4096);
    var k = mkIocb(3, IOCB_CMD_PREAD, 4096);
    const ios = [_]?*Iocb{ &r1, &k };
    t.lanes.refuse_next = 1;
    try testing.expectEqual(SubmitVerdict{ .accepted = 2 }, submitBatch(t.ctx(), 0xC0FFEE, &ios, ios.len, &t.lanes));
    try testing.expectEqual(@as(usize, 1), t.lanes.n_runs);
    try testing.expectEqual(@as(usize, 2), t.lanes.run_lens[0]);
    try testing.expectEqual(@as(i32, 100), t.lanes.runs[0][0]);
    try testing.expectEqual(@as(u32, 0), t.ctx().ring_pending);
    var k2 = mkIocb(4, IOCB_CMD_PWRITE, 4096);
    const pure = [_]?*Iocb{ &k, &k2 };
    try testing.expect(submitBatch(t.ctx(), 0xC0FFEE, &pure, pure.len, &t.lanes) == .all_kernel);
    // The screen.
    try testing.expect(t.lanes.classify(&mkIocb(100, IOCB_CMD_PREAD, 4096)) != null);
    try testing.expect(t.lanes.classify(&mkIocb(3, IOCB_CMD_PREAD, 4096)) == null);
    try testing.expect(t.lanes.classify(&mkIocb(100, 7, 4096)) == null);
    try testing.expect(t.lanes.classify(&mkIocb(100, IOCB_CMD_PREAD, 0)) == null);
    try testing.expect(t.lanes.classify(&mkIocb(100, IOCB_CMD_PREAD, 1 << 20)) == null);
    var resfd = mkIocb(100, IOCB_CMD_PREAD, 4096);
    resfd.flags = IOCB_FLAG_RESFD;
    try testing.expect(t.lanes.classify(&resfd) == null);
    var rwf = mkIocb(100, IOCB_CMD_PREAD, 4096);
    rwf.aio_rw_flags = linux.RWF.NOWAIT;
    try testing.expect(t.lanes.classify(&rwf) == null);
    rwf.aio_rw_flags = linux.RWF.HIPRI;
    try testing.expect(t.lanes.classify(&rwf) != null);
}

test "min 0 never blocks and returns whatever is ready; a zero timeout is one non-blocking pass; destroy abandons ring pendings and the destroyed context is the real call; io_cancel answers −EINPROGRESS for a ring-pending iocb" {
    const t = try testing.allocator.create(TestCtx);
    defer testing.allocator.destroy(t);
    t.* = .{};
    try testing.expect(t.reg.register(testing.allocator, 0xC0FFEE, 4, &t.lanes));
    var r1 = mkIocb(100, IOCB_CMD_PREAD, 4096);
    var r2 = mkIocb(101, IOCB_CMD_PREAD, 4096);
    const ios = [_]?*Iocb{ &r1, &r2 };
    try testing.expectEqual(SubmitVerdict{ .accepted = 2 }, submitBatch(t.ctx(), 0xC0FFEE, &ios, ios.len, &t.lanes));
    var events: [4]IoEvent = undefined;
    const k_opts: Knobs = .{ .park_max = 24, .quantum_ns = 50_000, .sparse_marks = true };
    switch (reap(t.ctx(), 0xC0FFEE, 0, &events, 0, k_opts, &t.lanes)) {
        .got => |n| try testing.expectEqual(@as(c_int, 0), n),
        .real => return error.TestUnexpectedResult,
    }
    try testing.expectEqual(@as(u32, 0), t.lanes.parks);
    // One ticket completes: min 1 returns it without parking.
    t.lanes.results[0] = 4096;
    switch (reap(t.ctx(), 0xC0FFEE, 1, &events, std.time.ns_per_s, k_opts, &t.lanes)) {
        .got => |n| {
            try testing.expectEqual(@as(c_int, 1), n);
            try testing.expectEqual(@intFromPtr(&r1), events[0].obj);
            try testing.expectEqual(@as(u64, 100), events[0].data);
        },
        .real => return error.TestUnexpectedResult,
    }
    try testing.expectEqual(@as(u32, 0), t.lanes.parks);
    // Error completions surface the negative errno in res.
    t.lanes.results[1] = -5;
    switch (reap(t.ctx(), 0xC0FFEE, 1, &events, std.time.ns_per_s, k_opts, &t.lanes)) {
        .got => |n| {
            try testing.expectEqual(@as(c_int, 1), n);
            try testing.expectEqual(@as(i64, -5), events[0].res);
        },
        .real => return error.TestUnexpectedResult,
    }
    // Destroy with a pending ring op abandons it.
    var r3 = mkIocb(100, IOCB_CMD_PREAD, 4096);
    const ios3 = [_]?*Iocb{&r3};
    try testing.expectEqual(SubmitVerdict{ .accepted = 1 }, submitBatch(t.ctx(), 0xC0FFEE, &ios3, 1, &t.lanes));
    const c = t.ctx();
    c.lock.lock();
    var found = false;
    for (c.live) |*l| {
        if (l.used and l.iocb == @intFromPtr(&r3)) found = true;
    }
    c.lock.unlock();
    try testing.expect(found);
    t.reg.remove(0xC0FFEE, &t.lanes);
    try testing.expectEqual(@as(u32, 1), t.lanes.abandoned);
    try testing.expect(t.reg.lookup(0xC0FFEE) == null);
    // Re-registration of the value is fresh state; a duplicate register
    // retro-neutralizes rather than stacking.
    try testing.expect(t.reg.register(testing.allocator, 0xC0FFEE, 4, &t.lanes));
    try testing.expect(t.reg.register(testing.allocator, 0xC0FFEE, 4, &t.lanes));
    try testing.expectEqual(@as(usize, 1), t.reg.count());
    t.reg.remove(0xC0FFEE, &t.lanes);
}

test "the registry refuses past its capacity and degrades to passthrough; sparse_park_k derives and gates; the knob rows read their defaults" {
    const t = try testing.allocator.create(TestCtx);
    defer testing.allocator.destroy(t);
    t.* = .{};
    var i: u64 = 1;
    while (i <= CTX_CAP) : (i += 1) try testing.expect(t.reg.register(testing.allocator, i, 1, &t.lanes));
    try testing.expect(!t.reg.register(testing.allocator, CTX_CAP + 1, 1, &t.lanes));
    try testing.expectEqual(@as(u64, 1), t.reg.overflowed.load(.monotonic));
    i = 1;
    while (i <= CTX_CAP) : (i += 1) t.reg.remove(i, &t.lanes);
    try testing.expectEqual(@as(usize, 0), t.reg.count());
    try testing.expectEqual(@as(u32, 1), sparseParkK(0, true));
    try testing.expectEqual(@as(u32, 1), sparseParkK(1, true));
    try testing.expectEqual(@as(u32, 2), sparseParkK(8, true));
    try testing.expectEqual(@as(u32, 4), sparseParkK(16, true));
    try testing.expectEqual(@as(u32, 1), sparseParkK(16, false));
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    const k = Knobs.fromEnv(&env);
    try testing.expectEqual(REAP_EVENT_PARK_MAX_DEFAULT, k.park_max);
    try testing.expectEqual(@as(u64, REAP_BATCH_QUANTUM_US_DEFAULT) * std.time.ns_per_us, k.quantum_ns);
    try testing.expect(k.sparse_marks);
}
