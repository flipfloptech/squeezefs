//! The ONE ring-park primitive (decision 0003 WP-C rows 12–13; charter §4
//! wake law, §9 "parks IN its ring"): an io_uring a lane thread owns and
//! parks in, woken by an eventfd READ it keeps armed, so a task wake and a
//! device CQE are ONE `io_uring_enter` wait. Linux only.
//!
//! Every ring-owning lane (`journal_lane`, rung 3's `sqz-fuse-ur<qid>`
//! workers, the NVMe lanes) embeds a parked ring and keeps its own slot
//! table; the primitive owns exactly the protocol they shared three times
//! over in rung 1:
//!
//!   - the wake pair (modelled in `nvme_derive.WakeCoalescer`):
//!       producer: publish (queue push) → `coalescer.arm()` → eventfd write
//!                 iff arm returned true;
//!       lane:     wake CQE consumed → `service()` disarms → scan the queue.
//!     Both RMWs are SeqCst on one word, so N publishes between two lane
//!     passes cost ONE eventfd write and no push is ever missed;
//!   - the armed READ at `user_data = wake_ud` (`u64::MAX`), level-triggered
//!     on a non-blocking eventfd, re-armed by `armWake` only after it
//!     completed;
//!   - the park: SQEs go in their own `submit()` FIRST, then a timed wait with
//!     `to_submit = 0` through EXT_ARG. WHY: when `io_uring_enter` submits
//!     anything it returns the submitted count and DISCARDS the wait's
//!     verdict, so a combined submit-and-timed-wait never reports ETIME
//!     (found red-first in rung 1; liburing waits in a separate call for the
//!     same reason). An untimed park is one `submit_and_wait(1)`;
//!   - the dead-ring park: once the ring broke nothing on it completes, so
//!     the lane parks on the eventfd alone (`poll(2)` — the failure drain,
//!     never a product I/O path) until a message, a kick or the tick;
//!   - teardown: cancel the armed READ and reap its CQE BEFORE the ring goes
//!     away so no in-kernel completion lands in `wake_buf` after the lane freed
//!     it.
//!
//! Fixed file 0 (`registerFile`) and `prepFdatasync` are here so a lane whose
//! barrier is an FSYNC on the same ring (the journal lane) lives on it too.
//!
//! The protocol is written ONCE, generic over the ring binding beneath it
//! (`Parked(Face)`). `ParkedRing` is the protocol over std's 64 B
//! `IoUring` (`StdFace`): the journal lane, the NVMe lanes and the
//! sideband/INIT ring. The transport workers' SQE128/CQE32 ring
//! (`io/uring128.zig`, decision 0003 §D "SQE128") brings its own face and
//! gets the same body: nothing in the wake/park/dead logic exists twice
//! (decision 0002 row 12). A face is a comptime namespace — the ring type,
//! its SQE/CQE types and the handful of calls the protocol makes — so the
//! indirection costs nothing at run time.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const IoUring = linux.IoUring;
const Allocator = std.mem.Allocator;

