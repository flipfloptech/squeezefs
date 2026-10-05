//! The pooled io_uring file layer and the O_DIRECT metadata device posture
//! (charter §1.2 "an io_uring file layer", §5.1 "Device posture";
//! `docs/porting/uring_fs.md`). Linux only.
//!
//! Shape: N worker threads, each owning ONE plain io_uring, consuming ONE
//! shared bounded MPMC queue of requests. A caller pushes a request carrying a
//! futex oneshot and parks on it; the worker burst-admits, builds one SQE per
//! op, `submit_and_wait(1)`, reaps, resubmits short reads/writes from the reap
//! pass, and completes the oneshots. The owned-ring variant (`OwnedRing`)
//! reuses the same reactor on the thread that also awaits completions (the
//! journal lane) and parks in `io_uring_enter` with an eventfd READ armed.
//!
//! The posture law (charter §5.1) is applied on the CALLER's thread before a
//! request is queued: a misaligned product write is refused (tripwire), a
//! misaligned buffer is bounced into an aligned copy, a narrow read is widened
//! to the grain and sliced on completion. Doing it before the queue keeps
//! "fail before any byte is submitted" literal and keeps the loud paths on
//! the thread whose contract they are.
//!
//! No fixed files, no registered buffers, no SQPOLL, no SQE128/DEFER_TASKRUN:
//! this is the ad-hoc local file layer (config records, metadata images,
//! journal rings on plain files) — the fd set churns per path and the
//! payload buffers are the caller's, so there is nothing stable to register.
//! The NVMe block workers (`nvme_dev`) are where fixed files pay.
//!
//! Buffer lifetime across DMA: every SQE's pointer target is owned by the
//! in-flight slot (`Pending`) — the read buffer outright, the write payload as
//! the caller's slice which the API contract keeps alive until the completion
//! is observed, or the bounce copy outright. A slot is never touched until its
//! CQE is reaped.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const IoUring = linux.IoUring;
const Allocator = std.mem.Allocator;