const log = @import("../core/log.zig");
const errors = @import("../core/errors.zig");
const nd = @import("nvme_derive.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("ring_park is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

/// `user_data` of the armed wake READ: never a slot index (slots are small
/// integers).
pub const wake_ud: u64 = std.math.maxInt(u64);
/// Reserved beside it: the cancel tag of the wake READ and a seam's linked
/// timeout; anything `>= reserved_ud` is never a request.
pub const reserved_ud: u64 = std.math.maxInt(u64) - 1;

pub const Verdict = enum {
    /// A CQE is ready (a device completion or the wake READ) — or the ring
    /// died, which the owner observes through `isDead`.
    event,
    /// The tick elapsed first.
    tick,
};

pub const InitError = error{ OutOfMemory, RingSetupFailed };

/// The cross-thread wake of a parked ring, always THROUGH the coalescer.
pub const Wake = struct {
    fd: linux.fd_t,
    coalescer: *nd.WakeCoalescer,

    /// True ⇒ this call paid the eventfd write; false ⇒ elided (a wake is
    /// already pending and its consumer will scan after this caller's push).
    pub fn wake(self: Wake) bool {
        if (!self.coalescer.arm()) return false;
        writeEventfd(self.fd);
        return true;
    }

    /// Unconditional (teardown, a kick): the lane parks on the ring, so a
    /// sealed queue is only observable through the wake.
    pub fn force(self: Wake) void {
        writeEventfd(self.fd);
    }
};

fn writeEventfd(fd: linux.fd_t) void {
    const one: u64 = 1;
    // EAGAIN (counter at u64::MAX−1) is impossible at one increment per
    // wake and harmless: the park is already pending.
    _ = linux.write(fd, @ptrCast(&one), @sizeOf(u64));
}

/// The park/wake/dead protocol over any ring binding that presents this
/// face (a comptime namespace):
///
///   - `Ring` with fields `fd: fd_t` and `features: u32` (the raw EXT_ARG
///     wait enters on them); `Sqe` with fields `flags: u8`, `user_data: u64`;
///     `Cqe` with field `user_data: u64`;
///   - `setup(entries: u16, flags: u32) !Ring`, `deinit(*Ring) void`;
///   - `getSqe(*Ring) ?*Sqe` — the raw take, null when the SQ is full;
///   - `submit(*Ring) errors.Error!u32`, `submitAndWait(*Ring, u32) errors.Error!u32`
///     (EINTR is `error.Interrupted`), `copyCqes(*Ring, []Cqe, u32) !u32`;
///   - `registerFiles(*Ring, []const fd_t) bool`;
///   - `prepRead(*Sqe, fd_t, []u8, u64)`, `prepCancel(*Sqe, target_user_data: u64)`,
///     `prepFdatasync(*Sqe, fd_t)`.
pub fn Parked(comptime F: type) type {
    return struct {
        const Self = @This();
        pub const Ring = F.Ring;
        pub const Sqe = F.Sqe;
        pub const Cqe = F.Cqe;

        ring: Ring,
        event_fd: linux.fd_t,
        /// The wake READ's target: heap, so its address is stable however the
        /// owner moves this struct between `init` and the first park.
        wake_buf: *u64,
        coalescer: nd.WakeCoalescer = .{},
        wake_armed: bool = false,
        /// A wake CQE was reaped and not yet disarmed (`service` closes it).
        wake_consumed: bool = false,
        dead: bool = false,
        gpa: Allocator,

        /// `flags` are the owner's `io_uring_setup` flags.
        pub fn init(gpa: Allocator, entries: u16, flags: u32) InitError!Self {
            var rung: usize = undefined;
            return initProbing(gpa, entries, &.{flags}, &rung);
        }

        /// `init` over the owner's probe ladder of setup flags (charter §3: a
        /// runtime ladder, never a kernel-version table): the first rung the
        /// kernel accepts wins and `rung` receives its index; a refused rung is
        /// a debug line, only exhaustion is the error.
        pub fn initProbing(gpa: Allocator, entries: u16, ladder: []const u32, rung: *usize) InitError!Self {
            std.debug.assert(ladder.len != 0);
            // Level-triggered counter; non-blocking so a READ racing a late
            // write never blocks a plain read.
            const efd_rc = linux.eventfd(0, linux.EFD.CLOEXEC | linux.EFD.NONBLOCK);
            if (linux.errno(efd_rc) != .SUCCESS) {
                log.err("ring_park: eventfd failed ({t})", .{linux.errno(efd_rc)});
                return error.RingSetupFailed;
            }
            const efd: linux.fd_t = @intCast(efd_rc);
            errdefer _ = linux.close(efd);
            const wake_buf = try gpa.create(u64);
            errdefer gpa.destroy(wake_buf);
            wake_buf.* = 0;
            var last_err: anyerror = error.RingSetupFailed;
            const ring = for (ladder, 0..) |flags, i| {
                if (F.setup(entries, flags)) |r| {
                    rung.* = i;
                    break r;
                } else |e| {
                    last_err = e;
                    if (ladder.len > 1) log.debug("ring_park: io_uring_setup({d}, flags 0x{x}) refused ({s}); next rung", .{ entries, flags, @errorName(e) });
                }
            } else {
                log.err("ring_park: io_uring_setup({d}) failed on every rung: {s} — the lane cannot start (charter §1.2: no classical fallback)", .{ entries, @errorName(last_err) });
                return error.RingSetupFailed;
            };
            return .{ .ring = ring, .event_fd = efd, .wake_buf = wake_buf, .gpa = gpa };
        }

        /// Cancel the armed wake READ and reap its CQE, then tear down. The
        /// owner has already drained its own in-flight ops (it knows them; this
        /// primitive does not). Only the ONE expected CQE is consumed here. On
        /// the ring's own thread, or after `quiesce` ran there.
        pub fn deinit(self: *Self) void {
            self.quiesce();
            F.deinit(&self.ring);
            _ = linux.close(self.event_fd);
            self.gpa.destroy(self.wake_buf);
            self.* = undefined;
        }

        /// Phase 1 of a two-thread teardown, ON the ring's thread: cancel the
        /// standing READ and reap its CQE, so nothing lands in `wake_buf` later
        /// and `deinit` may run on the joining thread (a `SINGLE_ISSUER` ring
        /// refuses another thread's enter). The eventfd stays open until
        /// `deinit`, so the closer's `forceWake` never races the lane's exit.
        pub fn quiesce(self: *Self) void {
            if (self.wake_armed and !self.dead) self.cancelWake();
        }

        pub fn wakeHandle(self: *Self) Wake {
            return .{ .fd = self.event_fd, .coalescer = &self.coalescer };
        }

        /// `wakeHandle().wake()` from the owner's own API face.
        pub fn wake(self: *Self) bool {
            return self.wakeHandle().wake();
        }

        pub fn forceWake(self: *Self) void {
            self.wakeHandle().force();
        }

        pub fn isDead(self: *const Self) bool {
            return self.dead;
        }

        /// The owner's verdict that the ring broke (an enter or a CQE read
        /// failed on its side): parks move to the eventfd, nothing is armed.
        pub fn markDead(self: *Self) void {
            self.dead = true;
            self.wake_armed = false;
        }

        /// The raw take with one SQ-full flush: a plain `submit()` frees the SQ
        /// independently of completions.
        pub fn getSqe(self: *Self) ?*Sqe {
            if (F.getSqe(&self.ring)) |sqe| return sqe;
            _ = F.submit(&self.ring) catch {};
            return F.getSqe(&self.ring);
        }

        /// Push the eventfd READ under `wake_ud` unless it already stands. True
        /// when armed on return; false when the SQ stayed full after a flush
        /// (the owner fires its tripwire and the park carries a tick).
        pub fn armWake(self: *Self) bool {
            if (self.wake_armed) return true;
            if (self.dead) return false;
            const sqe = self.getSqe() orelse return false;
            F.prepRead(sqe, self.event_fd, std.mem.asBytes(self.wake_buf), 0);
            sqe.user_data = wake_ud;
            self.wake_armed = true;
            return true;
        }

        /// Register `fd` as fixed file 0 (one fd lookup/refcount per op saved).
        /// A refusal degrades to the raw fd — the owner records which and names
        /// `sqeFd`/`markFixed` accordingly.
        pub fn registerFile(self: *Self, fd: linux.fd_t) bool {
            return F.registerFiles(&self.ring, &.{fd});
        }

        /// `IORING_FSYNC_DATASYNC` on `fd` (0 with `IOSQE_FIXED_FILE` when the
        /// owner registered it). Null when no SQE could be had after a flush.
        pub fn prepFdatasync(self: *Self, fd: linux.fd_t, fixed: bool, user_data: u64) ?*Sqe {
            const sqe = self.getSqe() orelse return null;
            F.prepFdatasync(sqe, fd);
            if (fixed) sqe.flags |= linux.IOSQE_FIXED_FILE;
            sqe.user_data = user_data;
            return sqe;
        }

        /// THE wait. Flushes this pass's SQEs, then waits for the first CQE of
        /// either class, for at most `tick_ns` (null = unbounded). An enter
        /// failure other than EINTR marks the ring dead and returns `.event` so
        /// the owner fails its in-flight ops.
        pub fn park(self: *Self, tick_ns: ?u64) Verdict {
            if (self.dead) return self.parkDead(tick_ns);
            const ring = &self.ring;
            const tick = tick_ns orelse return self.untimedWait();
            // Submit FIRST, in its own enter (see the module doc: a submitting
            // enter discards the wait's ETIME).
            _ = F.submit(ring) catch |e| switch (e) {
                error.Interrupted => {},
                else => return self.enterFailed(@errorName(e)),
            };
            if (ring.features & linux.IORING_FEAT_EXT_ARG == 0) return self.untimedWait();
            var ts: linux.kernel_timespec = .{ .sec = @intCast(tick / std.time.ns_per_s), .nsec = @intCast(tick % std.time.ns_per_s) };
            var arg: linux.io_uring_getevents_arg = .{ .sigmask = 0, .sigmask_sz = 0, .pad = 0, .ts = @intFromPtr(&ts) };
            // The kernel reads `arg`/`ts` during this call only.
            const rc = linux.syscall6(.io_uring_enter, @as(u32, @bitCast(ring.fd)), 0, 1, linux.IORING_ENTER_GETEVENTS | linux.IORING_ENTER_EXT_ARG, @intFromPtr(&arg), @sizeOf(linux.io_uring_getevents_arg));
            return switch (linux.errno(rc)) {
                .SUCCESS, .INTR => .event,
                .TIME => .tick,
                // Pre-5.11 kernels without EXT_ARG (below the charter's floor): an untimed wait.
                .INVAL => self.untimedWait(),
                else => |e| self.enterFailed(@tagName(e)),
            };
        }

        fn untimedWait(self: *Self) Verdict {
            _ = F.submitAndWait(&self.ring, 1) catch |e| switch (e) {
                error.Interrupted => {},
                else => return self.enterFailed(@errorName(e)),
            };
            return .event;
        }

        fn enterFailed(self: *Self, what: []const u8) Verdict {
            log.err("ring_park: io_uring_enter failed: {s} — the ring is dead; the owner fails every in-flight op and every later request (charter §1.2: no classical fallback)", .{what});
            self.markDead();
            return .event;
        }

        /// The dead ring's park: `poll(2)` on the eventfd until a wake or the
        /// tick; a wake counts as consumed so the owner's `service` disarms.
        fn parkDead(self: *Self, tick_ns: ?u64) Verdict {
            var pfd = [_]linux.pollfd{.{ .fd = self.event_fd, .events = linux.POLL.IN, .revents = 0 }};
            const timeout_ms: i32 = if (tick_ns) |t| @intCast(@min(t / std.time.ns_per_ms, std.math.maxInt(i32))) else -1;
            const rc = linux.poll(&pfd, 1, timeout_ms);
            if (linux.errno(rc) == .SUCCESS and rc == 0) return .tick;
            var count: u64 = 0;
            _ = linux.read(self.event_fd, std.mem.asBytes(&count), @sizeOf(u64));
            self.wake_consumed = true;
            return .event;
        }

        /// Fold one reaped CQE: true ⇒ it was the wake READ (consumed here; the
        /// owner skips it), false ⇒ it is the owner's.
        pub fn consumeWake(self: *Self, cqe: Cqe) bool {
            if (cqe.user_data != wake_ud) return false;
            self.wake_consumed = true;
            self.wake_armed = false;
            return true;
        }

        /// The lane half of the wake pair: drain → DISARM → scan. Called at the
        /// top of every pass, before the owner scans its queue. True ⇒ a wake
        /// was consumed since the last call.
        pub fn service(self: *Self) bool {
            if (!self.wake_consumed) return false;
            self.coalescer.disarm();
            self.wake_consumed = false;
            return true;
        }

        /// Cancel the standing READ and reap exactly its CQE (ECANCELED, or a
        /// completion if a wake was pending — either way one `wake_ud` CQE).
        fn cancelWake(self: *Self) void {
            const sqe = self.getSqe() orelse return;
            F.prepCancel(sqe, wake_ud);
            sqe.user_data = reserved_ud;
            _ = F.submit(&self.ring) catch return;
            var cqes: [8]Cqe = undefined;
            var seen_wake = false;
            var seen_cancel = false;
            while (!(seen_wake and seen_cancel)) {
                const n = F.copyCqes(&self.ring, &cqes, 1) catch return;
                for (cqes[0..n]) |cqe| {
                    if (cqe.user_data == wake_ud) seen_wake = true;
                    if (cqe.user_data == reserved_ud) seen_cancel = true;
                }
            }
            self.wake_armed = false;
        }
    };
}

/// std's 64 B `IoUring` behind the face. Its SQE/CQE prep and ring state
/// are std's; only the enters are raw so EINTR and the rest arrive as the
/// table's members (`core/errors.zig`) like every other ring's.
const StdFace = struct {
    pub const Ring = IoUring;
    pub const Sqe = linux.io_uring_sqe;
    pub const Cqe = linux.io_uring_cqe;

    pub fn setup(entries: u16, flags: u32) !IoUring {
        return IoUring.init(entries, flags);
    }

    pub fn deinit(ring: *IoUring) void {
        ring.deinit();
    }

    pub fn getSqe(ring: *IoUring) ?*Sqe {
        return ring.get_sqe() catch null;
    }

    /// No parked ring runs SQPOLL (charter §4: a knob, measured not
    /// recommended), so every submit is exactly one enter.
    pub fn submit(ring: *IoUring) errors.Error!u32 {
        return enterRaw(ring.fd, ring.flush_sq(), 0, 0);
    }

    pub fn submitAndWait(ring: *IoUring, wait_nr: u32) errors.Error!u32 {
        return enterRaw(ring.fd, ring.flush_sq(), wait_nr, linux.IORING_ENTER_GETEVENTS);
    }

    pub fn copyCqes(ring: *IoUring, cqes: []Cqe, wait_nr: u32) !u32 {
        return ring.copy_cqes(cqes, wait_nr);
    }

    pub fn registerFiles(ring: *IoUring, fds: []const linux.fd_t) bool {
        ring.register_files(fds) catch return false;
        return true;
    }

    pub fn prepRead(sqe: *Sqe, fd: linux.fd_t, buf: []u8, off: u64) void {
        sqe.prep_read(fd, buf, off);
    }

    pub fn prepCancel(sqe: *Sqe, target: u64) void {
        sqe.prep_cancel(target, 0);
    }

    pub fn prepFdatasync(sqe: *Sqe, fd: linux.fd_t) void {
        sqe.prep_fsync(fd, linux.IORING_FSYNC_DATASYNC);
    }

    fn enterRaw(fd: linux.fd_t, to_submit: u32, min_complete: u32, flags: u32) errors.Error!u32 {
        const rc = linux.io_uring_enter(fd, to_submit, min_complete, flags, null);
        return switch (linux.errno(rc)) {
            .SUCCESS => @intCast(rc),
            else => |e| errors.fromErrno(e),
        };
    }
};

/// The protocol over the 64 B ring: the journal lane, the NVMe lanes, the
/// sideband/INIT ring.
pub const ParkedRing = Parked(StdFace);

// ---------------------------------------------------------------------------
// Tests: the contract.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "a timed park returns .tick when nothing completes; a wake from another thread ends it as .event; a wake before the park is sticky" {
    var pr = try ParkedRing.init(testing.allocator, 8, 0);
    defer pr.deinit();
    try testing.expect(pr.armWake());
    try testing.expect(pr.armWake());
    try testing.expectEqual(Verdict.tick, pr.park(2 * std.time.ns_per_ms));
    try testing.expect(!pr.service());

    const Waker = struct {
        fn run(w: Wake) void {
            std.debug.assert(w.wake());
        }
    };
    const t = try std.Thread.spawn(.{}, Waker.run, .{pr.wakeHandle()});
    try testing.expectEqual(Verdict.event, pr.park(10 * std.time.ns_per_s));
    t.join();
    var cqes: [4]linux.io_uring_cqe = undefined;
    const n = try pr.ring.copy_cqes(&cqes, 0);
    try testing.expectEqual(@as(u32, 1), n);
    try testing.expect(pr.consumeWake(cqes[0]));
    try testing.expect(!pr.wake_armed);
    // Drain → disarm → scan: the disarm re-opens the coalescer.
    try testing.expect(pr.service());
    try testing.expect(!pr.service());

    // A wake delivered before the park: the re-armed READ completes at once.
    try testing.expect(pr.wake());
    // The second producer elides: one write per lane pass.
    try testing.expect(!pr.wake());
    try testing.expect(pr.armWake());
    try testing.expectEqual(Verdict.event, pr.park(10 * std.time.ns_per_s));
    const m = try pr.ring.copy_cqes(&cqes, 0);
    try testing.expectEqual(@as(u32, 1), m);
    try testing.expect(pr.consumeWake(cqes[0]));
    try testing.expect(pr.service());
    // Unarmed at teardown: nothing to cancel.
}