const align_probe = @import("align_probe.zig");
const clock = @import("../core/clock.zig");
const counters = @import("../core/counters.zig");
const derive = @import("../core/derive.zig");
const knobs = @import("../core/knobs.zig");
const log = @import("../core/log.zig");
const thread_name = @import("../core/thread_name.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("uring_fs is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

// ---------------------------------------------------------------------------
// Constants (`docs/porting/uring_fs.md` §7).
// ---------------------------------------------------------------------------

/// Backpressure, not custody: the queue holds small request structs while the
/// payload bytes stay with the caller until service. Parity with
/// `nvme_dev`'s request queue. Power of two for the MPMC ring arithmetic.
pub const queue_cap: usize = 4096;

/// SQ depth per worker ring; the kernel sizes the CQ at 2×.
pub const ring_entries: u16 = 512;

/// Max ops a worker admits/keeps in flight. Continuations reuse their slot,
/// so SQ pressure is bounded by this plus one burst of resubmits.
pub const admit_cap: usize = 256;

/// The journal lane's owned ring: a window's entries (≤ the commit batch cap,
/// each one or two segment SQEs plus its owned page headers) plus the wake
/// READ.
pub const journal_ring_entries: u16 = 512;

/// `user_data` of the owned ring's eventfd READ: never a slot index (slots
/// are small integers). `u64::MAX − 1` stays reserved for the fault shim's
/// linked timeout so a future seam cannot collide either.
pub const wake_ud: u64 = std.math.maxInt(u64);

/// `clamp(cpus/4, 4, 64)`: cpus/4 is the measured drain-thread slope; floor 4
/// is the shipped floor (Hang-1 "pinned-first-toucher sizing poison" — a tiny
/// affinity share still gets a pool); ceiling 64 is parity with the env clamp.
/// An explicit override wins verbatim within 1..=64.
pub fn resolveWorkerCount(override: ?u32, cpus: u32) u32 {
    if (override) |o| return @intCast(derive.clamp(o, 1, 64));
    return @intCast(derive.clamp(cpus / 4, 4, 64));
}

/// `clamp(soft/4/max(workers,1), 16, 1024)`: the pool's aggregate open fds
/// stay under a quarter of RLIMIT_NOFILE as FOUND at startup (the raised
/// limit serves listener caps alone); capped at 1024, floored at 16.
pub fn fdCacheCapFrom(nofile_soft: u64, workers: u32) u32 {
    return @intCast(derive.clamp(nofile_soft / 4 / @max(workers, 1), 16, 1024));
}

/// Worker thread base name; `sqz-uring-fs` + two digits fits the kernel's 15
/// visible comm chars untagged (12 + 2 = 14).
pub fn workerBase(buf: *[24]u8, index: u32) []const u8 {
    return std.fmt.bufPrint(buf, "sqz-uring-fs{d}", .{index}) catch unreachable;
}

pub fn workerName(buf: *thread_name.CommBuf, index: u32) [:0]const u8 {
    var base: [24]u8 = undefined;
    return thread_name.commName(buf, workerBase(&base, index));
}

var nofile_soft_cache: std.atomic.Value(u64) = .init(0);
var nofile_soft_found: std.atomic.Value(bool) = .init(false);

/// RLIMIT_NOFILE's soft limit as FOUND the first time anything asks — the
/// startup gate asks first, so a later raise never moves the cache budget.
pub fn nofileSoftAsFound() u64 {
    // acquire pairs with the release store below: the value is visible with the flag.
    if (nofile_soft_found.load(.acquire)) return nofile_soft_cache.load(.monotonic);
    var rl: linux.rlimit = undefined;
    const soft: u64 = if (linux.errno(linux.getrlimit(.NOFILE, &rl)) == .SUCCESS) @intCast(rl.cur) else 1024;
    nofile_soft_cache.store(soft, .monotonic);
    nofile_soft_found.store(true, .release);
    return soft;
}

pub const Config = struct {
    workers: u32,
    fd_cache_cap: u32,

    /// The product's derivation: knob override, process parallelism, the
    /// nofile limit as found.
    pub fn resolve(env: *const std.process.Environ.Map, facts: derive.Facts) Config {
        const override = knobs.optIntKnob(u32, env, "SQUEEZEFS_URING_FS_WORKERS");
        const workers = resolveWorkerCount(override, facts.cpus);
        return .{ .workers = workers, .fd_cache_cap = fdCacheCapFrom(nofileSoftAsFound(), workers) };
    }

    /// A synthetic shape for tests and harnesses.
    pub fn fixed(workers: u32, nofile_soft: u64) Config {
        return .{ .workers = workers, .fd_cache_cap = fdCacheCapFrom(nofile_soft, workers) };
    }
};

/// The first pool's config is the process's "in force" face: the posture
/// registry's negative cache is bounded by its fd cache cap, exactly as the
/// reference bounds it by `fd_cache_cap()`.
var config_in_force: std.atomic.Value(u64) = .init(0);

fn publishConfigInForce(c: Config) void {
    const packed_cfg = (@as(u64, c.workers) << 32) | c.fd_cache_cap;
    _ = config_in_force.cmpxchgStrong(0, packed_cfg, .monotonic, .monotonic);
}

pub fn configInForce() Config {
    const v = config_in_force.load(.monotonic);
    if (v == 0) {
        const workers = resolveWorkerCount(null, @intCast(@max(1, std.Thread.getCpuCount() catch 1)));
        return .{ .workers = workers, .fd_cache_cap = fdCacheCapFrom(nofileSoftAsFound(), workers) };
    }
    return .{ .workers = @intCast(v >> 32), .fd_cache_cap = @truncate(v) };
}

// ---------------------------------------------------------------------------
// Errors: one typed set, three classes, ONE errno table.
// ---------------------------------------------------------------------------

pub const Error = error{
    // ---- retryable: the caller may try again later ----
    /// The request queue is full (backpressure; `uring_queue_full` counts it).
    QueueFull,
    Interrupted,
    WouldBlock,
    // ---- terminal: this op is lost; the device or the file said no ----
    InputOutput,
    NoSpaceLeft,
    QuotaExceeded,
    FileNotFound,
    AccessDenied,
    BadFileDescriptor,
    IsDir,
    NotDir,
    PathAlreadyExists,
    TooManyOpenFiles,
    NameTooLong,
    FileTooBig,
    ReadOnlyFileSystem,
    NotSupported,
    Canceled,
    OutOfMemory,
    /// The kernel said EINVAL (e.g. an O_DIRECT shape the driver rejects).
    InvalidArgument,
    /// A write SQE completed with 0 bytes.
    ShortWrite,
    /// An SQE could not be pushed even after flushing the SQ.
    SubmissionQueueOverflow,
    /// The worker's ring broke; every op it held failed with this.
    RingFailed,
    /// The pool shut down before the op completed.
    WorkerClosed,
    /// An errno outside the table (logged with its number).
    Unexpected,
    // ---- refusal: a caller bug or an inadmissible device; never retried ----
    UnalignedDirectWrite,
    GrainNotAdmissible,
    NotADeviceOrFile,
    DirectOpenRefused,
    CannotStat,
};

pub const ErrorClass = enum { retryable, terminal, refusal };

pub fn classOf(err: Error) ErrorClass {
    return switch (err) {
        error.QueueFull, error.Interrupted, error.WouldBlock => .retryable,
        error.UnalignedDirectWrite, error.GrainNotAdmissible, error.NotADeviceOrFile, error.DirectOpenRefused, error.CannotStat => .refusal,
        else => .terminal,
    };
}

/// Registration's own error set (a subset of `Error`).
pub const RegisterError = error{ CannotStat, NotADeviceOrFile, GrainNotAdmissible, DirectOpenRefused, OutOfMemory, NameTooLong };

const ErrnoRow = struct { e: linux.E, err: Error };

/// THE errno table, read in both directions. A CQE's negative `res` maps
/// through `fromErrno`; a FUSE reply maps back through `toErrno`.
const errno_table = [_]ErrnoRow{
    .{ .e = .IO, .err = error.InputOutput },
    .{ .e = .NOSPC, .err = error.NoSpaceLeft },
    .{ .e = .DQUOT, .err = error.QuotaExceeded },
    .{ .e = .NOENT, .err = error.FileNotFound },
    .{ .e = .ACCES, .err = error.AccessDenied },
    .{ .e = .PERM, .err = error.AccessDenied },
    .{ .e = .BADF, .err = error.BadFileDescriptor },
    .{ .e = .ISDIR, .err = error.IsDir },
    .{ .e = .NOTDIR, .err = error.NotDir },
    .{ .e = .EXIST, .err = error.PathAlreadyExists },
    .{ .e = .MFILE, .err = error.TooManyOpenFiles },
    .{ .e = .NFILE, .err = error.TooManyOpenFiles },
    .{ .e = .NAMETOOLONG, .err = error.NameTooLong },
    .{ .e = .FBIG, .err = error.FileTooBig },
    .{ .e = .ROFS, .err = error.ReadOnlyFileSystem },
    .{ .e = .OPNOTSUPP, .err = error.NotSupported },
    .{ .e = .CANCELED, .err = error.Canceled },
    .{ .e = .NOMEM, .err = error.OutOfMemory },
    .{ .e = .INVAL, .err = error.InvalidArgument },
    .{ .e = .INTR, .err = error.Interrupted },
    .{ .e = .AGAIN, .err = error.WouldBlock },
};

pub fn fromErrno(e: linux.E) Error {
    for (errno_table) |row| if (row.e == e) return row.err;
    log.warn("uring-fs: errno {d} ({s}) is outside the table — reported as Unexpected (EIO)", .{ @backingInt(e), @tagName(e) });
    return error.Unexpected;
}

pub fn toErrno(err: Error) linux.E {
    for (errno_table) |row| if (row.err == err) return row.e;
    return switch (err) {
        // The reference replies EINVAL for backpressure and for every refusal.
        error.QueueFull, error.UnalignedDirectWrite, error.GrainNotAdmissible, error.NotADeviceOrFile, error.DirectOpenRefused, error.CannotStat => .INVAL,
        else => .IO,
    };
}

/// A CQE's `res`: negative is `-errno`.
fn cqeError(res: i32) Error {
    return fromErrno(@fromBackingInt(@intCast(@as(u16, @intCast(-res)))));
}

// ---------------------------------------------------------------------------
// Futex primitives (Linux raw; `std.Io`'s take an Io handle — none here).
// ---------------------------------------------------------------------------

fn futexWait(word: *const std.atomic.Value(u32), expect: u32) void {
    // EAGAIN (word != expect) and EINTR both mean "re-check"; the callers loop.
    _ = linux.futex_4arg(&word.raw, .{ .cmd = .WAIT, .private = true }, expect, null);
}

fn futexWake(word: *const std.atomic.Value(u32), max_waiters: u32) void {
    // FUTEX_WAKE's count is a C int: anything above INT_MAX reads as negative
    // and wakes exactly ONE waiter (a `close` that must wake every parked
    // worker would strand the rest in `join`).
    _ = linux.futex_3arg(&word.raw, .{ .cmd = .WAKE, .private = true }, @min(max_waiters, std.math.maxInt(i32)));
}

/// A minimal mutex for the rare writer paths (registry rebuilds). Never on an
/// I/O path.
const FutexMutex = struct {
    state: std.atomic.Value(u32) = .init(unlocked),
    const unlocked = 0;
    const locked = 1;
    const contended = 2;

    fn lock(self: *FutexMutex) void {
        // acquire pairs with unlock's release swap: the critical section sees
        // the previous holder's writes.
        if (self.state.cmpxchgStrong(unlocked, locked, .acquire, .monotonic) == null) return;
        while (self.state.swap(contended, .acquire) != unlocked) futexWait(&self.state, contended);
    }

    fn unlock(self: *FutexMutex) void {
        if (self.state.swap(unlocked, .release) == contended) futexWake(&self.state, 1);
    }
};

/// The reply channel (worker → caller): a futex word plus the value, shared
/// by two parties with a refcount so either may drop first.
fn Oneshot(comptime T: type) type {
    return struct {
        const Self = @This();
        const pending = 0;
        const done = 1;

        /// The futex word: `pending` until `complete` publishes the value.
        state: std.atomic.Value(u32) = .init(pending),
        /// Two parties: the completer and the waiter. The last one frees.
        refs: std.atomic.Value(u32) = .init(2),
        value: T = undefined,
        gpa: Allocator,

        fn create(gpa: Allocator) error{OutOfMemory}!*Self {
            const self = try gpa.create(Self);
            self.* = .{ .gpa = gpa };
            return self;
        }

        /// Publish the value and wake the waiter. The release store pairs
        /// with `wait`'s acquire load so the value is visible with the state.
        fn complete(self: *Self, value: T) void {
            self.value = value;
            self.state.store(done, .release);
            futexWake(&self.state, std.math.maxInt(u32));
            self.release();
        }

        fn isDone(self: *const Self) bool {
            return self.state.load(.acquire) == done;
        }

        /// Park until the value is published; the caller then `release`s.
        fn wait(self: *Self) T {
            while (self.state.load(.acquire) == pending) futexWait(&self.state, pending);
            return self.value;
        }

        /// Drop this party's reference. The acq_rel RMW pairs with the other
        /// party's: whoever frees has seen every access the other made.
        fn release(self: *Self) void {
            if (self.refs.fetchSub(1, .acq_rel) == 1) self.gpa.destroy(self);
        }

        /// A request that never reached a worker has no completer: both
        /// references are the caller's.
        fn destroyUnsent(self: *Self) void {
            self.gpa.destroy(self);
        }
    };
}

/// Bounded MPMC ring (the Vyukov cell-sequence queue) with a futex-parked
/// consumer side. Producers never block (`trySend`); consumers may park when
/// idle (`recvBlocking`). `cap` is a power of two.
fn Mpmc(comptime T: type, comptime cap: usize) type {
    comptime std.debug.assert(std.math.isPowerOfTwo(cap));
    return struct {
        const Self = @This();
        const mask = cap - 1;

        const Cell = struct {
            /// The cell's turn: equals its index when empty for position
            /// `index + k*cap`, index+1 when full.
            seq: std.atomic.Value(usize),
            data: T,
        };

        cells: [cap]Cell = undefined,
        enqueue_pos: std.atomic.Value(usize) align(std.atomic.cache_line) = .init(0),
        dequeue_pos: std.atomic.Value(usize) align(std.atomic.cache_line) = .init(0),
        /// Bumped by every send and by close; the consumer's futex word.
        epoch: std.atomic.Value(u32) align(std.atomic.cache_line) = .init(0),
        /// Consumers parked (or about to park) on `epoch`.
        sleepers: std.atomic.Value(u32) = .init(0),
        closed: std.atomic.Value(bool) = .init(false),

        fn init(self: *Self) void {
            self.* = .{};
            for (&self.cells, 0..) |*c, i| c.seq = .init(i);
        }

        pub fn trySend(self: *Self, item: T) error{ Full, Closed }!void {
            // seq_cst pairs with `close`'s store. A producer that observed
            // open may still push after the consumers' final drain; the owner
            // drains once more after joining them (Pool.deinit), so nothing
            // is stranded.
            if (self.closed.load(.seq_cst)) return error.Closed;
            var pos = self.enqueue_pos.load(.monotonic);
            const cell = while (true) {
                const c = &self.cells[pos & mask];
                // acquire pairs with the consumer's release of the cell.
                const seq = c.seq.load(.acquire);
                const dif = @as(isize, @bitCast(seq -% pos));
                if (dif == 0) {
                    if (self.enqueue_pos.cmpxchgWeak(pos, pos + 1, .monotonic, .monotonic)) |actual| pos = actual else break c;
                } else if (dif < 0) {
                    return error.Full;
                } else {
                    pos = self.enqueue_pos.load(.monotonic);
                }
            };
            cell.data = item;
            // release publishes `data` with the turn; the consumer's acquire load sees both.
            cell.seq.store(pos + 1, .release);
            _ = self.epoch.fetchAdd(1, .seq_cst);
            // seq_cst: a consumer that registered as a sleeper before our
            // epoch bump is seen here; one that registers after sees the bump.
            if (self.sleepers.load(.seq_cst) != 0) futexWake(&self.epoch, 1);
        }

        pub fn tryRecv(self: *Self) ?T {
            var pos = self.dequeue_pos.load(.monotonic);
            const cell = while (true) {
                const c = &self.cells[pos & mask];
                const seq = c.seq.load(.acquire);
                const dif = @as(isize, @bitCast(seq -% (pos + 1)));
                if (dif == 0) {
                    if (self.dequeue_pos.cmpxchgWeak(pos, pos + 1, .monotonic, .monotonic)) |actual| pos = actual else break c;
                } else if (dif < 0) {
                    return null;
                } else {
                    pos = self.dequeue_pos.load(.monotonic);
                }
            };
            const item = cell.data;
            cell.seq.store(pos +% mask +% 1, .release);
            return item;
        }

        /// Park until an item arrives; null once closed AND drained.
        pub fn recvBlocking(self: *Self) ?T {
            while (true) {
                if (self.tryRecv()) |item| return item;
                if (self.closed.load(.seq_cst)) return self.tryRecv();
                _ = self.sleepers.fetchAdd(1, .seq_cst);
                defer _ = self.sleepers.fetchSub(1, .seq_cst);
                const e = self.epoch.load(.seq_cst);
                // Re-check after registering: a send between the first
                // tryRecv and the registration bumped epoch, so the wait
                // below returns at once (EAGAIN) and we loop.
                if (self.tryRecv()) |item| return item;
                if (self.closed.load(.seq_cst)) continue;
                futexWait(&self.epoch, e);
            }
        }

        pub fn close(self: *Self) void {
            self.closed.store(true, .seq_cst);
            _ = self.epoch.fetchAdd(1, .seq_cst);
            futexWake(&self.epoch, std.math.maxInt(u32));
        }
    };
}

// ---------------------------------------------------------------------------
// Aligned buffers and read results.
// ---------------------------------------------------------------------------

/// A heap buffer with an explicit (runtime) alignment — the direct-I/O
/// buffer form. Stable address for the SQE's lifetime.
pub const AlignedBuf = struct {
    bytes: []u8,
    alignment: std.mem.Alignment,
    gpa: Allocator,

    pub fn zeroed(gpa: Allocator, len: usize, alignment: usize) error{OutOfMemory}!AlignedBuf {
        const a = std.mem.Alignment.fromByteUnits(@max(alignment, 1));
        if (len == 0) return .{ .bytes = &.{}, .alignment = a, .gpa = gpa };
        const ptr = gpa.rawAlloc(len, a, @returnAddress()) orelse return error.OutOfMemory;
        @memset(ptr[0..len], 0);
        return .{ .bytes = ptr[0..len], .alignment = a, .gpa = gpa };
    }

    pub fn fromSlice(gpa: Allocator, src: []const u8, alignment: usize) error{OutOfMemory}!AlignedBuf {
        const b = try zeroed(gpa, src.len, alignment);
        @memcpy(b.bytes, src);
        return b;
    }

    pub fn deinit(self: *AlignedBuf) void {
        if (self.bytes.len != 0) self.gpa.rawFree(self.bytes, self.alignment, @returnAddress());
        self.bytes = &.{};
    }
};

/// A read's answer: the widened buffer plus the caller's window into it —
/// zero-copy; the buffer frees with the result.
pub const ReadResult = struct {
    buf: AlignedBuf,
    lo: usize,
    hi: usize,

    pub fn bytes(self: *const ReadResult) []u8 {
        return self.buf.bytes[self.lo..self.hi];
    }

    pub fn deinit(self: *ReadResult) void {
        self.buf.deinit();
    }

    fn empty(gpa: Allocator) ReadResult {
        return .{ .buf = .{ .bytes = &.{}, .alignment = .@"1", .gpa = gpa }, .lo = 0, .hi = 0 };
    }
};

// ---------------------------------------------------------------------------
// Requests, outcomes, completion handles.
// ---------------------------------------------------------------------------

pub const WriteOp = struct { offset: u64, data: []const u8 };

/// A write's hop stamps (`uring_fs_write_phase_ns`).
pub const WriteStamps = struct {
    submitted_at: u64,
    admitted_at: u64,
    woken_at: u64,
    observed_at: u64,
};

pub const WriteOutcome = struct {
    result: Error!void,
    /// Taken BEFORE the open: a cold fd-cache miss counts against the device span.
    admitted_at: u64,
    /// The reap pass's one clock read.
    woken_at: u64,
};

const WriteDone = Oneshot(WriteOutcome);
const ReadDone = Oneshot(Error!ReadResult);
const SyncDone = Oneshot(Error!void);

/// One op of a prepared batch: the slice to submit (the caller's or the
/// bounce's) and the bounce it may own.
const PreparedOp = struct { offset: u64, data: []const u8, bounce: ?AlignedBuf };

/// What the queue carries. Every slice is the CALLER's and must stay alive
/// until the completion is observed; every `AlignedBuf` and the `ops` array
/// are owned by the request and freed by whoever finishes it.
const Request = union(enum) {
    write_all: struct { path: []const u8, data: []const u8, done: *WriteDone },
    read_all: struct { path: []const u8, done: *ReadDone },
    read_at: struct { path: []const u8, start: u64, skip: usize, want: usize, grain: u64, direct: bool, buf: AlignedBuf, done: *ReadDone },
    write_at: struct { path: []const u8, offset: u64, data: []const u8, bounce: ?AlignedBuf, direct: bool, done: *WriteDone },
    write_at_batch: struct { path: []const u8, ops: []PreparedOp, direct: bool, done: *WriteDone },
    fdatasync: struct { path: []const u8, direct: bool, done: *SyncDone },
};

/// Complete a request that never reached a reactor (queue closed, broken
/// ring, shutdown leftovers), freeing what it owns.
fn failRequest(gpa: Allocator, req: Request, err: Error) void {
    const now = clock.monotonicNs();
    switch (req) {
        .write_all => |r| r.done.complete(.{ .result = err, .admitted_at = now, .woken_at = now }),
        .read_all => |r| r.done.complete(err),
        .read_at => |r| {
            var b = r.buf;
            b.deinit();
            r.done.complete(err);
        },
        .write_at => |r| {
            if (r.bounce) |b| {
                var bb = b;
                bb.deinit();
            }
            r.done.complete(.{ .result = err, .admitted_at = now, .woken_at = now });
        },
        .write_at_batch => |r| {
            for (r.ops) |op| if (op.bounce) |b| {
                var bb = b;
                bb.deinit();
            };
            gpa.free(r.ops);
            r.done.complete(.{ .result = err, .admitted_at = now, .woken_at = now });
        },
        .fdatasync => |r| r.done.complete(err),
    }
}

pub const WritePhase = enum(u2) { queue_hop = 0, device = 1, wake_hop = 2, total = 3 };

pub const Observed = struct { result: Error!void, stamps: WriteStamps };

/// The awaitable handle of a submitted write. `wait`/`waitStamped` consume
/// it; `deinit` drops an unobserved one. The phases are recorded ONCE, at
/// first observation.
pub const WriteCompletion = struct {
    done: ?*WriteDone,
    submitted_at: u64,
    observed: ?Observed,

    pub fn submittedAt(self: *const WriteCompletion) u64 {
        return self.submitted_at;
    }

    /// Non-blocking: true once the outcome is in hand (retained for `wait`).
    pub fn pollDone(self: *WriteCompletion) bool {
        if (self.observed != null) return true;
        const d = self.done orelse return true;
        if (!d.isDone()) return false;
        self.observe(d);
        return true;
    }

    pub fn waitStamped(self: *WriteCompletion) Observed {
        if (self.observed) |o| return o;
        const d = self.done.?;
        _ = d.wait();
        self.observe(d);
        return self.observed.?;
    }

    pub fn wait(self: *WriteCompletion) Error!void {
        return self.waitStamped().result;
    }

    /// Drop a handle without observing it (the write still lands).
    pub fn deinit(self: *WriteCompletion) void {
        if (self.done) |d| {
            d.release();
            self.done = null;
        }
    }

    fn observe(self: *WriteCompletion, d: *WriteDone) void {
        const outcome = d.value;
        d.release();
        self.done = null;
        const stamps: WriteStamps = .{
            .submitted_at = self.submitted_at,
            .admitted_at = outcome.admitted_at,
            .woken_at = outcome.woken_at,
            .observed_at = clock.monotonicNs(),
        };
        recordWritePhases(stamps);
        self.observed = .{ .result = outcome.result, .stamps = stamps };
    }

    fn alreadyDone(result: Error!void) WriteCompletion {
        const now = clock.monotonicNs();
        return .{ .done = null, .submitted_at = now, .observed = .{ .result = result, .stamps = .{ .submitted_at = now, .admitted_at = now, .woken_at = now, .observed_at = now } } };
    }
};

// ---------------------------------------------------------------------------
// Stats (`docs/porting/uring_fs.md` §5): published ungated on the stats inode.
// ---------------------------------------------------------------------------

pub const Stats = struct {
    /// Registrations that landed O_DIRECT.
    direct_paths: counters.ShardedCounter = .{},
    /// Registrations that fell to BUFFERED (regular-file EINVAL or the test
    /// seam). Must stay 0 on any block device.
    buffered_fallback: counters.ShardedCounter = .{},
    /// Bytes copied into an aligned buffer because the caller's pointer was
    /// misaligned — an economy item (the hot conveyor path pays 0).
    bounce_bytes: counters.ShardedCounter = .{},
    /// Direct writes refused for a misaligned offset/length: a caller bug.
    unaligned_refusals: counters.Tripwire = .init("meta_io_unaligned_refusals"),
    /// Direct reads whose window was widened to the grain (cold-path instrument).
    read_widened: counters.ShardedCounter = .{},
    /// `canonicalize` calls: flat under a steady stream of I/O to known paths.
    mode_resolves: counters.ShardedCounter = .{},
    /// `trySend` refusals.
    queue_full: counters.ShardedCounter = .{},
    phases: [4]counters.ShardedLatencyHistogram = @splat(.{}),
};

pub var stats: Stats = .{};

fn recordWritePhases(s: WriteStamps) void {
    stats.phases[@backingInt(WritePhase.queue_hop)].record(s.admitted_at -| s.submitted_at);
    stats.phases[@backingInt(WritePhase.device)].record(s.woken_at -| s.admitted_at);
    stats.phases[@backingInt(WritePhase.wake_hop)].record(s.observed_at -| s.woken_at);
    stats.phases[@backingInt(WritePhase.total)].record(s.observed_at -| s.submitted_at);
}

/// `{"meta_io":{…},"uring_fs_write_phase_ns":{…},"uring_queue_full":n}`
pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"meta_io\":{{\"meta_io_direct_paths\":{d},\"meta_io_buffered_fallback\":{d},\"meta_io_bounce_bytes\":{d},\"meta_io_unaligned_refusals\":{d},\"meta_io_read_widened\":{d}}},\"uring_fs_write_phase_ns\":{{", .{
        stats.direct_paths.load(),
        stats.buffered_fallback.load(),
        stats.bounce_bytes.load(),
        stats.unaligned_refusals.load(),
        stats.read_widened.load(),
    });
    inline for (.{ WritePhase.queue_hop, WritePhase.device, WritePhase.wake_hop, WritePhase.total }, 0..) |phase, i| {
        if (i != 0) try w.writeByte(',');
        try w.print("\"{s}\":", .{@tagName(phase)});
        try stats.phases[@backingInt(phase)].snapshot().writeJson(w);
    }
    try w.print("}},\"uring_queue_full\":{d}}}", .{stats.queue_full.load()});
}

// ---------------------------------------------------------------------------
// Files: raw opens, the per-reactor fd cache.
// ---------------------------------------------------------------------------

fn openPath(path: []const u8, flags: linux.O, mode: linux.mode_t) Error!linux.fd_t {
    var zbuf: [linux.PATH_MAX]u8 = undefined;
    const z = try align_probe.pathZ(&zbuf, path);
    const rc = linux.open(z, flags, mode);
    return switch (linux.errno(rc)) {
        .SUCCESS => @intCast(rc),
        else => |e| fromErrno(e),
    };
}

/// `mkdir -p` for a control-path file's parents; best effort — the open that
/// follows reports the real error.
fn mkdirAll(dir: []const u8) void {
    var zbuf: [linux.PATH_MAX]u8 = undefined;
    var i: usize = 1;
    while (i < dir.len) : (i += 1) {
        if (dir[i] != '/') continue;
        const z = align_probe.pathZ(&zbuf, dir[0..i]) catch return;
        _ = linux.mkdirat(linux.AT.FDCWD, z, 0o755);
    }
    const z = align_probe.pathZ(&zbuf, dir) catch return;
    _ = linux.mkdirat(linux.AT.FDCWD, z, 0o755);
}

/// A refcounted fd on ONE reactor thread (no atomics: the reactor is
/// single-threaded). Every in-flight op holds its own reference, so a cache
/// eviction or a posture-mismatch removal can never close an fd with an
/// outstanding SQE.
const CachedFile = struct {
    fd: linux.fd_t,
    refs: u32,

    fn create(gpa: Allocator, fd: linux.fd_t) error{OutOfMemory}!*CachedFile {
        const f = gpa.create(CachedFile) catch {
            _ = linux.close(fd);
            return error.OutOfMemory;
        };
        f.* = .{ .fd = fd, .refs = 1 };
        return f;
    }

    fn retain(self: *CachedFile) void {
        self.refs += 1;
    }

    fn release(self: *CachedFile, gpa: Allocator) void {
        self.refs -= 1;
        if (self.refs == 0) {
            _ = linux.close(self.fd);
            gpa.destroy(self);
        }
    }
};

const FdCache = struct {
    const Entry = struct { file: *CachedFile, gen: u64, direct: bool };

    gpa: Allocator,
    map: std.StringHashMapUnmanaged(Entry) = .empty,
    gen: u64 = 0,
    cap: u32,

    /// Hit with the same posture ⇒ stamp and retain. Posture MISMATCH ⇒ drop
    /// the entry: a path registered AFTER its first (buffered) open is
    /// re-opened direct at its next use.
    fn touch(self: *FdCache, path: []const u8, direct: bool) ?*CachedFile {
        self.gen += 1;
        const e = self.map.getPtr(path) orelse return null;
        if (e.direct == direct) {
            e.gen = self.gen;
            e.file.retain();
            return e.file;
        }
        const kv = self.map.fetchRemove(path).?;
        kv.value.file.release(self.gpa);
        self.gpa.free(kv.key);
        return null;
    }

    /// Insert (replacing a same-path entry); at the cap, evict the
    /// least-recently stamped entry first.
    fn insert(self: *FdCache, path: []const u8, file: *CachedFile, direct: bool) error{OutOfMemory}!void {
        self.gen += 1;
        if (self.map.getPtr(path)) |e| {
            e.file.release(self.gpa);
            e.* = .{ .file = file, .gen = self.gen, .direct = direct };
            return;
        }
        if (self.map.count() >= self.cap) self.evictOldest();
        const key = try self.gpa.dupe(u8, path);
        errdefer self.gpa.free(key);
        try self.map.put(self.gpa, key, .{ .file = file, .gen = self.gen, .direct = direct });
    }

    fn evictOldest(self: *FdCache) void {
        var oldest_key: ?[]const u8 = null;
        var oldest_gen: u64 = std.math.maxInt(u64);
        var it = self.map.iterator();
        while (it.next()) |kv| if (kv.value_ptr.gen < oldest_gen) {
            oldest_gen = kv.value_ptr.gen;
            oldest_key = kv.key_ptr.*;
        };
        const key = oldest_key orelse return;
        const kv = self.map.fetchRemove(key).?;
        kv.value.file.release(self.gpa);
        self.gpa.free(kv.key);
    }

    fn deinit(self: *FdCache) void {
        var it = self.map.iterator();
        while (it.next()) |kv| {
            kv.value_ptr.file.release(self.gpa);
            self.gpa.free(kv.key_ptr.*);
        }
        self.map.deinit(self.gpa);
    }
};

// ---------------------------------------------------------------------------
// The reactor: one ring, slot-indexed in-flight ops, a reap/advance machine.
// ---------------------------------------------------------------------------

/// Who to tell when a write unit lands: one caller, or a batch aggregate.
const UnitDone = union(enum) {
    single: struct { done: *WriteDone, admitted_at: u64 },
    batch: *BatchState,
};

/// N slots share one reply: the first error wins, the reply goes out when the
/// last op lands (any failure fails the whole logical commit).
const BatchState = struct {
    remaining: usize,
    first_err: ?Error,
    done: *WriteDone,
    admitted_at: u64,
};

/// One in-flight op. OWNS everything its SQE references.
const Pending = union(enum) {
    read: struct { file: *CachedFile, buf: AlignedBuf, file_offset: u64, filled: usize, skip: usize, want: usize, grain: u64, done: *ReadDone },
    write: struct { file: *CachedFile, data: []const u8, bounce: ?AlignedBuf, file_offset: u64, written: usize, done: UnitDone },
    fsync: struct { file: *CachedFile, done: *SyncDone },
};

pub const InitError = error{ OutOfMemory, RingSetupFailed, ThreadSpawnFailed };

const Reactor = struct {
    gpa: Allocator,
    ring: IoUring,
    slots: std.ArrayList(?Pending) = .empty,
    free: std.ArrayList(u32) = .empty,
    inflight: usize = 0,
    cache: FdCache,

    fn init(gpa: Allocator, entries: u16, fd_cache_cap: u32) InitError!Reactor {
        var ring = IoUring.init(entries, 0) catch |e| {
            log.err("uring-fs: io_uring_setup({d}) failed: {s} — the io_uring file layer cannot start (charter §1.2: no classical fallback)", .{ entries, @errorName(e) });
            return error.RingSetupFailed;
        };
        errdefer ring.deinit();
        var self: Reactor = .{ .gpa = gpa, .ring = ring, .cache = .{ .gpa = gpa, .cap = fd_cache_cap } };
        // One burst (admit_cap) plus a resubmit burst never reallocates the
        // slot table; a batch wider than that grows it.
        try self.slots.ensureTotalCapacity(gpa, 2 * admit_cap);
        try self.free.ensureTotalCapacity(gpa, 2 * admit_cap);
        return self;
    }

    fn deinit(self: *Reactor) void {
        std.debug.assert(self.inflight == 0);
        self.cache.deinit();
        self.slots.deinit(self.gpa);
        self.free.deinit(self.gpa);
        self.ring.deinit();
    }

    fn claimSlot(self: *Reactor, p: Pending) error{OutOfMemory}!u32 {
        self.inflight += 1;
        errdefer self.inflight -= 1;
        if (self.free.pop()) |i| {
            self.slots.items[i] = p;
            return i;
        }
        try self.slots.append(self.gpa, p);
        return @intCast(self.slots.items.len - 1);
    }

    fn releaseSlot(self: *Reactor, i: u32) Pending {
        self.inflight -= 1;
        const p = self.slots.items[i].?;
        self.slots.items[i] = null;
        self.free.append(self.gpa, i) catch {
            // Capacity was reserved for 2×admit_cap; a wider batch may need
            // more. Losing a free-list entry only costs a slot of table
            // growth later — never correctness.
        };
        return p;
    }

    /// Open O_RDWR|O_CLOEXEC(|O_DIRECT) through the cache. WHY O_RDWR, never
    /// O_WRONLY: a write-only cached fd makes io_uring Read return EBADF.
    fn cachedOpen(self: *Reactor, path: []const u8, direct: bool, create: bool) Error!*CachedFile {
        if (self.cache.touch(path, direct)) |f| return f;
        const fd = try openPath(path, .{ .ACCMODE = .RDWR, .CLOEXEC = true, .DIRECT = direct, .CREAT = create }, 0o644);
        const file = try CachedFile.create(self.gpa, fd);
        // A cache insert that cannot allocate leaves the fd uncached: the one
        // reference is the caller's.
        self.cache.insert(path, file, direct) catch return file;
        file.retain();
        return file;
    }

    /// The control path's create|truncate open (config records, never a
    /// metadata device): BUFFERED posture, cached as such.
    fn openTruncate(self: *Reactor, path: []const u8) Error!*CachedFile {
        if (std.fs.path.dirname(path)) |parent| mkdirAll(parent);
        const fd = try openPath(path, .{ .ACCMODE = .RDWR, .CLOEXEC = true, .CREAT = true, .TRUNC = true }, 0o644);
        const file = try CachedFile.create(self.gpa, fd);
        self.cache.insert(path, file, false) catch return file;
        file.retain();
        return file;
    }

    fn admit(self: *Reactor, req: Request) void {
        // BEFORE the open: a cold fd-cache miss counts against the device span.
        const admitted_at = clock.monotonicNs();
        switch (req) {
            .write_all => |r| {
                const file = self.openTruncate(r.path) catch |e| return sendWriteNow(r.done, e, admitted_at);
                if (r.data.len == 0) {
                    file.release(self.gpa);
                    return sendWriteNow(r.done, {}, admitted_at);
                }
                self.enqueueWrite(file, r.data, null, 0, .{ .single = .{ .done = r.done, .admitted_at = admitted_at } });
            },
            .read_all => |r| {
                const st = (align_probe.statPath(r.path) catch null) orelse return r.done.complete(error.FileNotFound);
                const fd = openPath(r.path, .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0) catch |e| return r.done.complete(e);
                const file = CachedFile.create(self.gpa, fd) catch |e| return r.done.complete(e);
                if (st.size == 0) {
                    file.release(self.gpa);
                    return r.done.complete(ReadResult.empty(self.gpa));
                }
                const buf = AlignedBuf.zeroed(self.gpa, @intCast(st.size), 1) catch |e| {
                    file.release(self.gpa);
                    return r.done.complete(e);
                };
                self.enqueueRead(file, buf, 0, 0, @intCast(st.size), 1, r.done);
            },
            .read_at => |r| {
                const file = self.cachedOpen(r.path, r.direct, true) catch |e| {
                    var b = r.buf;
                    b.deinit();
                    return r.done.complete(e);
                };
                self.enqueueRead(file, r.buf, r.start, r.skip, r.want, r.grain, r.done);
            },
            .write_at => |r| {
                const file = self.cachedOpen(r.path, r.direct, true) catch |e| {
                    if (r.bounce) |b| {
                        var bb = b;
                        bb.deinit();
                    }
                    return sendWriteNow(r.done, e, admitted_at);
                };
                self.enqueueWrite(file, r.data, r.bounce, r.offset, .{ .single = .{ .done = r.done, .admitted_at = admitted_at } });
            },
            .write_at_batch => |r| {
                // The ops array is admission metadata; bounces move into slots.
                defer self.gpa.free(r.ops);
                const file = self.cachedOpen(r.path, r.direct, true) catch |e| {
                    for (r.ops) |op| if (op.bounce) |b| {
                        var bb = b;
                        bb.deinit();
                    };
                    return sendWriteNow(r.done, e, admitted_at);
                };
                const batch = self.gpa.create(BatchState) catch {
                    for (r.ops) |op| if (op.bounce) |b| {
                        var bb = b;
                        bb.deinit();
                    };
                    file.release(self.gpa);
                    return sendWriteNow(r.done, error.OutOfMemory, admitted_at);
                };
                batch.* = .{ .remaining = r.ops.len, .first_err = null, .done = r.done, .admitted_at = admitted_at };
                for (r.ops) |op| {
                    file.retain();
                    self.enqueueWrite(file, op.data, op.bounce, op.offset, .{ .batch = batch });
                }
                file.release(self.gpa);
            },
            .fdatasync => |r| {
                const file = self.cachedOpen(r.path, r.direct, false) catch |e| return r.done.complete(e);
                self.enqueueFsync(file, r.done);
            },
        }
    }

    /// Takes ownership of one `file` reference and of `buf`.
    fn enqueueRead(self: *Reactor, file: *CachedFile, buf: AlignedBuf, start: u64, skip: usize, want: usize, grain: u64, done: *ReadDone) void {
        const p: Pending = .{ .read = .{ .file = file, .buf = buf, .file_offset = start, .filled = 0, .skip = skip, .want = want, .grain = grain, .done = done } };
        const i = self.claimSlot(p) catch |e| return self.finishFailed(p, e, clock.monotonicNs());
        self.pushSlot(i);
    }

    /// Takes ownership of one `file` reference and of `bounce`.
    fn enqueueWrite(self: *Reactor, file: *CachedFile, data: []const u8, bounce: ?AlignedBuf, offset: u64, done: UnitDone) void {
        const p: Pending = .{ .write = .{ .file = file, .data = data, .bounce = bounce, .file_offset = offset, .written = 0, .done = done } };
        const i = self.claimSlot(p) catch |e| return self.finishFailed(p, e, clock.monotonicNs());
        self.pushSlot(i);
    }

    fn enqueueFsync(self: *Reactor, file: *CachedFile, done: *SyncDone) void {
        const p: Pending = .{ .fsync = .{ .file = file, .done = done } };
        const i = self.claimSlot(p) catch |e| return self.finishFailed(p, e, clock.monotonicNs());
        self.pushSlot(i);
    }

    /// Fill the SQE for slot `i`. Read/Write resume at their progress; every
    /// fdatasync is `IORING_FSYNC_DATASYNC`. No fixed files, no registered
    /// buffers, no IOSQE_ASYNC. `user_data` = the slot index.
    fn prepSqe(self: *Reactor, i: u32) bool {
        const sqe = self.ring.get_sqe() catch return false;
        const p = &self.slots.items[i].?;
        switch (p.*) {
            .read => |*r| sqe.prep_read(r.file.fd, r.buf.bytes[r.filled..], r.file_offset + r.filled),
            .write => |*w| sqe.prep_write(w.file.fd, w.data[w.written..], w.file_offset + w.written),
            .fsync => |*f| sqe.prep_fsync(f.file.fd, linux.IORING_FSYNC_DATASYNC),
        }
        sqe.user_data = i;
        return true;
    }

    /// A full SQ is normal backpressure (a single batch may carry more entries
    /// than the ring): flush it to the kernel with `submit()` — which consumes
    /// SQ entries immediately, independent of completions — and push again.
    /// Only a push that fails right after a successful flush is a real error,
    /// and it fails that op alone.
    fn pushSlot(self: *Reactor, i: u32) void {
        if (self.prepSqe(i)) return;
        _ = self.ring.submit() catch {};
        if (self.prepSqe(i)) return;
        const p = self.releaseSlot(i);
        self.finishFailed(p, error.SubmissionQueueOverflow, clock.monotonicNs());
    }

    /// A continuation's slot was just reaped, so an SQ entry is free: no
    /// flush-and-retry.
    fn pushContinue(self: *Reactor, i: u32) void {
        if (self.prepSqe(i)) return;
        const p = self.releaseSlot(i);
        self.finishFailed(p, error.SubmissionQueueOverflow, clock.monotonicNs());
    }

    /// Reap every ready CQE; one clock read per pass (the `woken_at` stamp);
    /// continuations are pushed after the CQEs are consumed. Returns whether
    /// the owned ring's wake READ completed.
    fn reap(self: *Reactor) bool {
        var saw_wake = false;
        var cqes: [64]linux.io_uring_cqe = undefined;
        while (true) {
            const n = self.ring.copy_cqes(&cqes, 0) catch |e| {
                log.err("uring-fs: completion queue read failed: {s} — failing every in-flight op", .{@errorName(e)});
                self.failAll(error.RingFailed);
                return saw_wake;
            };
            if (n == 0) break;
            const now = clock.monotonicNs();
            var cont: [64]u32 = undefined;
            var nc: usize = 0;
            for (cqes[0..n]) |cqe| {
                if (cqe.user_data == wake_ud) {
                    saw_wake = true;
                    continue;
                }
                const i: u32 = @intCast(cqe.user_data);
                if (self.advance(i, cqe.res, now)) {
                    cont[nc] = i;
                    nc += 1;
                }
            }
            for (cont[0..nc]) |i| self.pushContinue(i);
        }
        return saw_wake;
    }

    /// Apply one CQE. True ⇒ the op continues (short read/write) and must be
    /// re-pushed; false ⇒ the slot was released and the caller told.
    fn advance(self: *Reactor, i: u32, res: i32, now: u64) bool {
        const p = &self.slots.items[i].?;
        switch (p.*) {
            .read => |*r| {
                if (res > 0) {
                    r.filled += @intCast(res);
                    if (align_probe.shouldContinueRead(r.filled, r.buf.bytes.len, r.grain)) return true;
                }
                var done = self.releaseSlot(i).read;
                done.file.release(self.gpa);
                if (res < 0) {
                    done.buf.deinit();
                    done.done.complete(cqeError(res));
                } else {
                    const b = align_probe.readSliceBounds(done.filled, done.skip, done.want);
                    done.done.complete(ReadResult{ .buf = done.buf, .lo = b.lo, .hi = b.hi });
                }
            },
            .write => |*w| {
                if (res > 0) {
                    w.written += @intCast(res);
                    if (w.written < w.data.len) return true;
                }
                var done = self.releaseSlot(i).write;
                done.file.release(self.gpa);
                if (done.bounce) |*b| b.deinit();
                const result: Error!void = if (res < 0) cqeError(res) else if (res == 0) error.ShortWrite else {};
                self.completeUnit(done.done, result, now);
            },
            .fsync => {
                const done = self.releaseSlot(i).fsync;
                done.file.release(self.gpa);
                done.done.complete(if (res < 0) cqeError(res) else {});
            },
        }
        return false;
    }

    fn completeUnit(self: *Reactor, unit: UnitDone, result: Error!void, now: u64) void {
        switch (unit) {
            .single => |s| s.done.complete(.{ .result = result, .admitted_at = s.admitted_at, .woken_at = now }),
            .batch => |b| {
                result catch |e| if (b.first_err == null) {
                    b.first_err = e;
                };
                b.remaining -= 1;
                if (b.remaining == 0) {
                    b.done.complete(.{ .result = if (b.first_err) |e| e else {}, .admitted_at = b.admitted_at, .woken_at = now });
                    self.gpa.destroy(b);
                }
            },
        }
    }

    /// A slot that never got its SQE (or the ring broke): release everything
    /// it owns and tell the caller.
    fn finishFailed(self: *Reactor, p: Pending, err: Error, now: u64) void {
        switch (p) {
            .read => |r| {
                var rr = r;
                rr.file.release(self.gpa);
                rr.buf.deinit();
                rr.done.complete(err);
            },
            .write => |w| {
                var ww = w;
                ww.file.release(self.gpa);
                if (ww.bounce) |*b| b.deinit();
                self.completeUnit(ww.done, err, now);
            },
            .fsync => |f| {
                f.file.release(self.gpa);
                f.done.complete(err);
            },
        }
    }

    fn failAll(self: *Reactor, err: Error) void {
        const now = clock.monotonicNs();
        for (self.slots.items, 0..) |slot, i| {
            if (slot == null) continue;
            const p = self.releaseSlot(@intCast(i));
            self.finishFailed(p, err, now);
        }
    }
};