test "an owner's SQE completes through the park beside the standing wake READ; deinit cancels an armed READ cleanly" {
    var tmp = testing.tmpDir(.{});
    defer tmp.cleanup();
    var dir_buf: [std.fs.max_path_bytes]u8 = undefined;
    const dir_len = try tmp.dir.realPath(testing.io, &dir_buf);
    var path_buf: [std.fs.max_path_bytes + 16]u8 = undefined;
    const path = try std.mem.printSentinel(&path_buf, "{s}/park.dat", .{dir_buf[0..dir_len]}, 0);
    const fd_rc = linux.open(path, .{ .ACCMODE = .RDWR, .CLOEXEC = true, .CREAT = true }, 0o644);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(fd_rc));
    const fd: linux.fd_t = @intCast(fd_rc);
    defer _ = linux.close(fd);

    var pr = try ParkedRing.init(testing.allocator, 8, 0);
    defer pr.deinit();
    const fixed = pr.registerFile(fd);
    const sqe_fd: linux.fd_t = if (fixed) 0 else fd;
    try testing.expect(pr.armWake());

    var block: [4096]u8 = @splat(0xA5);
    const w = pr.getSqe().?;
    w.prep_write(sqe_fd, &block, 0);
    if (fixed) w.flags |= linux.IOSQE_FIXED_FILE;
    w.user_data = 7;
    try testing.expectEqual(Verdict.event, pr.park(10 * std.time.ns_per_s));
    var cqes: [4]linux.io_uring_cqe = undefined;
    var saw_write = false;
    while (!saw_write) {
        const n = try pr.ring.copy_cqes(&cqes, 1);
        for (cqes[0..n]) |cqe| {
            try testing.expect(!pr.consumeWake(cqe));
            try testing.expectEqual(@as(u64, 7), cqe.user_data);
            try testing.expectEqual(@as(i32, 4096), cqe.res);
            saw_write = true;
        }
    }
    // The barrier on the same ring: an fdatasync SQE under a reserved tag.
    const s = pr.prepFdatasync(sqe_fd, fixed, reserved_ud - 1).?;
    _ = s;
    try testing.expectEqual(Verdict.event, pr.park(10 * std.time.ns_per_s));
    const n = try pr.ring.copy_cqes(&cqes, 1);
    try testing.expectEqual(@as(u32, 1), n);
    try testing.expectEqual(reserved_ud - 1, cqes[0].user_data);
    try testing.expectEqual(@as(i32, 0), cqes[0].res);
    try testing.expect(pr.wake_armed);
    // `deinit` (deferred) cancels the standing READ and reaps its CQE.
}