fn sendWriteNow(done: *WriteDone, result: Error!void, now: u64) void {
    done.complete(.{ .result = result, .admitted_at = now, .woken_at = now });
}

// ---------------------------------------------------------------------------
// The pool.
// ---------------------------------------------------------------------------

const Queue = Mpmc(Request, queue_cap);

pub const Pool = struct {
    gpa: Allocator,
    config: Config,
    queue: *Queue,
    reactors: []Reactor,
    threads: []std.Thread,

    /// Rings are created HERE, on the caller, so a box that cannot do io_uring
    /// refuses at startup rather than degrading a worker later.
    pub fn init(gpa: Allocator, config: Config) InitError!*Pool {
        // `resolveWorkerCount` floors at 1; a zero-worker pool would park every caller forever.
        std.debug.assert(config.workers >= 1);
        const self = try gpa.create(Pool);
        errdefer gpa.destroy(self);
        self.* = .{ .gpa = gpa, .config = config, .queue = undefined, .reactors = &.{}, .threads = &.{} };
        self.queue = try gpa.create(Queue);
        errdefer gpa.destroy(self.queue);
        self.queue.init();

        self.reactors = try gpa.alloc(Reactor, config.workers);
        errdefer gpa.free(self.reactors);
        var made: usize = 0;
        errdefer for (self.reactors[0..made]) |*r| r.deinit();
        while (made < self.reactors.len) : (made += 1) {
            self.reactors[made] = try Reactor.init(gpa, ring_entries, config.fd_cache_cap);
        }

        self.threads = try gpa.alloc(std.Thread, config.workers);
        errdefer gpa.free(self.threads);
        var spawned: usize = 0;
        errdefer {
            self.queue.close();
            for (self.threads[0..spawned]) |t| t.join();
        }
        while (spawned < self.threads.len) : (spawned += 1) {
            self.threads[spawned] = std.Thread.spawn(.{}, workerMain, .{ self, @as(u32, @intCast(spawned)) }) catch |e| {
                log.err("uring-fs: spawning worker {d} failed: {s}", .{ spawned, @errorName(e) });
                return error.ThreadSpawnFailed;
            };
        }
        publishConfigInForce(config);
        return self;
    }

    /// Close the queue so every idle worker's `recvBlocking` ends, join them
    /// (a worker with in-flight ops finishes them first), then fail anything a
    /// producer slipped in between the last drain and the join.
    pub fn deinit(self: *Pool) void {
        self.queue.close();
        for (self.threads) |t| t.join();
        while (self.queue.tryRecv()) |req| failRequest(self.gpa, req, error.WorkerClosed);
        for (self.reactors) |*r| r.deinit();
        self.gpa.free(self.threads);
        self.gpa.free(self.reactors);
        self.gpa.destroy(self.queue);
        self.gpa.destroy(self);
    }

    pub fn workerCount(self: *const Pool) usize {
        return self.threads.len;
    }

    /// Non-blocking push. Full ⇒ `QueueFull` (counted); closed ⇒ `WorkerClosed`.
    fn send(self: *Pool, req: Request) Error!void {
        self.queue.trySend(req) catch |e| switch (e) {
            error.Full => {
                stats.queue_full.inc();
                return error.QueueFull;
            },
            error.Closed => return error.WorkerClosed,
        };
    }

    /// Create|truncate the whole file (control path: buffered by definition);
    /// parents are created.
    pub fn writeAll(self: *Pool, path: []const u8, data: []const u8) Error!void {
        const done = try WriteDone.create(self.gpa);
        var handle: WriteCompletion = .{ .done = done, .submitted_at = clock.monotonicNs(), .observed = null };
        self.send(.{ .write_all = .{ .path = path, .data = data, .done = done } }) catch |e| {
            done.destroyUnsent();
            return e;
        };
        return handle.wait();
    }

    /// The whole file; length from stat.
    pub fn readAll(self: *Pool, path: []const u8) Error!ReadResult {
        const done = try ReadDone.create(self.gpa);
        self.send(.{ .read_all = .{ .path = path, .done = done } }) catch |e| {
            done.destroyUnsent();
            return e;
        };
        const result = done.wait();
        done.release();
        return result;
    }

    /// Positional read. On a direct path the window is widened to the grain
    /// into an aligned buffer and sliced on completion.
    pub fn readAt(self: *Pool, path: []const u8, offset: u64, size: usize) Error!ReadResult {
        const mode = metaIoMode(path) orelse @as(MetaIoMode, .buffered);
        const span = align_probe.readSpan(offset, size, mode.grain());
        if (span.widened) stats.read_widened.inc();
        var buf = try AlignedBuf.zeroed(self.gpa, @intCast(span.len), mode.memAlign());
        if (size == 0) return .{ .buf = buf, .lo = 0, .hi = 0 };
        errdefer buf.deinit();
        const done = try ReadDone.create(self.gpa);
        self.send(.{ .read_at = .{ .path = path, .start = span.start, .skip = span.skip, .want = size, .grain = mode.grain(), .direct = mode.isDirect(), .buf = buf, .done = done } }) catch |e| {
            done.destroyUnsent();
            return e;
        };
        const result = done.wait();
        done.release();
        return result;
    }

    /// Positional write with the direct preflight (refuse / bounce). `data`
    /// must stay alive until this returns.
    pub fn writeAt(self: *Pool, path: []const u8, offset: u64, data: []const u8) Error!void {
        var handle = try self.submitWriteAt(path, offset, data);
        return handle.wait();
    }

    fn submitWriteAt(self: *Pool, path: []const u8, offset: u64, data: []const u8) Error!WriteCompletion {
        if (data.len == 0) return WriteCompletion.alreadyDone({});
        const mode = metaIoMode(path) orelse @as(MetaIoMode, .buffered);
        var prepared = try prepareDirectWrite(self.gpa, mode, path, offset, data);
        errdefer prepared.deinit();
        const done = try WriteDone.create(self.gpa);
        const submitted_at = clock.monotonicNs();
        self.send(.{ .write_at = .{ .path = path, .offset = offset, .data = prepared.data, .bounce = prepared.bounce, .direct = mode.isDirect(), .done = done } }) catch |e| {
            done.destroyUnsent();
            return e;
        };
        return .{ .done = done, .submitted_at = submitted_at, .observed = null };
    }

    /// One logical commit of N positional writes: every op is preflighted
    /// before any byte is submitted (a misaligned op fails the whole batch),
    /// N parallel SQEs, one reply. `ops` and their data must stay alive until
    /// the completion is observed. 0 ops ⇒ already done; 1 op ⇒ a plain write.
    pub fn submitWriteAtBatch(self: *Pool, path: []const u8, ops: []const WriteOp) Error!WriteCompletion {
        var live: usize = 0;
        for (ops) |op| if (op.data.len != 0) {
            live += 1;
        };
        if (live == 0) return WriteCompletion.alreadyDone({});
        if (live == 1) {
            for (ops) |op| if (op.data.len != 0) return self.submitWriteAt(path, op.offset, op.data);
        }
        const mode = metaIoMode(path) orelse @as(MetaIoMode, .buffered);
        const prepared = try self.gpa.alloc(PreparedOp, live);
        var n: usize = 0;
        errdefer {
            for (prepared[0..n]) |*p| if (p.bounce) |*b| b.deinit();
            self.gpa.free(prepared);
        }
        for (ops) |op| {
            if (op.data.len == 0) continue;
            const p = try prepareDirectWrite(self.gpa, mode, path, op.offset, op.data);
            prepared[n] = .{ .offset = op.offset, .data = p.data, .bounce = p.bounce };
            n += 1;
        }
        const done = try WriteDone.create(self.gpa);
        const submitted_at = clock.monotonicNs();
        self.send(.{ .write_at_batch = .{ .path = path, .ops = prepared, .direct = mode.isDirect(), .done = done } }) catch |e| {
            done.destroyUnsent();
            return e;
        };
        return .{ .done = done, .submitted_at = submitted_at, .observed = null };
    }

    pub fn writeAtBatch(self: *Pool, path: []const u8, ops: []const WriteOp) Error!void {
        var handle = try self.submitWriteAtBatch(path, ops);
        return handle.wait();
    }

    /// `IORING_FSYNC_DATASYNC` on the cached O_RDWR fd.
    pub fn fdatasync(self: *Pool, path: []const u8) Error!void {
        const mode = metaIoMode(path) orelse @as(MetaIoMode, .buffered);
        const done = try SyncDone.create(self.gpa);
        self.send(.{ .fdatasync = .{ .path = path, .direct = mode.isDirect(), .done = done } }) catch |e| {
            done.destroyUnsent();
            return e;
        };
        const result = done.wait();
        done.release();
        return result;
    }
};

/// The worker loop (`docs/porting/uring_fs.md` §1.9): park in the queue when
/// nothing is in flight, else burst-admit up to `admit_cap`, one
/// `submit_and_wait(1)`, reap. Exits when the queue is closed and drained and
/// nothing is in flight.
fn workerMain(pool: *Pool, index: u32) void {
    var name_buf: thread_name.CommBuf = undefined;
    var base: [24]u8 = undefined;
    _ = thread_name.nameCurrentThread(&name_buf, workerBase(&base, index));
    const r = &pool.reactors[index];
    while (true) {
        if (r.inflight == 0) {
            const req = pool.queue.recvBlocking() orelse return;
            r.admit(req);
        }
        while (r.inflight < admit_cap) {
            const req = pool.queue.tryRecv() orelse break;
            r.admit(req);
        }
        if (r.inflight == 0) continue;
        _ = r.ring.submit_and_wait(1) catch |e| switch (e) {
            error.SignalInterrupt => continue,
            else => {
                log.err("uring-fs: worker {d}: io_uring_enter failed: {s} — the ring is dead; failing every in-flight op and every later request on this worker (charter §1.2: no classical fallback)", .{ index, @errorName(e) });
                r.failAll(error.RingFailed);
                // The failing lane: this worker keeps draining its share of the
                // queue so callers see a loud error instead of a hang.
                while (pool.queue.recvBlocking()) |req| failRequest(pool.gpa, req, error.RingFailed);
                return;
            },
        };
        _ = r.reap();
    }
}

// ---------------------------------------------------------------------------
// The O_DIRECT metadata posture (`docs/porting/uring_fs.md` §3).
// ---------------------------------------------------------------------------

pub const MetaIoMode = union(enum) {
    buffered,
    direct: struct { grain: u64, mem_align: usize },

    pub fn grain(self: MetaIoMode) u64 {
        return switch (self) {
            .buffered => 1,
            .direct => |d| d.grain,
        };
    }

    pub fn memAlign(self: MetaIoMode) usize {
        return switch (self) {
            .buffered => 1,
            .direct => |d| d.mem_align,
        };
    }

    pub fn isDirect(self: MetaIoMode) bool {
        return self == .direct;
    }
};

/// Registry memory outlives every pool and test: process-lifetime allocator.
const registry_gpa: Allocator = std.heap.smp_allocator;

/// An immutable registry snapshot: path (as given AND canonical) → posture,
/// `null` = a remembered NEGATIVE. Keys live in the snapshot's arena.
const Snapshot = struct {
    arena: std.heap.ArenaAllocator,
    map: std.StringHashMapUnmanaged(?MetaIoMode),
    is_static: bool = false,

    fn create() error{OutOfMemory}!*Snapshot {
        const s = try registry_gpa.create(Snapshot);
        s.* = .{ .arena = .init(registry_gpa), .map = .empty };
        return s;
    }

    fn put(self: *Snapshot, key: []const u8, value: ?MetaIoMode) error{OutOfMemory}!void {
        const a = self.arena.allocator();
        if (self.map.getPtr(key)) |v| {
            v.* = value;
            return;
        }
        try self.map.put(a, try a.dupe(u8, key), value);
    }

    /// Copy `old` into this snapshot, optionally dropping its negatives.
    fn copyFrom(self: *Snapshot, old: *const Snapshot, keep_negatives: bool) error{OutOfMemory}!void {
        var it = old.map.iterator();
        while (it.next()) |kv| {
            if (!keep_negatives and kv.value_ptr.* == null) continue;
            try self.put(kv.key_ptr.*, kv.value_ptr.*);
        }
    }

    fn destroy(self: *Snapshot) void {
        if (self.is_static) return;
        self.arena.deinit();
        registry_gpa.destroy(self);
    }
};

var registry_empty: Snapshot = .{ .arena = .init(registry_gpa), .map = .empty, .is_static = true };

/// Per-thread-striped reader count for one epoch bucket (a core-local RMW on
/// the hot path, a fold on the rare writer path).
const ReaderCount = struct {
    const Line = struct { v: std.atomic.Value(u32) align(std.atomic.cache_line) = .init(0) };
    lines: [counters.max_stripes]Line = @splat(.{}),

    fn inc(self: *ReaderCount) void {
        // seq_cst: ordered before the reader's epoch re-check and visible to
        // the writer's post-flip fold (see Registry.enter).
        _ = self.lines[counters.stripeIndex()].v.fetchAdd(1, .seq_cst);
    }

    fn dec(self: *ReaderCount) void {
        // release pairs with the writer's acquire fold: the free happens
        // after this reader's last access to the snapshot.
        _ = self.lines[counters.stripeIndex()].v.fetchSub(1, .release);
    }

    fn fold(self: *const ReaderCount) u32 {
        var sum: u32 = 0;
        for (&self.lines) |*l| sum +%= l.v.load(.acquire);
        return sum;
    }
};

/// The hot-path lookup pays ONE pointer load and one hash probe (plus a
/// core-local reader stamp); writers never block readers. Reclamation is
/// epoch-based: a reader stamps the bucket of the epoch it observed, re-checks
/// the epoch, and only then loads the snapshot; a writer swaps the snapshot,
/// flips the epoch, and frees the old snapshot once the OLD bucket folds to 0.
/// WHY not a RwLock: the reference measured the RwLock form at three reader
/// acquisitions and a canonicalize(2) per MISS per I/O; this keeps the miss
/// path to one canonicalize per distinct spelling and the hit path lock-free.
const Registry = struct {
    current: std.atomic.Value(*Snapshot) = .init(&registry_empty),
    epoch: std.atomic.Value(u32) = .init(0),
    readers: [2]ReaderCount = .{ .{}, .{} },
    writer: FutexMutex = .{},

    const Lookup = union(enum) { hit: ?MetaIoMode, miss };

    /// Reader protocol. A reader counted in bucket `e & 1` observed
    /// `epoch == e` AFTER its increment, so its increment precedes the next
    /// flip in the seq_cst order and the writer's post-flip fold sees it until
    /// the matching decrement. A reader that sees a flipped epoch retracts and
    /// retries in the new bucket, so a bucket drains once in-progress probes
    /// finish (a hash probe — microseconds).
    fn enter(self: *Registry) struct { snap: *Snapshot, bucket: u1 } {
        while (true) {
            const e = self.epoch.load(.seq_cst);
            const bucket: u1 = @truncate(e);
            self.readers[bucket].inc();
            if (self.epoch.load(.seq_cst) == e) {
                // acquire pairs with the writer's release store of `current`.
                return .{ .snap = self.current.load(.acquire), .bucket = bucket };
            }
            self.readers[bucket].dec();
        }
    }

    fn leave(self: *Registry, bucket: u1) void {
        self.readers[bucket].dec();
    }

    fn lookup(self: *Registry, path: []const u8) Lookup {
        const r = self.enter();
        defer self.leave(r.bucket);
        if (r.snap.map.get(path)) |v| return .{ .hit = v };
        return .miss;
    }

    /// Writer protocol: build the successor under the writer lock, publish it,
    /// flip the epoch, drain the old bucket, free the old snapshot.
    fn publish(self: *Registry, new: *Snapshot) void {
        const old = self.current.swap(new, .acq_rel);
        const e = self.epoch.fetchAdd(1, .seq_cst);
        const old_bucket: u1 = @truncate(e);
        var spins: u32 = 0;
        while (self.readers[old_bucket].fold() != 0) : (spins += 1) {
            if (spins < 64) std.atomic.spinLoopHint() else _ = linux.sched_yield();
        }
        old.destroy();
    }

    /// Remember a resolved lookup. No-op if already known, or if negative and
    /// the negative cache is at its cap (bounded by the fd cache cap).
    fn remember(self: *Registry, path: []const u8, mode: ?MetaIoMode) void {
        self.writer.lock();
        defer self.writer.unlock();
        const old = self.current.load(.acquire);
        if (old.map.contains(path)) return;
        if (mode == null and old.map.count() >= configInForce().fd_cache_cap) return;
        const new = Snapshot.create() catch return;
        new.copyFrom(old, true) catch return new.destroy();
        new.put(path, mode) catch return new.destroy();
        self.publish(new);
    }

    /// REBUILD keeping only positives — an alias spelling seen before the
    /// registration must not stay BUFFERED forever — then insert both
    /// spellings.
    fn register(self: *Registry, path: []const u8, canonical: ?[]const u8, mode: MetaIoMode) error{OutOfMemory}!void {
        self.writer.lock();
        defer self.writer.unlock();
        const old = self.current.load(.acquire);
        const new = try Snapshot.create();
        errdefer new.destroy();
        try new.copyFrom(old, false);
        try new.put(path, mode);
        if (canonical) |c| try new.put(c, mode);
        self.publish(new);
    }

    fn clear(self: *Registry) void {
        self.writer.lock();
        defer self.writer.unlock();
        const new = Snapshot.create() catch return;
        self.publish(new);
    }
};

var registry: Registry = .{};