test "initProbing: a refused rung falls through to the next (its index reported); a ladder the kernel refuses entirely is the one loud error" {
    // DEFER_TASKRUN without SINGLE_ISSUER is EINVAL on every kernel that
    // knows the flag, and an unknown flag before that: a reliably refused rung.
    const ladder = [_]u32{ linux.IORING_SETUP_DEFER_TASKRUN, 0 };
    var rung: usize = undefined;
    var pr = try ParkedRing.initProbing(testing.allocator, 8, &ladder, &rung);
    defer pr.deinit();
    try testing.expectEqual(@as(usize, 1), rung);
    try testing.expect(pr.armWake());
    try testing.expectEqual(Verdict.tick, pr.park(2 * std.time.ns_per_ms));
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.RingSetupFailed, ParkedRing.initProbing(testing.allocator, 8, ladder[0..1], &rung));
    try log.testing.expectAllFired();
}

test "quiesce on the ring's thread, deinit on the joiner's: the standing READ is cancelled and reaped first, the eventfd outlives the ring thread" {
    // The ring is built on the lane thread (SINGLE_ISSUER: its only
    // submitter) and freed by the joiner, as the NVMe lanes do.
    var pr: ParkedRing = undefined;
    const Owner = struct {
        fn run(p: *ParkedRing) void {
            p.* = ParkedRing.init(testing.allocator, 8, linux.IORING_SETUP_SINGLE_ISSUER) catch @panic("ring setup failed");
            std.debug.assert(p.armWake());
            // A wake pending in the eventfd: the READ completes rather than
            // cancels — either way exactly its CQE is reaped.
            p.forceWake();
            p.quiesce();
            std.debug.assert(!p.wake_armed);
        }
    };
    const t = try std.Thread.spawn(.{}, Owner.run, .{&pr});
    t.join();
    // The closer's late wake lands in a live eventfd (nothing reads it).
    pr.forceWake();
    var cqes: [4]linux.io_uring_cqe = undefined;
    try testing.expectEqual(@as(u32, 0), try pr.ring.copy_cqes(&cqes, 0));
    pr.deinit();
}

test "a dead ring parks on the eventfd alone: the tick still fires, a wake still arrives and is consumed" {
    var pr = try ParkedRing.init(testing.allocator, 8, 0);
    defer pr.deinit();
    pr.markDead();
    try testing.expect(pr.isDead());
    try testing.expect(!pr.armWake());
    try testing.expectEqual(Verdict.tick, pr.park(2 * std.time.ns_per_ms));
    pr.forceWake();
    try testing.expectEqual(Verdict.event, pr.park(10 * std.time.ns_per_s));
    try testing.expect(pr.service());
    // The coalescer still gates producers on a dead ring.
    try testing.expect(pr.wake());
    try testing.expect(!pr.wake());
    try testing.expectEqual(Verdict.event, pr.park(null));
    try testing.expect(pr.service());
}