/// `realpath` without libc: open O_PATH, read the `/proc/self/fd/N` link.
/// Null when the path does not resolve.
pub fn canonicalize(gpa: Allocator, path: []const u8) error{OutOfMemory}!?[]u8 {
    var zbuf: [linux.PATH_MAX]u8 = undefined;
    const z = align_probe.pathZ(&zbuf, path) catch return null;
    const rc = linux.open(z, .{ .PATH = true, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) return null;
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    var link: [64]u8 = undefined;
    const lz = std.mem.printSentinel(&link, "/proc/self/fd/{d}", .{fd}, 0) catch return null;
    var out: [linux.PATH_MAX]u8 = undefined;
    const n = linux.readlink(lz, &out, out.len);
    if (linux.errno(n) != .SUCCESS) return null;
    return try gpa.dupe(u8, out[0..n]);
}

/// Registry lookup: one snapshot probe. The first miss for a spelling
/// canonicalizes ONCE, probes the canonical spelling, and remembers the
/// answer (positive or negative) for the given spelling.
pub fn metaIoMode(path: []const u8) ?MetaIoMode {
    switch (registry.lookup(path)) {
        .hit => |m| return m,
        .miss => {},
    }
    const canon = canonicalize(registry_gpa, path) catch null;
    defer if (canon) |c| registry_gpa.free(c);
    const resolved: ?MetaIoMode = if (canon) |c| switch (registry.lookup(c)) {
        .hit => |m| m,
        .miss => null,
    } else null;
    stats.mode_resolves.inc();
    registry.remember(path, resolved);
    return resolved;
}

/// `mode.grain` or 1 for an unregistered/buffered path.
pub fn metaIoGrain(path: []const u8) u64 {
    return (metaIoMode(path) orelse @as(MetaIoMode, .buffered)).grain();
}

/// Registry reset (test harness).
pub fn clearMetaDevices() void {
    registry.clear();
}

/// Probe the grain, admit it, trial-open O_DIRECT, record the posture.
/// Idempotent. The ONE product fallback: EINVAL from the trial open on a
/// REGULAR FILE ⇒ BUFFERED, loud, counted. A block device refusing O_DIRECT
/// for any reason, or a regular file failing with any other errno, REFUSES.
pub fn registerMetaDevice(gpa: Allocator, env: *const std.process.Environ.Map, path: []const u8) RegisterError!MetaIoMode {
    if (metaIoMode(path)) |m| return m;
    const st = (align_probe.statPath(path) catch null) orelse {
        log.err("metadata device {s}: cannot stat", .{path});
        return error.CannotStat;
    };
    if (!st.is_bdev and !st.is_file) {
        log.err("metadata device {s}: neither a block device nor a regular file — REFUSING", .{path});
        return error.NotADeviceOrFile;
    }
    const probe = align_probe.probeGrain(path);
    const grain = align_probe.admitMetaIoGrain(probe.grain) catch {
        var buf: [512]u8 = undefined;
        log.err("{s}", .{align_probe.formatGrainRefusal(&buf, path, probe.source, probe.grain)});
        return error.GrainNotAdmissible;
    };
    const mem_align = align_probe.memAlignFor(probe.mem_align, grain);

    const mode: MetaIoMode = if (knobs.boolKnob(env, "SQUEEZEFS_TEST_META_BUFFERED", false)) seam: {
        log.warn("metadata device {s}: SQUEEZEFS_TEST_META_BUFFERED=1 keeps it BUFFERED (the pre-posture control arm); meta_io_buffered_fallback counts it", .{path});
        stats.buffered_fallback.inc();
        break :seam .buffered;
    } else trial: {
        // The TRIAL open; the fd is dropped at once — the workers open their own.
        if (openPath(path, .{ .ACCMODE = .RDWR, .CLOEXEC = true, .DIRECT = true }, 0)) |fd| {
            _ = linux.close(fd);
            stats.direct_paths.inc();
            log.info("metadata device {s}: O_DIRECT (grain {d} B from {s}, buffer alignment {d} B) — every byte bypasses the host page cache", .{ path, grain, probe.source.label(), mem_align });
            break :trial .{ .direct = .{ .grain = grain, .mem_align = mem_align } };
        } else |e| {
            if (e == error.InvalidArgument and !st.is_bdev) {
                stats.buffered_fallback.inc();
                log.err("metadata volume {s}: the filesystem refuses O_DIRECT (EINVAL) — falling back to BUFFERED; a shared LUN must never see this (meta_io_buffered_fallback counts it)", .{path});
                break :trial .buffered;
            }
            log.err("metadata device {s}: O_DIRECT open failed ({s}); shared-LUN metadata I/O must bypass the host page cache (design-symmetric-metadata §5.12) and a block device that refuses it cannot serve a coherent set — REFUSING", .{ path, @errorName(e) });
            return error.DirectOpenRefused;
        }
    };

    const canon = try canonicalize(gpa, path);
    defer if (canon) |c| gpa.free(c);
    try registry.register(path, canon, mode);
    return mode;
}

/// A prepared direct write: the slice to submit and the bounce it may own.
pub const Prepared = struct {
    data: []const u8,
    bounce: ?AlignedBuf,

    pub fn deinit(self: *Prepared) void {
        if (self.bounce) |*b| b.deinit();
        self.bounce = null;
    }
};

/// The write law. Buffered ⇒ nothing. Direct: a misaligned offset or length
/// is REFUSED (a caller bug — the aligned form belongs beside the shape;
/// there is no RMW for a product write); a misaligned POINTER is bounced into
/// an aligned copy (`meta_io_bounce_bytes`). Only the pointer is checked for
/// the buffer; its length was already checked against the grain.
pub fn prepareDirectWrite(gpa: Allocator, mode: MetaIoMode, path: []const u8, offset: u64, data: []const u8) Error!Prepared {
    const d = switch (mode) {
        .buffered => return .{ .data = data, .bounce = null },
        .direct => |d| d,
    };
    if (!align_probe.writeShapeAligned(offset, data.len, d.grain)) {
        var buf: [512]u8 = undefined;
        const detail = std.fmt.bufPrint(&buf, "direct metadata write at offset {d} of {d} bytes to {s} is not aligned to the device grain ({d} B) — a caller bug: the aligned form belongs beside the shape", .{ offset, data.len, path, d.grain }) catch "direct metadata write misaligned";
        stats.unaligned_refusals.fire(detail);
        return error.UnalignedDirectWrite;
    }
    if (!align_probe.ptrAligned(data.ptr, d.mem_align)) {
        const bounce = try AlignedBuf.fromSlice(gpa, data, d.mem_align);
        stats.bounce_bytes.add(data.len);
        return .{ .data = bounce.bytes, .bounce = bounce };
    }
    return .{ .data = data, .bounce = null };
}

/// Zero-extend an image to the path's grain multiple (the slot-tails spill).
pub fn padToGrain(gpa: Allocator, path: []const u8, image: *std.ArrayList(u8)) error{OutOfMemory}!void {
    const target = align_probe.paddedLen(image.items.len, metaIoGrain(path));
    try image.appendNTimes(gpa, 0, target - image.items.len);
}

// ---------------------------------------------------------------------------
// The owned ring (the journal lane's): the same reactor, driven by the thread
// that also awaits completions, parked in io_uring_enter with an eventfd READ
// armed so a task wake and a device CQE are ONE wait.
// ---------------------------------------------------------------------------

/// Cross-thread wake of a parked owned ring: `write(eventfd, 1u64)`. The
/// counter is sticky, so a park not yet entered returns at once.
pub const RingWake = struct {
    fd: linux.fd_t,

    pub fn wake(self: RingWake) void {
        const one: u64 = 1;
        // EAGAIN (counter at u64::MAX−1) is impossible at one increment per
        // wake and harmless: the park is already pending.
        _ = linux.write(self.fd, @ptrCast(&one), @sizeOf(u64));
    }
};

pub const OwnedRing = struct {
    gpa: Allocator,
    reactor: Reactor,
    event_fd: linux.fd_t,
    /// The wake READ's target: heap, stable address across parks.
    wake_buf: *u64,
    wake_armed: bool = false,

    pub fn init(gpa: Allocator, entries: u16, fd_cache_cap: u32) InitError!*OwnedRing {
        const self = try gpa.create(OwnedRing);
        errdefer gpa.destroy(self);
        const efd_rc = linux.eventfd(0, linux.EFD.CLOEXEC | linux.EFD.NONBLOCK);
        if (linux.errno(efd_rc) != .SUCCESS) {
            log.err("uring-fs: eventfd for the owned ring failed: {s}", .{@tagName(linux.errno(efd_rc))});
            return error.RingSetupFailed;
        }
        const efd: linux.fd_t = @intCast(efd_rc);
        errdefer _ = linux.close(efd);
        const wake_buf = try gpa.create(u64);
        errdefer gpa.destroy(wake_buf);
        wake_buf.* = 0;
        self.* = .{ .gpa = gpa, .reactor = try Reactor.init(gpa, entries, fd_cache_cap), .event_fd = efd, .wake_buf = wake_buf };
        return self;
    }

    /// Drains anything still in flight (the kernel may still be writing into
    /// those buffers), then tears down. The armed wake READ is cancelled by
    /// the ring's close; its buffer is freed after.
    pub fn deinit(self: *OwnedRing) void {
        while (self.reactor.inflight != 0) {
            _ = self.reactor.ring.submit_and_wait(1) catch |e| switch (e) {
                error.SignalInterrupt => continue,
                else => {
                    self.reactor.failAll(error.RingFailed);
                    break;
                },
            };
            _ = self.reactor.reap();
        }
        self.reactor.deinit();
        _ = linux.close(self.event_fd);
        self.gpa.destroy(self.wake_buf);
        self.gpa.destroy(self);
    }

    pub fn wakeHandle(self: *const OwnedRing) RingWake {
        return .{ .fd = self.event_fd };
    }

    /// Admit a batch on the owning thread and `submit()` without waiting. The
    /// completion is observed through `park`/`service`.
    pub fn submitWriteAtBatch(self: *OwnedRing, path: []const u8, ops: []const WriteOp) Error!WriteCompletion {
        var live: usize = 0;
        for (ops) |op| if (op.data.len != 0) {
            live += 1;
        };
        if (live == 0) return WriteCompletion.alreadyDone({});
        const mode = metaIoMode(path) orelse @as(MetaIoMode, .buffered);
        const prepared = try self.gpa.alloc(PreparedOp, live);
        var n: usize = 0;
        errdefer {
            for (prepared[0..n]) |*p| if (p.bounce) |*b| b.deinit();
            self.gpa.free(prepared);
        }
        for (ops) |op| {
            if (op.data.len == 0) continue;
            const p = try prepareDirectWrite(self.gpa, mode, path, op.offset, op.data);
            prepared[n] = .{ .offset = op.offset, .data = p.data, .bounce = p.bounce };
            n += 1;
        }
        const done = try WriteDone.create(self.gpa);
        const submitted_at = clock.monotonicNs();
        self.reactor.admit(.{ .write_at_batch = .{ .path = path, .ops = prepared, .direct = mode.isDirect(), .done = done } });
        _ = self.reactor.ring.submit() catch |e| switch (e) {
            error.SignalInterrupt => {},
            else => {
                log.err("uring-fs: owned ring submit failed: {s} — failing every in-flight op", .{@errorName(e)});
                self.reactor.failAll(error.RingFailed);
            },
        };
        return .{ .done = done, .submitted_at = submitted_at, .observed = null };
    }

    /// Park until a CQE (a device completion or the wake READ) or the tick.
    /// True ⇒ the tick fired. The wake READ is armed once and re-armed only
    /// after it completes.
    pub fn park(self: *OwnedRing, tick_ns: u64) bool {
        if (!self.wake_armed) self.armWake();
        const ring = &self.reactor.ring;
        // Flush pending SQEs in their own enter FIRST: when io_uring_enter
        // submits anything it returns the submitted count and DISCARDS the
        // wait's verdict, so a combined submit-and-timed-wait never reports
        // ETIME (liburing waits in a separate to_submit=0 call for the same
        // reason).
        _ = ring.submit() catch |e| switch (e) {
            error.SignalInterrupt => {},
            else => {
                log.err("uring-fs: owned ring submit failed: {s} — failing every in-flight op", .{@errorName(e)});
                self.reactor.failAll(error.RingFailed);
                return false;
            },
        };
        if (ring.features & linux.IORING_FEAT_EXT_ARG == 0) return self.untimedWait();
        var ts: linux.kernel_timespec = .{ .sec = @intCast(tick_ns / std.time.ns_per_s), .nsec = @intCast(tick_ns % std.time.ns_per_s) };
        var arg: linux.io_uring_getevents_arg = .{ .sigmask = 0, .sigmask_sz = 0, .pad = 0, .ts = @intFromPtr(&ts) };
        // The kernel reads `arg`/`ts` during this call only.
        const rc = linux.syscall6(.io_uring_enter, @as(u32, @bitCast(ring.fd)), 0, 1, linux.IORING_ENTER_GETEVENTS | linux.IORING_ENTER_EXT_ARG, @intFromPtr(&arg), @sizeOf(linux.io_uring_getevents_arg));
        return switch (linux.errno(rc)) {
            .SUCCESS => false,
            .TIME => true,
            .INTR => false,
            // Pre-5.11 kernels without EXT_ARG: an untimed wait.
            .INVAL => self.untimedWait(),
            else => |e| {
                log.err("uring-fs: owned ring io_uring_enter failed: {s} — failing every in-flight op", .{@tagName(e)});
                self.reactor.failAll(error.RingFailed);
                return false;
            },
        };
    }

    fn untimedWait(self: *OwnedRing) bool {
        _ = self.reactor.ring.submit_and_wait(1) catch |e| switch (e) {
            error.SignalInterrupt => {},
            else => {
                log.err("uring-fs: owned ring io_uring_enter failed: {s} — failing every in-flight op", .{@errorName(e)});
                self.reactor.failAll(error.RingFailed);
            },
        };
        return false;
    }

    fn armWake(self: *OwnedRing) void {
        const ring = &self.reactor.ring;
        const sqe = ring.get_sqe() catch blk: {
            _ = ring.submit() catch {};
            break :blk ring.get_sqe() catch {
                log.err("uring-fs: owned ring cannot arm its wake READ (SQ full after a flush)", .{});
                return;
            };
        };
        sqe.prep_read(self.event_fd, std.mem.asBytes(self.wake_buf), 0);
        sqe.user_data = wake_ud;
        self.wake_armed = true;
    }

    /// Reap on the owning thread; a completed wake READ re-arms at the next park.
    pub fn service(self: *OwnedRing) void {
        if (self.reactor.reap()) self.wake_armed = false;
    }
};

// ---------------------------------------------------------------------------
// Tests: the contract (`docs/porting/uring_fs.md` §6).
// ---------------------------------------------------------------------------

const testing = std.testing;

const TmpPath = struct {
    tmp: testing.TmpDir,
    dir: [std.fs.max_path_bytes]u8 = undefined,
    dir_len: usize = 0,

    fn init() !TmpPath {
        var t: TmpPath = .{ .tmp = testing.tmpDir(.{}) };
        t.dir_len = try t.tmp.dir.realPath(testing.io, &t.dir);
        return t;
    }

    fn join(self: *const TmpPath, buf: []u8, name: []const u8) []const u8 {
        return std.fmt.bufPrint(buf, "{s}/{s}", .{ self.dir[0..self.dir_len], name }) catch unreachable;
    }

    fn deinit(self: *TmpPath) void {
        self.tmp.cleanup();
    }
};

fn testPool(workers: u32) !*Pool {
    return Pool.init(testing.allocator, Config.fixed(workers, 1024));
}

test "worker count derives from process cores: clamp(cpus/4, 4, 64); the env override clamps to 1..=64; junk derives" {
    try testing.expectEqual(@as(u32, 8), resolveWorkerCount(null, 32));
    try testing.expectEqual(@as(u32, 4), resolveWorkerCount(null, 2));
    try testing.expectEqual(@as(u32, 16), resolveWorkerCount(null, 64));
    try testing.expectEqual(@as(u32, 64), resolveWorkerCount(null, 512));
    try testing.expectEqual(@as(u32, 2), resolveWorkerCount(2, 32));
    try testing.expectEqual(@as(u32, 64), resolveWorkerCount(999, 32));
    try testing.expectEqual(@as(u32, 1), resolveWorkerCount(0, 32));
    // The fleet-share floor: a tiny share still gets the shipped floor.
    try testing.expectEqual(@as(u32, 4), resolveWorkerCount(null, 1));

    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    try testing.expectEqual(@as(u32, 8), Config.resolve(&env, derive.Facts.synthetic(32, 0)).workers);
    try env.put("SQUEEZEFS_URING_FS_WORKERS", "2");
    try testing.expectEqual(@as(u32, 2), Config.resolve(&env, derive.Facts.synthetic(32, 0)).workers);
    try env.put("SQUEEZEFS_URING_FS_WORKERS", "junk");
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(@as(u32, 8), Config.resolve(&env, derive.Facts.synthetic(32, 0)).workers);
    try log.testing.expectAllFired();
}

test "fd cache cap derives from RLIMIT_NOFILE as found: clamp(soft/4/workers, 16, 1024)" {
    try testing.expectEqual(@as(u32, 32), fdCacheCapFrom(1024, 8));
    try testing.expectEqual(@as(u32, 1024), fdCacheCapFrom(524288, 8));
    try testing.expectEqual(@as(u32, 16), fdCacheCapFrom(0, 8));
    try testing.expectEqual(@as(u32, 1024), fdCacheCapFrom(1 << 40, 1));
    try testing.expectEqual(@as(u32, 256), fdCacheCapFrom(1024, 0));
}

test "thread names fit the kernel comm budget for every worker index" {
    var buf: thread_name.CommBuf = undefined;
    const n = workerName(&buf, 63);
    try testing.expectEqualStrings("sqz-uring-fs63", n);
    try testing.expect(n.len <= thread_name.comm_max);
}

test "errno table: every mapped error round-trips to its errno; unknown errnos become Unexpected, which replies EIO" {
    try testing.expectEqual(Error.InputOutput, fromErrno(.IO));
    try testing.expectEqual(linux.E.IO, toErrno(Error.InputOutput));
    try testing.expectEqual(Error.NoSpaceLeft, fromErrno(.NOSPC));
    try testing.expectEqual(Error.FileNotFound, fromErrno(.NOENT));
    try testing.expectEqual(Error.InvalidArgument, fromErrno(.INVAL));
    try testing.expectEqual(Error.Unexpected, fromErrno(.NOTSOCK));
    try testing.expectEqual(linux.E.IO, toErrno(Error.Unexpected));
    // The refusals and the backpressure class map as the reference does: EINVAL.
    try testing.expectEqual(linux.E.INVAL, toErrno(Error.UnalignedDirectWrite));
    try testing.expectEqual(linux.E.INVAL, toErrno(Error.QueueFull));
    try testing.expectEqual(ErrorClass.retryable, classOf(Error.QueueFull));
    try testing.expectEqual(ErrorClass.terminal, classOf(Error.InputOutput));
    try testing.expectEqual(ErrorClass.refusal, classOf(Error.UnalignedDirectWrite));
}

test "the bounded queue: try-send never blocks, a full queue refuses, close drains then ends the consumer" {
    const Q = Mpmc(u32, 8);
    const q = try testing.allocator.create(Q);
    defer testing.allocator.destroy(q);
    q.init();
    var i: u32 = 0;
    while (i < 8) : (i += 1) try q.trySend(i);
    try testing.expectError(error.Full, q.trySend(99));
    try testing.expectEqual(@as(?u32, 0), q.tryRecv());
    try q.trySend(99);
    try testing.expectError(error.Full, q.trySend(100));
    var expect: u32 = 1;
    while (q.tryRecv()) |v| : (expect += 1) {
        if (expect == 8) try testing.expectEqual(@as(u32, 99), v) else try testing.expectEqual(expect, v);
    }
    try testing.expectEqual(@as(?u32, null), q.tryRecv());

    const Consumer = struct {
        fn run(queue: *Q, got: *std.atomic.Value(u32)) void {
            while (queue.recvBlocking()) |v| _ = got.fetchAdd(v, .monotonic);
        }
    };
    var got: std.atomic.Value(u32) = .init(0);
    const t = try std.Thread.spawn(.{}, Consumer.run, .{ q, &got });
    try q.trySend(5);
    try q.trySend(7);
    q.close();
    try testing.expectError(error.Closed, q.trySend(1));
    t.join();
    try testing.expectEqual(@as(u32, 12), got.load(.monotonic));
}

test "the oneshot: complete wakes a parked waiter; either side may drop first" {
    const Done = Oneshot(u32);
    const d = try Done.create(testing.allocator);
    const Completer = struct {
        fn run(done: *Done) void {
            done.complete(42);
        }
    };
    const t = try std.Thread.spawn(.{}, Completer.run, .{d});
    try testing.expectEqual(@as(u32, 42), d.wait());
    d.release();
    t.join();
    // Caller gives up first: the completer's release frees it.
    const e = try Done.create(testing.allocator);
    e.release();
    e.complete(1);
}

test "AlignedBuf honours the alignment it was asked for and copies a slice" {
    var b = try AlignedBuf.zeroed(testing.allocator, 4096, 4096);
    defer b.deinit();
    try testing.expect(align_probe.ptrAligned(b.bytes.ptr, 4096));
    try testing.expectEqual(@as(usize, 4096), b.bytes.len);
    for (b.bytes) |x| try testing.expectEqual(@as(u8, 0), x);
    var c = try AlignedBuf.fromSlice(testing.allocator, "hello", 512);
    defer c.deinit();
    try testing.expect(align_probe.ptrAligned(c.bytes.ptr, 512));
    try testing.expectEqualStrings("hello", c.bytes);
    var z = try AlignedBuf.zeroed(testing.allocator, 0, 4096);
    z.deinit();
}

test "write_all then read_all round-trips 12,345 bytes; fdatasync succeeds; parents are created" {
    var tp = try TmpPath.init();
    defer tp.deinit();
    const pool = try testPool(2);
    defer pool.deinit();

    var pbuf: [std.fs.max_path_bytes]u8 = undefined;
    const path = tp.join(&pbuf, "a/b/c.dat");
    const data = try testing.allocator.alloc(u8, 12_345);
    defer testing.allocator.free(data);
    for (data, 0..) |*b, i| b.* = @truncate(i *% 31 + 7);
    try pool.writeAll(path, data);
    try pool.fdatasync(path);
    var got = try pool.readAll(path);
    defer got.deinit();
    try testing.expectEqualSlices(u8, data, got.bytes());
    // read_at past EOF is short, not an error; a zero-size read is empty.
    var tail = try pool.readAt(path, 12_000, 1000);
    defer tail.deinit();
    try testing.expectEqualSlices(u8, data[12_000..], tail.bytes());
    var none = try pool.readAt(path, 0, 0);
    defer none.deinit();
    try testing.expectEqual(@as(usize, 0), none.bytes().len);
}

test "the pool runs at least the shipped floor of workers and names them" {
    const pool = try Pool.init(testing.allocator, Config.fixed(resolveWorkerCount(null, 1), 1024));
    defer pool.deinit();
    try testing.expect(pool.workerCount() >= 4);
}

test "fdatasync then read on one path from 64 threads never sees EBADF (the cached fd is O_RDWR)" {
    var tp = try TmpPath.init();
    defer tp.deinit();
    const pool = try testPool(4);
    defer pool.deinit();
    var pbuf: [std.fs.max_path_bytes]u8 = undefined;
    const path = tp.join(&pbuf, "shared.dat");
    try pool.writeAll(path, "");

    const Task = struct {
        fn run(p: *Pool, file: []const u8, i: usize, failed: *std.atomic.Value(u32)) void {
            runInner(p, file, i) catch {
                _ = failed.fetchAdd(1, .monotonic);
            };
        }
        fn runInner(p: *Pool, file: []const u8, i: usize) !void {
            var block: [256]u8 = undefined;
            @memset(&block, @as(u8, @truncate(i)));
            try p.writeAt(file, i * 256, &block);
            try p.fdatasync(file);
            var got = try p.readAt(file, i * 256, 256);
            defer got.deinit();
            try testing.expectEqualSlices(u8, &block, got.bytes());
        }
    };
    var failed: std.atomic.Value(u32) = .init(0);
    var threads: [64]std.Thread = undefined;
    for (&threads, 0..) |*t, i| t.* = try std.Thread.spawn(.{}, Task.run, .{ pool, path, i, &failed });
    for (threads) |t| t.join();
    try testing.expectEqual(@as(u32, 0), failed.load(.monotonic));
}

test "64 files of 4096+i bytes written, synced and read back byte-exact across the pool" {
    var tp = try TmpPath.init();
    defer tp.deinit();
    const pool = try testPool(4);
    defer pool.deinit();

    const Task = struct {
        fn run(p: *Pool, tpp: *const TmpPath, i: usize, failed: *std.atomic.Value(u32)) void {
            runInner(p, tpp, i) catch {
                _ = failed.fetchAdd(1, .monotonic);
            };
        }
        fn runInner(p: *Pool, tpp: *const TmpPath, i: usize) !void {
            var pbuf: [std.fs.max_path_bytes]u8 = undefined;
            var name: [32]u8 = undefined;
            const path = tpp.join(&pbuf, try std.fmt.bufPrint(&name, "f{d}.dat", .{i}));
            const data = try testing.allocator.alloc(u8, 4096 + i);
            defer testing.allocator.free(data);
            for (data, 0..) |*b, k| b.* = @truncate(k ^ i);
            try p.writeAt(path, 0, data);
            try p.fdatasync(path);
            var got = try p.readAll(path);
            defer got.deinit();
            try testing.expectEqualSlices(u8, data, got.bytes());
        }
    };
    var failed: std.atomic.Value(u32) = .init(0);
    var threads: [64]std.Thread = undefined;
    for (&threads, 0..) |*t, i| t.* = try std.Thread.spawn(.{}, Task.run, .{ pool, &tp, i, &failed });
    for (threads) |t| t.join();
    try testing.expectEqual(@as(u32, 0), failed.load(.monotonic));
}

test "write_at_batch lands every entry of an unordered, non-contiguous batch; the gap keeps its seed; an empty batch is Ok" {
    var tp = try TmpPath.init();
    defer tp.deinit();
    const pool = try testPool(2);
    defer pool.deinit();
    var pbuf: [std.fs.max_path_bytes]u8 = undefined;
    const path = tp.join(&pbuf, "batch.dat");
    const seed = try testing.allocator.alloc(u8, 32768);
    defer testing.allocator.free(seed);
    @memset(seed, 0xEE);
    try pool.writeAll(path, seed);

    var a: [512]u8 = @splat(0xA1);
    var b: [4096]u8 = @splat(0xB2);
    var c: [12288]u8 = @splat(0xC3);
    var d: [100]u8 = @splat(0xD4);
    const ops = [_]WriteOp{
        .{ .offset = 20000, .data = &c },
        .{ .offset = 100, .data = &a },
        .{ .offset = 8192, .data = &b },
        .{ .offset = 4000, .data = &d },
    };
    try pool.writeAtBatch(path, &ops);
    try pool.writeAtBatch(path, &.{});
    var got = try pool.readAll(path);
    defer got.deinit();
    const out = got.bytes();
    try testing.expectEqual(@as(usize, 32768), out.len);
    try testing.expectEqualSlices(u8, &a, out[100..612]);
    try testing.expectEqualSlices(u8, &b, out[8192..12288]);
    try testing.expectEqualSlices(u8, &c, out[20000..32288]);
    try testing.expectEqualSlices(u8, &d, out[4000..4100]);
    for (out[612..4000]) |x| try testing.expectEqual(@as(u8, 0xEE), x);
    for (out[32288..]) |x| try testing.expectEqual(@as(u8, 0xEE), x);
}

test "write_at_batch propagates an open failure as the batch's error" {
    const pool = try testPool(1);
    defer pool.deinit();
    const ops = [_]WriteOp{.{ .offset = 0, .data = "x" }};
    try testing.expectError(Error.FileNotFound, pool.writeAtBatch("/proc/definitely/not/here/file", &ops));
    try testing.expectError(Error.FileNotFound, pool.fdatasync("/proc/definitely/not/here/file"));
}

test "ONE batch of 600 x 4 KiB sectors (more entries than the 512-deep SQ) completes; the SQ flushes and retries" {
    var tp = try TmpPath.init();
    defer tp.deinit();
    const pool = try testPool(1);
    defer pool.deinit();
    var pbuf: [std.fs.max_path_bytes]u8 = undefined;
    const path = tp.join(&pbuf, "big-batch.dat");
    try pool.writeAll(path, "");

    const n = 600;
    const sectors = try testing.allocator.alloc(u8, n * 4096);
    defer testing.allocator.free(sectors);
    const ops = try testing.allocator.alloc(WriteOp, n);
    defer testing.allocator.free(ops);
    for (ops, 0..) |*op, i| {
        const s = sectors[i * 4096 ..][0..4096];
        @memset(s, @as(u8, @truncate(i)));
        op.* = .{ .offset = i * 4096, .data = s };
    }
    var handle = try pool.submitWriteAtBatch(path, ops);
    try testing.expect(handle.submittedAt() > 0);
    const out = handle.waitStamped();
    try out.result;
    try testing.expect(out.stamps.admitted_at >= out.stamps.submitted_at);
    try testing.expect(out.stamps.woken_at >= out.stamps.admitted_at);
    try testing.expect(out.stamps.observed_at >= out.stamps.woken_at);
    for ([_]usize{ 0, 255, 511, 512, 599 }) |i| {
        var got = try pool.readAt(path, i * 4096, 4096);
        defer got.deinit();
        try testing.expectEqual(@as(usize, 4096), got.bytes().len);
        for (got.bytes()) |x| try testing.expectEqual(@as(u8, @truncate(i)), x);
    }
    try testing.expect(stats.phases[@backingInt(WritePhase.total)].snapshot().count >= 1);
}

test "fd cache churn under load: 16 threads over 200 distinct files with a 16-entry cache never corrupts in-flight I/O" {
    var tp = try TmpPath.init();
    defer tp.deinit();
    // nofile 0 ⇒ the floor: a 16-entry cache per worker, so eviction is constant.
    const pool = try Pool.init(testing.allocator, Config.fixed(4, 0));
    defer pool.deinit();
    try testing.expectEqual(@as(u32, 16), pool.config.fd_cache_cap);

    const Task = struct {
        fn run(p: *Pool, tpp: *const TmpPath, t: usize, failed: *std.atomic.Value(u32)) void {
            runInner(p, tpp, t) catch {
                _ = failed.fetchAdd(1, .monotonic);
            };
        }
        fn runInner(p: *Pool, tpp: *const TmpPath, t: usize) !void {
            var k: usize = 0;
            while (k < 200) : (k += 1) {
                var pbuf: [std.fs.max_path_bytes]u8 = undefined;
                var name: [32]u8 = undefined;
                const path = tpp.join(&pbuf, try std.fmt.bufPrint(&name, "churn-{d}-{d}", .{ t, k }));
                var block: [1024]u8 = undefined;
                @memset(&block, @as(u8, @truncate(t * 7 + k)));
                try p.writeAt(path, 0, &block);
                var got = try p.readAt(path, 0, 1024);
                defer got.deinit();
                try testing.expectEqualSlices(u8, &block, got.bytes());
            }
        }
    };
    var failed: std.atomic.Value(u32) = .init(0);
    var threads: [16]std.Thread = undefined;
    for (&threads, 0..) |*t, i| t.* = try std.Thread.spawn(.{}, Task.run, .{ pool, &tp, i, &failed });
    for (threads) |t| t.join();
    try testing.expectEqual(@as(u32, 0), failed.load(.monotonic));
}

test "concurrent mixed I/O is byte-exact: 32 threads x 10 rounds over 8 files in disjoint 4 KiB regions" {
    var tp = try TmpPath.init();
    defer tp.deinit();
    const pool = try testPool(4);
    defer pool.deinit();
    const files = 8;
    var paths: [files][std.fs.max_path_bytes]u8 = undefined;
    var lens: [files]usize = undefined;
    for (&paths, 0..) |*pb, i| {
        var name: [16]u8 = undefined;
        lens[i] = tp.join(pb, try std.fmt.bufPrint(&name, "mix{d}.dat", .{i})).len;
        try pool.writeAll(pb[0..lens[i]], "");
    }
    const Ctx = struct { paths: *const [files][std.fs.max_path_bytes]u8, lens: *const [files]usize };
    const Task = struct {
        fn run(p: *Pool, ctx: Ctx, t: usize, failed: *std.atomic.Value(u32)) void {
            runInner(p, ctx, t) catch {
                _ = failed.fetchAdd(1, .monotonic);
            };
        }
        fn runInner(p: *Pool, ctx: Ctx, t: usize) !void {
            var round: usize = 0;
            while (round < 10) : (round += 1) {
                const f = (t + round) % files;
                const path = ctx.paths[f][0..ctx.lens[f]];
                const region: u64 = t * 4096;
                var block: [4096]u8 = undefined;
                @memset(&block, @as(u8, @truncate(t * 13 + round)));
                if (round % 3 == 1) {
                    const ops = [_]WriteOp{ .{ .offset = region, .data = block[0..2048] }, .{ .offset = region + 2048, .data = block[2048..] } };
                    try p.writeAtBatch(path, &ops);
                } else {
                    try p.writeAt(path, region, &block);
                }
                if (round % 2 == 0) try p.fdatasync(path);
                var got = try p.readAt(path, region, 4096);
                defer got.deinit();
                try testing.expectEqualSlices(u8, &block, got.bytes());
            }
        }
    };
    var failed: std.atomic.Value(u32) = .init(0);
    var threads: [32]std.Thread = undefined;
    for (&threads, 0..) |*t, i| t.* = try std.Thread.spawn(.{}, Task.run, .{ pool, Ctx{ .paths = &paths, .lens = &lens }, i, &failed });
    for (threads) |t| t.join();
    try testing.expectEqual(@as(u32, 0), failed.load(.monotonic));
}

test "shutdown drains and joins: every outstanding handle completes, none hangs" {
    var tp = try TmpPath.init();
    defer tp.deinit();
    const pool = try testPool(2);
    var pbuf: [std.fs.max_path_bytes]u8 = undefined;
    const path = tp.join(&pbuf, "drain.dat");
    try pool.writeAll(path, "");
    var blocks: [32][4096]u8 = undefined;
    var handles: [32]WriteCompletion = undefined;
    for (&handles, 0..) |*h, i| {
        @memset(&blocks[i], @as(u8, @truncate(i)));
        const ops = [_]WriteOp{.{ .offset = i * 4096, .data = &blocks[i] }};
        h.* = try pool.submitWriteAtBatch(path, &ops);
    }
    // One handle is dropped unobserved: the write still lands and nothing leaks.
    handles[0].deinit();
    pool.deinit();
    for (handles[1..]) |*h| {
        h.wait() catch |e| try testing.expectEqual(Error.WorkerClosed, e);
    }
    const pool2 = try testPool(1);
    defer pool2.deinit();
    var got = try pool2.readAt(path, 0, 4096);
    defer got.deinit();
    for (got.bytes()) |x| try testing.expectEqual(@as(u8, 0), x);
}

test "a registered metadata file opens O_DIRECT with a derived grain; lookup by given and canonical path; idempotent" {
    clearMetaDevices();
    var tp = try TmpPath.init();
    defer tp.deinit();
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    var pbuf: [std.fs.max_path_bytes]u8 = undefined;
    const path = tp.join(&pbuf, "meta.img");
    {
        const pool = try testPool(1);
        defer pool.deinit();
        const zeros = try testing.allocator.alloc(u8, 65536);
        defer testing.allocator.free(zeros);
        @memset(zeros, 0);
        try pool.writeAll(path, zeros);
    }
    try testing.expectEqual(@as(?MetaIoMode, null), metaIoMode(path));
    try testing.expectEqual(@as(u64, 1), metaIoGrain(path));

    const direct_before = stats.direct_paths.load();
    const mode = try registerMetaDevice(testing.allocator, &env, path);
    try testing.expect(mode.isDirect());
    try testing.expect(std.math.isPowerOfTwo(mode.grain()));
    try testing.expect(mode.grain() <= align_probe.max_grain);
    try testing.expect(std.math.isPowerOfTwo(mode.memAlign()));
    try testing.expect(mode.memAlign() >= mode.grain());
    try testing.expectEqual(direct_before + 1, stats.direct_paths.load());
    try testing.expectEqual(mode, metaIoMode(path).?);
    try testing.expectEqual(mode.grain(), metaIoGrain(path));
    // Idempotent: a second registration returns the recorded posture, no second trial open.
    try testing.expectEqual(mode, try registerMetaDevice(testing.allocator, &env, path));
    try testing.expectEqual(direct_before + 1, stats.direct_paths.load());
    // The canonical spelling resolves to the same posture.
    const canon = (try canonicalize(testing.allocator, path)).?;
    defer testing.allocator.free(canon);
    try testing.expectEqual(mode, metaIoMode(canon).?);
    clearMetaDevices();
    try testing.expectEqual(@as(?MetaIoMode, null), metaIoMode(path));
}

test "a regular file whose filesystem refuses O_DIRECT with EINVAL falls back to BUFFERED, loudly and counted" {
    clearMetaDevices();
    defer clearMetaDevices();
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    // procfs is a regular file that has no direct-I/O path: the trial open says EINVAL.
    const path = "/proc/self/comm";
    const fallback_before = stats.buffered_fallback.load();
    log.testing.expectErrorLogs(1);
    const mode = try registerMetaDevice(testing.allocator, &env, path);
    try log.testing.expectAllFired();
    try testing.expectEqual(@as(MetaIoMode, .buffered), mode);
    try testing.expectEqual(@as(u64, 1), mode.grain());
    try testing.expectEqual(fallback_before + 1, stats.buffered_fallback.load());
    try testing.expectEqual(@as(MetaIoMode, .buffered), metaIoMode(path).?);
}

test "a path that is neither a block device nor a regular file refuses registration; a missing path cannot stat" {
    clearMetaDevices();
    defer clearMetaDevices();
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    log.testing.expectErrorLogs(2);
    try testing.expectError(error.NotADeviceOrFile, registerMetaDevice(testing.allocator, &env, "/dev/null"));
    try testing.expectError(error.CannotStat, registerMetaDevice(testing.allocator, &env, "/definitely/not/here"));
    try log.testing.expectAllFired();
    try testing.expectEqual(@as(?MetaIoMode, null), metaIoMode("/dev/null"));
}

test "misaligned direct writes refuse, misaligned buffers bounce, narrow reads widen, pad_to_grain zero-extends" {
    clearMetaDevices();
    defer clearMetaDevices();
    var tp = try TmpPath.init();
    defer tp.deinit();
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    const pool = try testPool(2);
    defer pool.deinit();
    var pbuf: [std.fs.max_path_bytes]u8 = undefined;
    const path = tp.join(&pbuf, "direct.img");
    const image_len: usize = 8 * 4096;
    {
        const zeros = try testing.allocator.alloc(u8, image_len);
        defer testing.allocator.free(zeros);
        @memset(zeros, 0);
        try pool.writeAll(path, zeros);
    }
    const mode = try registerMetaDevice(testing.allocator, &env, path);
    try testing.expect(mode.isDirect());
    const g: usize = @intCast(mode.grain());

    // Refusals: offset off the grain, then length off the grain. Nothing lands.
    const refusals_before = stats.unaligned_refusals.load();
    var page = try AlignedBuf.zeroed(testing.allocator, 2 * g, mode.memAlign());
    defer page.deinit();
    @memset(page.bytes, 0xC3);
    log.testing.expectErrorLogs(2);
    try testing.expectError(Error.UnalignedDirectWrite, pool.writeAt(path, 1, page.bytes[0..g]));
    try testing.expectError(Error.UnalignedDirectWrite, pool.writeAt(path, 0, page.bytes[0 .. g - 1]));
    try testing.expect(log.testing.capturedErrorLogs() >= 1);
    log.testing.expectErrorLogs(0);
    try testing.expectEqual(refusals_before + 2, stats.unaligned_refusals.load());
    {
        var got = try pool.readAt(path, 0, 2 * g);
        defer got.deinit();
        for (got.bytes()) |x| try testing.expectEqual(@as(u8, 0), x);
    }

    // A misaligned POINTER with an aligned shape bounces and lands.
    const bounce_before = stats.bounce_bytes.load();
    try pool.writeAt(path, 2 * g, page.bytes[1 .. 1 + g]);
    try testing.expectEqual(bounce_before + g, stats.bounce_bytes.load());
    // An aligned pointer with an aligned shape pays no bounce.
    try pool.writeAt(path, 3 * g, page.bytes[0..g]);
    try testing.expectEqual(bounce_before + g, stats.bounce_bytes.load());

    // A narrow read widens to the grain and is exact; a straddling read too.
    const widened_before = stats.read_widened.load();
    {
        var got = try pool.readAt(path, 2 * g + 7, 21);
        defer got.deinit();
        try testing.expectEqual(@as(usize, 21), got.bytes().len);
        for (got.bytes()) |x| try testing.expectEqual(@as(u8, 0xC3), x);
    }
    {
        var got = try pool.readAt(path, 4 * g - 4, 8);
        defer got.deinit();
        try testing.expectEqual(@as(usize, 8), got.bytes().len);
        try testing.expectEqualSlices(u8, &[_]u8{ 0xC3, 0xC3, 0xC3, 0xC3, 0, 0, 0, 0 }, got.bytes());
    }
    try testing.expectEqual(widened_before + 2, stats.read_widened.load());

    // pad_to_grain: length rounds up, the tail is zero; a buffered path is verbatim.
    var image: std.ArrayList(u8) = .empty;
    defer image.deinit(testing.allocator);
    try image.appendSlice(testing.allocator, "abc");
    try padToGrain(testing.allocator, path, &image);
    try testing.expectEqual(g, image.items.len);
    try testing.expectEqualStrings("abc", image.items[0..3]);
    for (image.items[3..]) |x| try testing.expectEqual(@as(u8, 0), x);
    var plain: std.ArrayList(u8) = .empty;
    defer plain.deinit(testing.allocator);
    try plain.appendSlice(testing.allocator, "abc");
    try padToGrain(testing.allocator, "/unregistered/path", &plain);
    try testing.expectEqual(@as(usize, 3), plain.items.len);

    // The write law as a pure call: refuse, bounce, or pass through.
    var prepared = try prepareDirectWrite(testing.allocator, mode, path, 0, page.bytes[0..g]);
    try testing.expect(prepared.bounce == null);
    try testing.expectEqual(page.bytes[0..g].ptr, prepared.data.ptr);
    prepared.deinit();
    var bounced = try prepareDirectWrite(testing.allocator, mode, path, 0, page.bytes[1 .. 1 + g]);
    try testing.expect(bounced.bounce != null);
    try testing.expect(align_probe.ptrAligned(bounced.data.ptr, mode.memAlign()));
    bounced.deinit();
    var buffered = try prepareDirectWrite(testing.allocator, .buffered, path, 1, page.bytes[1..3]);
    try testing.expect(buffered.bounce == null);
    buffered.deinit();
}

test "the buffered seam (SQUEEZEFS_TEST_META_BUFFERED=1) keeps the registered file buffered: grain 1, verbatim writes" {
    clearMetaDevices();
    defer clearMetaDevices();
    var tp = try TmpPath.init();
    defer tp.deinit();
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    try env.put("SQUEEZEFS_TEST_META_BUFFERED", "1");
    const pool = try testPool(1);
    defer pool.deinit();
    var pbuf: [std.fs.max_path_bytes]u8 = undefined;
    const path = tp.join(&pbuf, "seam.img");
    try pool.writeAll(path, "0123456789");
    const fallback_before = stats.buffered_fallback.load();
    const mode = try registerMetaDevice(testing.allocator, &env, path);
    try testing.expect(!mode.isDirect());
    try testing.expectEqual(@as(u64, 1), mode.grain());
    try testing.expectEqual(fallback_before + 1, stats.buffered_fallback.load());
    try pool.writeAt(path, 3, "abc");
    var got = try pool.readAll(path);
    defer got.deinit();
    try testing.expectEqualStrings("012abc6789", got.bytes());
}

test "the posture lookup resolves a path once and never on a hit; aliases; registration drops negatives; clear forgets" {
    clearMetaDevices();
    defer clearMetaDevices();
    var tp = try TmpPath.init();
    defer tp.deinit();
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    var pbuf: [std.fs.max_path_bytes]u8 = undefined;
    const path = tp.join(&pbuf, "once.img");
    {
        const pool = try testPool(1);
        defer pool.deinit();
        const zeros = try testing.allocator.alloc(u8, 4096);
        defer testing.allocator.free(zeros);
        @memset(zeros, 0);
        try pool.writeAll(path, zeros);
    }
    var lbuf: [std.fs.max_path_bytes]u8 = undefined;
    const alias = tp.join(&lbuf, "alias.img");
    try tp.tmp.dir.symLink(testing.io, path, "alias.img", .{});

    // An unregistered path resolves once, then the negative is remembered.
    const r0 = stats.mode_resolves.load();
    try testing.expectEqual(@as(?MetaIoMode, null), metaIoMode(path));
    try testing.expectEqual(r0 + 1, stats.mode_resolves.load());
    var i: usize = 0;
    while (i < 1000) : (i += 1) try testing.expectEqual(@as(?MetaIoMode, null), metaIoMode(path));
    try testing.expectEqual(r0 + 1, stats.mode_resolves.load());
    // The alias seen BEFORE registration is a remembered negative too.
    try testing.expectEqual(@as(?MetaIoMode, null), metaIoMode(alias));
    try testing.expectEqual(r0 + 2, stats.mode_resolves.load());

    // Registration overwrites the negative and drops every other negative.
    const mode = try registerMetaDevice(testing.allocator, &env, path);
    try testing.expectEqual(mode, metaIoMode(path).?);
    try testing.expectEqual(r0 + 2, stats.mode_resolves.load());
    // The alias re-resolves exactly once (it was dropped, not kept BUFFERED forever).
    try testing.expectEqual(mode, metaIoMode(alias).?);
    try testing.expectEqual(r0 + 3, stats.mode_resolves.load());
    i = 0;
    while (i < 1000) : (i += 1) try testing.expectEqual(mode, metaIoMode(alias).?);
    try testing.expectEqual(r0 + 3, stats.mode_resolves.load());

    clearMetaDevices();
    try testing.expectEqual(@as(?MetaIoMode, null), metaIoMode(path));
    try testing.expectEqual(r0 + 4, stats.mode_resolves.load());
}

test "the stats JSON names every meta_io counter, the four write phases and the queue-full count" {
    var buf: [8192]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    const s = w.buffered();
    for ([_][]const u8{ "\"meta_io\":{", "\"meta_io_direct_paths\":", "\"meta_io_buffered_fallback\":", "\"meta_io_bounce_bytes\":", "\"meta_io_unaligned_refusals\":", "\"meta_io_read_widened\":", "\"uring_fs_write_phase_ns\":{", "\"queue_hop\":", "\"device\":", "\"wake_hop\":", "\"total\":", "\"uring_queue_full\":" }) |needle| {
        try testing.expect(std.mem.indexOf(u8, s, needle) != null);
    }
}

test "the owned ring: a timed park returns on the tick, a wake returns it early, and a batch completes through service" {
    var tp = try TmpPath.init();
    defer tp.deinit();
    var pbuf: [std.fs.max_path_bytes]u8 = undefined;
    const path = tp.join(&pbuf, "lane.dat");
    {
        const pool = try testPool(1);
        defer pool.deinit();
        try pool.writeAll(path, "");
    }
    const ring = try OwnedRing.init(testing.allocator, journal_ring_entries, 16);
    defer ring.deinit();

    // Nothing in flight: the tick fires.
    try testing.expect(ring.park(2 * std.time.ns_per_ms));
    // A wake from another thread ends the park before a long tick.
    const Waker = struct {
        fn run(w: RingWake) void {
            w.wake();
        }
    };
    const t = try std.Thread.spawn(.{}, Waker.run, .{ring.wakeHandle()});
    try testing.expect(!ring.park(10 * std.time.ns_per_s));
    t.join();
    ring.service();
    // A wake delivered before the park is sticky: the next park returns at once.
    ring.wakeHandle().wake();
    try testing.expect(!ring.park(10 * std.time.ns_per_s));
    ring.service();

    var blocks: [3][4096]u8 = undefined;
    for (&blocks, 0..) |*b, i| @memset(b, @as(u8, @truncate(0x10 + i)));
    const ops = [_]WriteOp{
        .{ .offset = 0, .data = &blocks[0] },
        .{ .offset = 4096, .data = &blocks[1] },
        .{ .offset = 8192, .data = &blocks[2] },
    };
    var handle = try ring.submitWriteAtBatch(path, &ops);
    while (!handle.pollDone()) {
        _ = ring.park(std.time.ns_per_s);
        ring.service();
    }
    try handle.wait();
    const pool = try testPool(1);
    defer pool.deinit();
    var got = try pool.readAll(path);
    defer got.deinit();
    try testing.expectEqual(@as(usize, 3 * 4096), got.bytes().len);
    for (got.bytes(), 0..) |x, i| try testing.expectEqual(@as(u8, @truncate(0x10 + i / 4096)), x);
}

test "a block device registers direct with its logical block size as the grain (loop device; skipped without loop control)" {
    clearMetaDevices();
    defer clearMetaDevices();
    var tp = try TmpPath.init();
    defer tp.deinit();
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    var pbuf: [std.fs.max_path_bytes]u8 = undefined;
    const backing = tp.join(&pbuf, "loop-backing.img");
    const pool = try testPool(1);
    defer pool.deinit();
    {
        const zeros = try testing.allocator.alloc(u8, 8 << 20);
        defer testing.allocator.free(zeros);
        @memset(zeros, 0);
        try pool.writeAll(backing, zeros);
    }
    var loop = (try LoopDevice.attach(backing)) orelse return error.SkipZigTest;
    defer loop.detach();
    const dev = loop.path();

    const fallback_before = stats.buffered_fallback.load();
    const mode = try registerMetaDevice(testing.allocator, &env, dev);
    try testing.expect(mode.isDirect());
    // The loop driver's logical block size is 512 unless configured otherwise;
    // whatever it reports, the ladder admitted a power of two within a page.
    try testing.expect(std.math.isPowerOfTwo(mode.grain()));
    try testing.expect(mode.grain() <= align_probe.max_grain);
    try testing.expectEqual(fallback_before, stats.buffered_fallback.load());
    const probe = align_probe.probeGrain(dev);
    try testing.expectEqual(probe.grain, mode.grain());
    try testing.expect(probe.source != .default);

    var page = try AlignedBuf.zeroed(testing.allocator, 4096, mode.memAlign());
    defer page.deinit();
    @memset(page.bytes, 0x5A);
    try pool.writeAt(dev, 4096, page.bytes);
    try pool.fdatasync(dev);
    var got = try pool.readAt(dev, 4096, 4096);
    defer got.deinit();
    try testing.expectEqualSlices(u8, page.bytes, got.bytes());
    const refusals_before = stats.unaligned_refusals.load();
    // The tripwire logs at most once per second per site; an earlier test may
    // have used this second, so the log is allowed, not required.
    log.testing.expectErrorLogs(1);
    try testing.expectError(Error.UnalignedDirectWrite, pool.writeAt(dev, 4096 + 1, page.bytes));
    log.testing.expectErrorLogs(0);
    try testing.expectEqual(refusals_before + 1, stats.unaligned_refusals.load());
}

/// Test harness: attach a backing file to a free loop device through the
/// loop-control ioctls (no `losetup` child process). Null when the host has
/// no loop control or refuses it.
const LoopDevice = struct {
    fd: linux.fd_t,
    backing_fd: linux.fd_t,
    index: u32,
    name: [32]u8 = undefined,
    name_len: usize = 0,

    const LOOP_SET_FD = 0x4C00;
    const LOOP_CLR_FD = 0x4C01;
    const LOOP_CTL_GET_FREE = 0x4C82;

    fn attach(backing: []const u8) !?LoopDevice {
        const ctl_raw = linux.open("/dev/loop-control", .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
        if (linux.errno(ctl_raw) != .SUCCESS) return null;
        const ctl: linux.fd_t = @intCast(ctl_raw);
        defer _ = linux.close(ctl);
        const free_raw = linux.ioctl(ctl, LOOP_CTL_GET_FREE, 0);
        if (linux.errno(free_raw) != .SUCCESS) return null;
        var self: LoopDevice = .{ .fd = -1, .backing_fd = -1, .index = @intCast(free_raw) };
        self.name_len = (try std.fmt.bufPrint(&self.name, "/dev/loop{d}", .{self.index})).len;
        self.name[self.name_len] = 0;
        var zbuf: [linux.PATH_MAX]u8 = undefined;
        const bz = try align_probe.pathZ(&zbuf, backing);
        const b_raw = linux.open(bz, .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
        if (linux.errno(b_raw) != .SUCCESS) return null;
        self.backing_fd = @intCast(b_raw);
        const d_raw = linux.open(self.name[0..self.name_len :0], .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
        if (linux.errno(d_raw) != .SUCCESS) {
            _ = linux.close(self.backing_fd);
            return null;
        }
        self.fd = @intCast(d_raw);
        if (linux.errno(linux.ioctl(self.fd, LOOP_SET_FD, @intCast(self.backing_fd))) != .SUCCESS) {
            _ = linux.close(self.fd);
            _ = linux.close(self.backing_fd);
            return null;
        }
        return self;
    }

    fn path(self: *const LoopDevice) []const u8 {
        return self.name[0..self.name_len];
    }

    fn detach(self: *LoopDevice) void {
        _ = linux.ioctl(self.fd, LOOP_CLR_FD, 0);
        _ = linux.close(self.fd);
        _ = linux.close(self.backing_fd);
    }
};
