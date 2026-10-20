//! The transport session (charter §4 "Bring-up (ordered)", "Sideband",
//! "Teardown"; `docs/porting/fuse_abi_session.md` §2, §4, §7, §8;
//! `fuse_uring_transport.md` §2, §11): `open("/dev/fuse")`, the root
//! `mount(2)`, the classical INIT read through a `ParkedRing` READ, the
//! geometry plan BEFORE the reply is serialized, `fuse.enable_uring=Y`
//! BEFORE the reply is written, the reply, the drain-group workers, the arm
//! barrier (submissions), `ready`; then the classical SIDEBAND forever
//! (FORGET / BATCH_FORGET / INTERRUPT / resends / switch-over stragglers)
//! carrying the notify writes; and the teardown that synthesizes an error
//! for every owed slot, unmounts, joins and closes. Linux only.
//!
//! ONE `ParkedRing` serves the INIT exchange and the sideband (decision
//! 0002 ledger Q1: the reference built two mutexed rings per connection
//! under an epoll watcher); the sideband's READ is armed like the wake and
//! its replies and notify frames are WRITEs on the same ring from
//! preallocated cells. A straggler's reply (`ReplySlot.classical`) is
//! written before `submitReply` returns, so a handler's stack body is legal
//! there; a notify frame is fire-and-forget (ENOENT = the kernel does not
//! hold the inode, expected and counted; any other errno counted and
//! warned once per class; the notify arm never takes part in teardown).
//!
//! The session is also the duck-typed transport the handler layer binds
//! (`fuse/handler/transport_seam.zig`): `SlotRef`, `PayloadLease`,
//! `replyWindow`, `submitReply`, `failEnt`, `releaseLease`.

const std = @import("std");
const testkit = @import("../../core/testkit.zig");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const assert = std.debug.assert;

const ring_park = @import("../../io/ring_park.zig");
const abi = @import("../abi.zig");
const codec = @import("../codec.zig");
const geometry = @import("../geometry.zig");
const mount_options = @import("../mount_options.zig");
const watchdog = @import("../handler/watchdog.zig");
const slot = @import("slot.zig");
const zc = @import("zc.zig");
const probes = @import("probes.zig");
const kmbuf = @import("kmbuf.zig");
const commit_queue = @import("commit_queue.zig");
const fsv = @import("filesystem_vtable.zig");
const worker = @import("worker.zig");
const counters = @import("../../core/counters.zig");
const clock = @import("../../core/clock.zig");
const log = @import("../../core/log.zig");
const knobs = @import("../../core/knobs.zig");
const derive = @import("../../core/derive.zig");
const thread_name = @import("../../core/thread_name.zig");
const mountinfo = @import("../../core/mountinfo.zig");
const futex = @import("../../sync/futex.zig");
const mpmc = @import("../../sync/mpmc.zig");
const completion = @import("../../sync/completion.zig");
const tick_law = @import("../../sync/tick.zig");
const exec = @import("../../exec/exec.zig");
const placement = @import("../../exec/placement.zig");
const mem_budget = @import("../../mem/budget.zig");
const read_divert = @import("../../meta/sym/tokens/read_divert.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("fuse/transport/session is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const SqzIo = exec.SqzIo;

// ---------------------------------------------------------------------------
// Constants with their reasons.
// ---------------------------------------------------------------------------

/// The arm barrier deadline: every REGISTER pushed within it or the mount
/// fails (the reference's `try_start` bound).
pub const ARM_BARRIER_NS: u64 = 30 * std.time.ns_per_s;
/// How long `close` waits for the workers' drains to synthesize before
/// `umount2`: the drain is bounded by the 100 ms lease budget plus one
/// flush; ten of those is generous and still no wedge.
pub const DRAIN_WAIT_NS: u64 = 10 * worker.DRAIN_LEASE_BUDGET_NS;
/// The plain-`umount2` retry window on EBUSY: a caller the drain just
/// answered is still inside its syscall or closing its fd for microseconds;
/// two seconds is the lock plane's tick (`sync/tick.zig`) — past it the
/// mount is detached lazily and the warn names it.
pub const UMOUNT_RETRY_NS: u64 = tick_law.tick_ns;
/// The fusectl tree: `max_background` per connection is the per-queue
/// background accounting's only userspace-readable fact (the kernel
/// exposes no per-queue counters; patch 0026 changes the LOCKING, not the
/// uapi).
pub const fusectl_connections = "/sys/fs/fuse/connections";
/// A sideband cell holds a name-carrying notify frame (header + struct +
/// `FUSE_NAME_MAX` + NUL = 1065 B) or a reply head (16 + 144 B); rounded
/// to the KiB so a PRUNE frame carries up to 250 inos.
pub const SIDEBAND_CELL_BYTES: usize = derive.alignUp(codec.NOTIFY_NAME_FRAME_MAX, 1024);
/// The sysfs module parameter the ring is created from at INIT-reply time
/// (since 7.2.4 iff it reads Y THEN).
pub const enable_uring_path = "/sys/module/fuse/parameters/enable_uring";
pub const dev_fuse_path = "/dev/fuse";

/// Sideband cells: one per possible concurrent straggler (≤ one per queue
/// CPU — the handler lanes are per CPU) plus a notify fan-out floor of 16
/// (two frames per served mutation × eight concurrent mutators).
pub fn sidebandCells(nqueues: u32) usize {
    return @intCast(derive.nextPow2(nqueues + 16));
}

/// Cells reserved for the notifies that never wait in the kernel —
/// attrs-only `INVAL_INODE` (a spinlock and a counter) and `INC_EPOCH` (an
/// atomic increment) — so a caller may WAIT for one (decision 0003 §AB.2:
/// such a cell recycles at its own CQE, never behind an `INVAL_ENTRY`
/// queued on a directory a request holds). Eight: the notify fan-out
/// floor's eight concurrent mutators, one attrs frame each in flight.
pub const SIDEBAND_NB_CELLS: usize = 8;

/// Sideband ring entries: the standing READ, the wake READ, one WRITE per
/// cell and a cancel.
pub fn sidebandEntries(cells: usize) u16 {
    return @intCast(derive.clamp(derive.nextPow2(cells + 3), 8, 4096));
}

// ---------------------------------------------------------------------------
// Counters (stats-inode keys).
// ---------------------------------------------------------------------------

/// `transport_sessions_active`: +1 at ready, −1 at shutdown.
pub var sessions_active: counters.Gauge = .{};
/// `transport_replies_dropped_no_slot`: replies whose slot no longer owes
/// the request (post-shutdown, or a foreign commit id) — kept OUT of the
/// reply gauge.
pub var replies_dropped_no_slot: counters.ShardedCounter = .{};
/// `transport_classical_sideband`: post-arm classical deliveries serviced
/// (`0` after an unlink storm = stranding).
pub var sideband_deliveries: counters.ShardedCounter = .{};
/// `transport_sideband_forgets` / `_interrupts` / `_requests`: what the
/// sideband saw by class — forgets count nlookup RECORDS (a BATCH_FORGET
/// carries many), requests the switch-over stragglers handed to a lane.
pub var sideband_forgets: counters.ShardedCounter = .{};
pub var sideband_interrupts: counters.ShardedCounter = .{};
pub var sideband_requests: counters.ShardedCounter = .{};
/// `transport_sideband_replies`: classical replies written.
pub var sideband_replies: counters.ShardedCounter = .{};
/// `transport_notify_sent` / `_enoent` / `_failed` / `_dropped`.
pub var notify_sent: counters.ShardedCounter = .{};
pub var notify_enoent: counters.ShardedCounter = .{};
pub var notify_failed: counters.ShardedCounter = .{};
pub var notify_dropped: counters.ShardedCounter = .{};
/// `transport_notify_epochs_owed` / `_epochs_sent`: an `INVAL_ENTRY` that
/// found no cell owes the kernel an `INC_EPOCH` instead (never dropped —
/// decision 0003 §AB.2); the epochs written (owed or asked).
pub var notify_epochs_owed: counters.ShardedCounter = .{};
pub var notify_epochs_sent: counters.ShardedCounter = .{};
/// `transport_notify_nb_waits`: a caller of a non-blocking notify waited
/// for a reserved cell.
pub var notify_nb_waits: counters.ShardedCounter = .{};
/// `transport_sideband_recycle_respins`: a sideband cell given back met its
/// free list's cell still held by a taker between its dequeue CAS and its
/// sequence store (the ring holds the pool's whole population, so the
/// refusal is never "full"): the give-back retried (decision 0003 §AI.4).
pub var sideband_recycle_respins: counters.ShardedCounter = .{};
/// `transport_notify_pending`: fire-and-forget writes (notifies, inline
/// replies) filled and not yet answered by the kernel — what "at notify
/// speed" still owes (decision 0003 §AG.2: a recall's names are queued
/// before its ack and written by the loop after it).
pub var notify_pending: counters.Gauge = .{};
/// `transport_sideband_cells_exhausted` (must-stay-0): the sideband itself
/// had no cell for an inline reply.
pub var sideband_cells_exhausted: counters.Tripwire = .init("transport_sideband_cells_exhausted");
/// `transport_arena_bytes`: the payload arenas (the budget's non-sheddable
/// `transport_payload_arena` component reads it).
pub var arena_bytes: counters.Gauge = .{};
/// `transport_umount_busy_retries`: plain `umount2` attempts that met EBUSY
/// inside the retry window.
pub var umount_busy_retries: counters.ShardedCounter = .{};
/// `transport_umount_busy_refusals`: orderly stops (SIGTERM, `squeezefs
/// umount`) refused because the holder census found the mount busy
/// (decision 0003 §S); the daemon kept serving.
pub var umount_busy_refusals: counters.ShardedCounter = .{};
/// `transport_zc_bounce_reserve_bytes`: the zc bounce slab's virtual
/// reserve (`nqueues × depth × stride`); the pages it holds are
/// `worker.zc_bounce_resident_bytes` (`transport_zc_bounce_bytes`), the
/// budget component's gauge.
pub var zc_bounce_slab_bytes: counters.Gauge = .{};
/// `transport_time_limits`: 1 when FUSE_TIME_LIMITS was echoed with our
/// i64-ns whole-second range.
pub var time_limits_gauge: counters.Gauge = .{};
/// `transport_request_timeout_s`: the request timeout the kernel applies to
/// this connection (the INIT reply's, capped by `max_request_timeout`,
/// floored at the check period), seconds; 0 = none negotiated.
pub var request_timeout_gauge: counters.Gauge = .{};
/// `transport_kernel_max_background`: fusectl's `max_background` for this
/// connection (0 when fusectl is not mounted) — the per-queue background
/// budget is `this / nr_queues` kernel-side.
pub var kernel_max_background: counters.Gauge = .{};
/// `transport_releases_requested`: `releaseRetained` calls that reached a
/// worker (the economy check admitted them).
pub var releases_requested: counters.ShardedCounter = .{};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

pub const MountError = error{
    OutOfMemory,
    DevFuseOpenFailed,
    MountFailed,
    InitReadFailed,
    InitNotFirst,
    InitUnsupported,
    InitReplyFailed,
    RingSetupFailed,
    ArenaMapFailed,
    ThreadSpawnFailed,
    ArmBarrierFailed,
    EveryEntRetired,
    /// An explicit `SQUEEZEFS_FUSE_BUFFER_MODE` the probes refused (named
    /// in the log) — the only degrade point is the probe; never a downgrade.
    BufferModeRefused,
    /// An explicit `SQUEEZEFS_FUSE_RINGS_PER_QUEUE` above what the FEATURES
    /// probe admits (named in the log): the kernel would accept the second
    /// ring and recycle its buffers into the first's.
    RingsPerQueueRefused,
    /// The kmbuf ring's registration answered ENOMEM after every retry
    /// (`kmbuf.Error`): the probe decided the mode, so the law is fail
    /// loud, never degrade; `Config.refusal` carries the region and levers.
    KmbufRingEnomem,
    SlabFailed,
    /// `SQUEEZEFS_TEST_FUSE_FAIL_AFTER_INIT_REPLY`: the bring-up failed by
    /// the seam after the INIT reply, before any queue registered.
    InjectedStartupFailure,
};

/// The operator-facing detail of a typed refusal: `MountError` names the
/// class, this names the numbers the error alone cannot carry. Written by
/// `mount` into `Config.refusal` before the error returns.
pub const Refusal = union(enum) {
    none,
    /// `KmbufRingEnomem`: one queue's region (`buffers × buf_size`, `pages`
    /// at once) found too few free pages (`kmbuf.Error`'s law), charged to
    /// `RLIMIT_MEMLOCK` when the process lacks `CAP_IPC_LOCK`.
    kmbuf_ring_enomem: struct { pages: u64, buffers: u32, buf_size: u32, memlock_charged: bool },

    pub fn format(self: Refusal, w: *Io.Writer) Io.Writer.Error!void {
        switch (self) {
            .none => try w.writeAll("no refusal detail"),
            .kmbuf_ring_enomem => |r| {
                try w.print("kmbuf ring registration ENOMEM: a queue's region takes {d} pages at once ({d} × {d} B); the kernel falls back to order-0 pages but draws them from the free lists only (no reclaim) — free memory, or shrink the region (SQUEEZEFS_FUSE_MAX_WRITE: the buffer; SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH: the buffers per queue), or SQUEEZEFS_FUSE_BUFFER_MODE=user", .{ r.pages, r.buffers, r.buf_size });
                if (r.memlock_charged) try w.writeAll("; without CAP_IPC_LOCK the region is charged to RLIMIT_MEMLOCK: raise it or grant the capability");
            },
        }
    }
};

/// A caller's callback with its context (the daemon's hooks).
pub const Hook = struct {
    ctx: *anyopaque,
    call: *const fn (ctx: *anyopaque) void,

    pub fn fire(self: Hook) void {
        self.call(self.ctx);
    }
};

/// The unprivileged path's unmounter (`fusermount3 -u`, `-u -z` when
/// `lazy`): true once the mountpoint is detached.
pub const Unmounter = struct {
    ctx: *anyopaque,
    call: *const fn (ctx: *anyopaque, mountpoint: [:0]const u8, lazy: bool) bool,
};

pub const Config = struct {
    opts: mount_options.MountOptions = .{},
    /// The volume's block size: the INIT `max_write` desire is
    /// `max(block, 1 MiB)`.
    block_size: u64 = 4 * derive.MiB,
    /// The R5 budget the payload-arena cap derives from (`budget/8`).
    mem_budget_bytes: u64,
    /// Register the arena as a non-sheddable component here.
    budget: ?*mem_budget.MemBudget = null,
    /// A `/dev/fuse` fd an unprivileged mount already received from
    /// `fusermount3` (`cli/fusermount.zig`): the session skips `open` and
    /// `mount(2)` and unmounts through `unmounter` (root's `umount2` is
    /// EPERM there).
    fuse_fd: ?linux.fd_t = null,
    unmounter: ?Unmounter = null,
    /// Fired ONCE when the session shuts down for any reason — DESTROY
    /// from the kernel (an external umount), a disconnect, a failed queue,
    /// or `close` — so the daemon's main task can begin its teardown.
    on_shutdown: ?Hook = null,
    /// Written with a typed refusal's detail before `mount` returns its
    /// error (the daemon's refusal line names it).
    refusal: ?*Refusal = null,
    /// The INIT reply's request timeout, seconds: the fail-stop lattice's
    /// `geometry.requestTimeoutS` (decision 0003 §Z), the
    /// `SQUEEZEFS_TEST_FUSE_REQUEST_TIMEOUT_S` seam wins. 0 asks for none —
    /// and so declines FUSE_SYNCFS (the session tests' EchoFs); the product
    /// mount always derives one.
    request_timeout_s: u16 = 0,
    /// The filesystem answers OPEN with ENOSYS (decision 0003 §AF, the
    /// mount's `SQUEEZEFS_FUSE_NO_OPEN`): INIT drops FUSE_ATOMIC_O_TRUNC
    /// (no OPEN will carry a truncate) and every FLUSH — the drain at
    /// close — is probed on the worker, else handed to the data lane.
    no_open: bool = false,
    /// Error logs each worker/sideband thread expects in a test.
    test_error_logs: usize = 0,
};

// ---------------------------------------------------------------------------
// The sideband
// ---------------------------------------------------------------------------

const SbCell = struct {
    done: completion.Completion(i32) = .{},
    buf: [SIDEBAND_CELL_BYTES]u8 = undefined,
    head_len: usize = 0,
    body: []const u8 = &.{},
    iov: [2]std.posix.iovec_const = undefined,
    /// Nobody waits: the sideband recycles the cell at the CQE.
    fire_and_forget: bool = true,
};

/// One of the sideband's cell pools: its free list — a ring holding the
/// pool's WHOLE population — and the recycle word a taker out of cells
/// parks on. Any thread takes; the sideband (a CQE) and a blocking reply's
/// waiter give back.
const CellPool = struct {
    free: mpmc.BoundedMpmc(u32),
    freed: futex.Word = .init(0),

    /// Cells `[first, first + n)`, every one free.
    fn init(gpa: std.mem.Allocator, first: usize, n: usize) std.mem.Allocator.Error!CellPool {
        var free = try mpmc.BoundedMpmc(u32).init(gpa, n);
        for (first..first + n) |i| assert(free.tryPush(@intCast(i)));
        return .{ .free = free };
    }

    fn deinit(self: *CellPool, gpa: std.mem.Allocator) void {
        self.free.deinit(gpa);
    }

    /// Give a cell back and wake a taker out of cells. The ring holds the
    /// pool's whole population, so a refused push is never "full": it is a
    /// taker between its dequeue CAS and its sequence store, with a second
    /// taker's pop and this give-back run past it — retried until that
    /// taker stores (`mpmc.pushSpin`; the model `sync/mpmc_pool_model.zig`,
    /// decision 0003 §AI.4).
    fn give(self: *CellPool, idx: u32) void {
        if (!self.free.tryPush(idx)) {
            sideband_recycle_respins.inc();
            self.free.pushSpin(idx);
        }
        _ = self.freed.fetchAdd(1, .release);
        futex.wakeAll(&self.freed);
    }
};

const Sideband = struct {
    const read_ud: u64 = 1;
    const cancel_ud: u64 = 2;
    const write_ud_base: u64 = 1 << 32;

    session: *Session,
    ring: ring_park.ParkedRing,
    fuse_fd: linux.fd_t,
    fixed: bool,
    buf: []u8,
    cells: []SbCell,
    /// Cells `[0, nb_base)`: the shared pool.
    pool: CellPool,
    ready: mpmc.BoundedMpmc(u32),
    /// Cells `[nb_base, cells.len)`: the non-blocking class's own pool.
    nb_base: u32,
    nb_pool: CellPool,
    /// An `INVAL_ENTRY` found no cell: the loop writes an `INC_EPOCH`.
    epoch_owed: std.atomic.Value(bool) = .init(false),
    read_armed: bool = false,
    stop: std.atomic.Value(bool) = .init(false),
    disconnected: bool = false,
    thread: ?std.Thread = null,
    name_buf: thread_name.CommBuf = undefined,

    fn init(session: *Session, gpa: std.mem.Allocator, fuse_fd: linux.fd_t, buf_len: usize, ncells: usize) MountError!Sideband {
        var ring = ring_park.ParkedRing.init(gpa, sidebandEntries(ncells), 0) catch return error.RingSetupFailed;
        errdefer ring.deinit();
        const fixed = ring.registerFile(fuse_fd);
        const buf = try gpa.alloc(u8, buf_len);
        errdefer gpa.free(buf);
        const total = ncells + SIDEBAND_NB_CELLS;
        const cells = try gpa.alloc(SbCell, total);
        errdefer gpa.free(cells);
        for (cells) |*c| c.* = .{};
        var pool = try CellPool.init(gpa, 0, ncells);
        errdefer pool.deinit(gpa);
        var nb_pool = try CellPool.init(gpa, ncells, SIDEBAND_NB_CELLS);
        errdefer nb_pool.deinit(gpa);
        const ready = try mpmc.BoundedMpmc(u32).init(gpa, total);
        return .{ .session = session, .ring = ring, .fuse_fd = fuse_fd, .fixed = fixed, .buf = buf, .cells = cells, .pool = pool, .ready = ready, .nb_base = @intCast(ncells), .nb_pool = nb_pool };
    }

    fn deinit(self: *Sideband, gpa: std.mem.Allocator) void {
        assert(self.thread == null);
        self.ring.deinit();
        self.ready.deinit(gpa);
        self.nb_pool.deinit(gpa);
        self.pool.deinit(gpa);
        gpa.free(self.cells);
        gpa.free(self.buf);
    }

    fn sqeFd(self: *const Sideband) linux.fd_t {
        return if (self.fixed) 0 else self.fuse_fd;
    }

    fn markFixed(self: *const Sideband, sqe: *linux.io_uring_sqe) void {
        if (self.fixed) sqe.flags |= linux.IOSQE_FIXED_FILE;
    }

    /// One classical READ of `/dev/fuse` on the calling thread (the INIT
    /// exchange, before the loop owns the ring). Returns the byte count.
    fn readOnce(self: *Sideband) MountError!usize {
        const sqe = self.ring.getSqe() orelse return error.InitReadFailed;
        sqe.prep_read(self.sqeFd(), self.buf, 0);
        self.markFixed(sqe);
        sqe.user_data = read_ud;
        const cqe = self.awaitOne(read_ud) orelse return error.InitReadFailed;
        if (cqe.res < 0) {
            log.err("transport: the INIT read on /dev/fuse failed ({t})", .{cqe.err()});
            return error.InitReadFailed;
        }
        return @intCast(cqe.res);
    }

    /// One classical WRITE on the calling thread (the INIT reply).
    fn writeOnce(self: *Sideband, bytes: []const u8) MountError!void {
        const sqe = self.ring.getSqe() orelse return error.InitReplyFailed;
        sqe.prep_write(self.sqeFd(), bytes, 0);
        self.markFixed(sqe);
        sqe.user_data = write_ud_base;
        const cqe = self.awaitOne(write_ud_base) orelse return error.InitReplyFailed;
        if (cqe.res != @as(i32, @intCast(bytes.len))) {
            log.err("transport: the INIT reply write returned {d} for {d} bytes", .{ cqe.res, bytes.len });
            return error.InitReplyFailed;
        }
    }

    fn awaitOne(self: *Sideband, ud: u64) ?linux.io_uring_cqe {
        _ = self.ring.ring.submit_and_wait(1) catch return null;
        var cqes: [4]linux.io_uring_cqe = undefined;
        while (true) {
            const n = self.ring.ring.copy_cqes(&cqes, 1) catch return null;
            for (cqes[0..n]) |cqe| if (cqe.user_data == ud) return cqe;
        }
    }

    // ---- the cell path (any thread) ----

    /// Take a free cell, waiting on the recycle word when none is free
    /// (a straggler reply must never be dropped); null when `wait` is false
    /// and none is free.
    fn takeCell(self: *Sideband, wait: bool) ?u32 {
        while (true) {
            if (self.pool.free.tryPop()) |i| {
                self.cells[i].done.reset();
                return i;
            }
            if (!wait) return null;
            const gen = self.pool.freed.load(.acquire);
            if (self.pool.free.tryPop()) |i| {
                self.cells[i].done.reset();
                return i;
            }
            // The lock plane's ticked park: a lost recycle costs one tick.
            _ = futex.waitTimeout(&self.pool.freed, gen, tick_law.tick_ns);
        }
    }

    fn recycle(self: *Sideband, idx: u32) void {
        if (idx >= self.nb_base) self.nb_pool.give(idx) else self.pool.give(idx);
    }

    /// A reserved cell for a non-blocking notify; `wait` parks on the
    /// class's recycle word (a reserved cell's write never waits in the
    /// kernel, so the park is one sideband round) until the session ends.
    fn takeNbCell(self: *Sideband, wait: bool) ?u32 {
        var waited = false;
        while (true) {
            if (self.nb_pool.free.tryPop()) |i| {
                self.cells[i].done.reset();
                return i;
            }
            if (!wait or self.disconnected or self.stop.load(.acquire)) return null;
            if (!waited) notify_nb_waits.inc();
            waited = true;
            const gen = self.nb_pool.freed.load(.acquire);
            if (self.nb_pool.free.tryPop()) |i| {
                self.cells[i].done.reset();
                return i;
            }
            _ = futex.waitTimeout(&self.nb_pool.freed, gen, tick_law.tick_ns);
        }
    }

    /// A non-blocking-class notify (attrs-only `INVAL_INODE`, `INC_EPOCH`)
    /// from a reserved cell, waiting for one; false = the session ended.
    fn notifyNb(self: *Sideband, comptime build: anytype, args: anytype) bool {
        if (self.disconnected or self.stop.load(.acquire)) return false;
        const idx = self.takeNbCell(true) orelse return false;
        self.fillNotify(idx, build, args) catch return false;
        self.publish(idx);
        return true;
    }

    /// `notifyNb` AWAITED: returns once the device write completed — the
    /// kernel has the attrs invalidated when its write returns, and a
    /// reserved cell's write never waits in the kernel, so the wait is one
    /// sideband round. What a ring write that grew a file owes before its
    /// ack (`ipc/host/inval.zig`). False = the session ended or the write
    /// failed (counted).
    fn notifyNbAwaited(self: *Sideband, comptime build: anytype, args: anytype) bool {
        if (self.disconnected or self.stop.load(.acquire)) return false;
        const idx = self.takeNbCell(true) orelse return false;
        self.fillNotify(idx, build, args) catch return false;
        const c = &self.cells[idx];
        // `fillNotify` marked it fire-and-forget: this one is waited on.
        c.fire_and_forget = false;
        notify_pending.sub(1);
        self.publish(idx);
        const res = c.done.wait();
        self.recycle(idx);
        if (res < 0) {
            const e: linux.E = @fromBackingInt(@as(u16, @intCast(-res)));
            if (e == .NOENT) notify_enoent.inc() else {
                notify_failed.inc();
                log.warn("transport: an awaited attrs invalidation failed ({t})", .{e});
            }
            return false;
        }
        notify_sent.inc();
        return true;
    }

    /// `INVAL_ENTRY` from the shared pool, never waiting (its write waits
    /// on the parent's `i_rwsem`, which a request may hold while it waits
    /// on this very caller); no cell ⇒ the loop owes the kernel an epoch.
    fn notifyEntryOrEpoch(self: *Sideband, parent: u64, name: []const u8) void {
        if (self.disconnected or self.stop.load(.acquire)) return;
        if (self.takeCell(false)) |idx| {
            if (self.fillNotify(idx, codec.notifyInvalEntry, .{ parent, name, @as(u32, 0) })) |_| {
                self.publish(idx);
                return;
            } else |_| {}
        }
        notify_epochs_owed.inc();
        self.oweEpoch();
    }

    /// The loop writes an `INC_EPOCH` at its next turn.
    fn oweEpoch(self: *Sideband) void {
        if (self.disconnected or self.stop.load(.acquire)) return;
        self.epoch_owed.store(true, .release);
        if (self.ring.wake()) slot.wake_writes.inc() else slot.wakes_elided.inc();
    }

    /// The loop's half of an owed epoch: a reserved cell without waiting
    /// (the loop recycles them itself); none free ⇒ owed still, retried at
    /// the next CQE.
    fn writeOwedEpoch(self: *Sideband) void {
        if (!self.epoch_owed.load(.acquire)) return;
        const idx = self.takeNbCell(false) orelse return;
        self.epoch_owed.store(false, .release);
        self.fillNotify(idx, codec.notifyIncEpoch, .{}) catch return self.recycle(idx);
        notify_epochs_sent.inc();
        self.pushWrite(idx);
    }

    fn fillNotify(self: *Sideband, idx: u32, comptime build: anytype, args: anytype) codec.NotifyError!void {
        const c = &self.cells[idx];
        const frame: codec.NotifyFrame = @call(.auto, build, .{@as([]u8, &c.buf)} ++ args) catch |e| {
            self.recycle(idx);
            notify_dropped.inc();
            return e;
        };
        c.head_len = frame.head.len;
        c.body = frame.body;
        markFireAndForget(c);
    }

    fn publish(self: *Sideband, idx: u32) void {
        assert(self.ready.tryPush(idx));
        if (self.ring.wake()) slot.wake_writes.inc() else slot.wakes_elided.inc();
    }

    /// A classical reply from any thread: `header ‖ out` copied into the
    /// cell, the body a view; returns once the device write completed.
    fn replyBlocking(self: *Sideband, r: codec.Reply) void {
        if (self.disconnected or self.stop.load(.acquire)) {
            replies_dropped_no_slot.inc();
            return;
        }
        const idx = self.takeCell(true).?;
        const c = &self.cells[idx];
        c.buf[0..codec.OUT_HEADER_LEN].* = abi.encode(r.header);
        @memcpy(c.buf[codec.OUT_HEADER_LEN..][0..r.out.len], r.out);
        c.head_len = codec.OUT_HEADER_LEN + r.out.len;
        c.body = r.body;
        c.fire_and_forget = false;
        self.publish(idx);
        const res = c.done.wait();
        self.recycle(idx);
        sideband_replies.inc();
        if (res < 0) {
            const e: linux.E = @fromBackingInt(@as(u16, @intCast(-res)));
            if (e == .NOENT) log.warn("transport: classical reply for unique {d}: ENOENT — the request was interrupted", .{r.header.unique}) else log.err("transport: classical reply for unique {d} failed ({t})", .{ r.header.unique, e });
        }
    }

    /// A write nobody waits on: the kernel's answer is counted, never
    /// returned; pending until then (`transport_notify_pending`).
    fn markFireAndForget(c: *SbCell) void {
        c.fire_and_forget = true;
        notify_pending.add(1);
    }

    /// A notify frame built into a free cell by `build`; fire-and-forget.
    fn notify(self: *Sideband, comptime build: anytype, args: anytype) error{QueueFull}!void {
        if (self.disconnected or self.stop.load(.acquire)) return error.QueueFull;
        const idx = self.takeCell(false) orelse {
            notify_dropped.inc();
            return error.QueueFull;
        };
        const c = &self.cells[idx];
        const frame: codec.NotifyFrame = @call(.auto, build, .{@as([]u8, &c.buf)} ++ args) catch {
            self.recycle(idx);
            notify_dropped.inc();
            return error.QueueFull;
        };
        c.head_len = frame.head.len;
        c.body = frame.body;
        markFireAndForget(c);
        self.publish(idx);
    }

    /// An inline header-only answer from the sideband thread itself.
    fn replyInline(self: *Sideband, r: codec.Reply) void {
        const idx = self.takeCell(false) orelse {
            sideband_cells_exhausted.fire("no cell for an inline sideband reply");
            return;
        };
        const c = &self.cells[idx];
        c.buf[0..codec.OUT_HEADER_LEN].* = abi.encode(r.header);
        @memcpy(c.buf[codec.OUT_HEADER_LEN..][0..r.out.len], r.out);
        c.head_len = codec.OUT_HEADER_LEN + r.out.len;
        c.body = &.{};
        markFireAndForget(c);
        assert(self.ready.tryPush(idx));
    }

    // ---- the loop ----

    fn start(self: *Sideband) MountError!void {
        self.thread = std.Thread.spawn(.{}, threadMain, .{self}) catch return error.ThreadSpawnFailed;
    }

    fn requestStop(self: *Sideband) void {
        self.stop.store(true, .release);
        self.ring.forceWake();
    }

    fn join(self: *Sideband) void {
        const t = self.thread orelse return;
        t.join();
        self.thread = null;
    }

    fn threadMain(self: *Sideband) void {
        _ = thread_name.nameCurrentThread(&self.name_buf, "sqz-fuse-sb");
        if (builtin.is_test) log.testing.expectErrorLogs(self.session.test_error_logs);
        while (!self.stop.load(.acquire) and !self.disconnected and !self.ring.isDead()) {
            _ = self.ring.service();
            while (self.ready.tryPop()) |idx| self.pushWrite(idx);
            self.writeOwedEpoch();
            if (!self.read_armed) self.armRead();
            _ = self.ring.armWake();
            _ = self.ring.park(null);
            self.reap();
        }
        // A connection gone without DESTROY (a lazy umount, a sysfs abort,
        // a dead ring): the session ends here too.
        if (self.disconnected or self.ring.isDead()) {
            // Our own fail-stop's end is no kernel verdict and leaves the
            // mount ours to unmount (acquire pairs with `failStop`'s
            // release); release pairs with `endedUnderLiveMount`'s acquire.
            if (!self.session.self_aborted.load(.acquire)) self.session.lost_while_mounted.store(self.session.mounted.swap(false, .acq_rel), .release);
            self.session.shutdown();
        }
        self.finish();
    }

    fn armRead(self: *Sideband) void {
        const sqe = self.ring.getSqe() orelse return;
        sqe.prep_read(self.sqeFd(), self.buf, 0);
        self.markFixed(sqe);
        sqe.user_data = read_ud;
        self.read_armed = true;
    }

    fn pushWrite(self: *Sideband, idx: u32) void {
        const c = &self.cells[idx];
        const sqe = self.ring.getSqe() orelse {
            self.completeCell(idx, -@as(i32, @intCast(@backingInt(linux.E.AGAIN))));
            return;
        };
        if (c.body.len == 0) {
            sqe.prep_write(self.sqeFd(), c.buf[0..c.head_len], 0);
        } else {
            c.iov = .{ .{ .base = &c.buf, .len = c.head_len }, .{ .base = c.body.ptr, .len = c.body.len } };
            sqe.prep_writev(self.sqeFd(), &c.iov, 0);
        }
        self.markFixed(sqe);
        sqe.user_data = write_ud_base + idx;
    }

    fn completeCell(self: *Sideband, idx: u32, res: i32) void {
        const c = &self.cells[idx];
        if (c.fire_and_forget) {
            notify_pending.sub(1);
            if (res < 0) {
                const e: linux.E = @fromBackingInt(@as(u16, @intCast(-res)));
                if (e == .NOENT) notify_enoent.inc() else {
                    notify_failed.inc();
                    log.warn("transport: a sideband write failed ({t})", .{e});
                }
            } else notify_sent.inc();
            self.recycle(idx);
            return;
        }
        _ = c.done.complete(res);
    }

    fn reap(self: *Sideband) void {
        var cqes: [32]linux.io_uring_cqe = undefined;
        while (true) {
            const n = self.ring.ring.copy_cqes(&cqes, 0) catch {
                self.ring.markDead();
                return;
            };
            for (cqes[0..n]) |cqe| {
                if (self.ring.consumeWake(cqe)) continue;
                if (cqe.user_data == read_ud) {
                    self.read_armed = false;
                    if (cqe.res > 0) {
                        self.handleRead(@intCast(cqe.res));
                    } else if (cqe.res < 0) {
                        const e = cqe.err();
                        if (e == .INTR or e == .AGAIN) continue;
                        if (worker.isDisconnect(e)) {
                            log.debug("transport: /dev/fuse read: {t} — the connection is gone", .{e});
                        } else {
                            log.err("transport: /dev/fuse read failed ({t}) — the session ends", .{e});
                        }
                        self.disconnected = true;
                    }
                } else if (cqe.user_data >= write_ud_base) {
                    const idx: u32 = @intCast(cqe.user_data - write_ud_base);
                    if (idx < self.cells.len) self.completeCell(idx, cqe.res);
                }
            }
            if (n < cqes.len) return;
        }
    }

    fn handleRead(self: *Sideband, n: usize) void {
        sideband_deliveries.inc();
        const d = codec.Delivery.fromClassical(self.buf, n) catch return;
        const parsed = codec.parse(d) catch |e| {
            if (codec.opInfo(d.header.op())) |info| if (info.no_reply) return;
            self.replyInline(codec.replyError(d.header.unique, codec.parseErrno(e)));
            return;
        };
        const s = self.session;
        switch (parsed) {
            .forget => |f| {
                sideband_forgets.inc();
                s.fs.forget(d.header.nodeid, f.nlookup);
            },
            .batch_forget => |b| {
                sideband_forgets.add(b.entries.len);
                for (b.entries) |e| s.fs.forget(e.nodeid, e.nlookup);
            },
            .interrupt => sideband_interrupts.inc(),
            .notify_reply => {},
            .init => {
                // A duplicate INIT: answer it with the negotiated reply again.
                log.warn("transport: a duplicate INIT (unique {d}) on the sideband — re-answered", .{d.header.unique});
                var out: codec.OutBuf = undefined;
                self.replyInline(codec.replyInit(&out, d.header.unique, s.init_reply));
            },
            .destroy => {
                s.destroyOnce();
                self.replyInline(codec.replyOk(d.header.unique));
                // The kernel is unmounting (release: `unmount` acquires).
                s.mounted.store(false, .release);
                s.shutdown();
            },
            .unsupported => self.replyInline(codec.replyEnosys(d.header.unique)),
            else => {
                // A switch-over straggler: a lane handler, replied classically.
                sideband_requests.inc();
                const req: fsv.Request = .{ .header = d.header, .op = parsed };
                const handle: fsv.Handle = .{ .tx = &s.tx, .slot = .classical, .unique = d.header.unique, .window = &.{} };
                const lane = s.exec.homeLane(null, null);
                s.exec.spawnGroupOn(lane, &s.handler_group, handlerMain, .{ s.fs, req, handle }) catch {
                    worker.dispatch_refused.inc();
                    self.replyInline(codec.replyError(d.header.unique, .AGAIN));
                };
            },
        }
    }

    /// Loop exit: cancel the standing READ, answer every waiter ENOTCONN.
    fn finish(self: *Sideband) void {
        if (self.read_armed and !self.ring.isDead()) {
            if (self.ring.getSqe()) |sqe| {
                sqe.prep_cancel(read_ud, 0);
                sqe.user_data = cancel_ud;
                _ = self.ring.ring.submit() catch {};
                var cqes: [8]linux.io_uring_cqe = undefined;
                var seen_read = false;
                var seen_cancel = false;
                var rounds: u32 = 0;
                while (!(seen_read and seen_cancel) and rounds < 64) : (rounds += 1) {
                    const n = self.ring.ring.copy_cqes(&cqes, 1) catch break;
                    for (cqes[0..n]) |cqe| {
                        if (cqe.user_data == read_ud) seen_read = true;
                        if (cqe.user_data == cancel_ud) seen_cancel = true;
                        if (cqe.user_data >= write_ud_base) {
                            const idx: u32 = @intCast(cqe.user_data - write_ud_base);
                            if (idx < self.cells.len) self.completeCell(idx, cqe.res);
                        }
                    }
                }
            }
            self.read_armed = false;
        }
        self.disconnected = true;
        while (self.ready.tryPop()) |idx| self.completeCell(idx, -@as(i32, @intCast(@backingInt(linux.E.NOTCONN))));
        self.ring.quiesce();
    }
};

fn handlerMain(fs: fsv.Filesystem, req: fsv.Request, handle: fsv.Handle) void {
    fs.dispatch(req, handle, null);
}

// ---------------------------------------------------------------------------
// The session
// ---------------------------------------------------------------------------

pub const Session = struct {
    gpa: std.mem.Allocator,
    io: Io,
    env: *const std.process.Environ.Map,
    facts: derive.Facts,
    exec: *SqzIo,
    fs: fsv.Filesystem,
    mountpoint: [:0]u8,
    source: [:0]u8,
    fuse_fd: linux.fd_t = -1,
    /// Whether the mountpoint is attached. Cleared by the sideband at
    /// DESTROY (the kernel is unmounting: our own `umount2` afterwards
    /// could detach a NEW mount at the path) — release there, acquire in
    /// `unmount`, ordered through `shutdown`'s swap.
    mounted: std.atomic.Value(bool) = .init(false),
    /// The mount's id in the mount table at `mount(2)` (null when the table
    /// was unreadable): the proof `unmount` demands before an `umount2` by
    /// path.
    mount_id: ?u64 = null,
    unmounter: ?Unmounter = null,
    on_shutdown: ?Hook = null,
    init_reply: codec.InitReply = undefined,
    init_unique: u64 = 0,
    plan: geometry.Plan = undefined,
    drain_groups: geometry.DrainGroups = undefined,
    topo: placement.Topology = .empty,
    shared: worker.Shared = undefined,
    groups: []*worker.Group = &.{},
    qid_group: []u16 = &.{},
    qid_member: []u16 = &.{},
    handler_group: Io.Group = .init,
    tx: fsv.Tx = undefined,
    sideband: Sideband = undefined,
    have_sideband: bool = false,
    arena_component: mem_budget.Component = undefined,
    /// The lattice rung and the verdicts it was decided from.
    mode: geometry.BufferMode = .user_ents,
    verdicts: probes.Verdicts = .{ .kmbuf = .absent, .zc = .absent },
    retention: probes.Verdict = .absent,
    time_limits: probes.Verdict = .absent,
    /// The FEATURES probe's rings-per-queue bit (series patch 0030).
    rings_feature: probes.Verdict = .absent,
    /// Rings (groups, `sqz-fuse-ur<qid>` workers) registered on each queue:
    /// `drain_groups.count() × rings_per_queue` groups, a queue's rings at
    /// consecutive indices from `qid_group[qid]`.
    rings_per_queue: u32 = 1,
    /// `SQUEEZEFS_READ_ZC_SERVE` ∧ zero-copy: what INIT told the filesystem.
    read_zc_serve: bool = false,
    /// The request timeout the kernel applies, seconds (0 = none).
    request_timeout_s: u16 = 0,
    /// The connection ended (not by DESTROY) while the session still held
    /// the mount attached — the sideband's verdict, judged against the
    /// mount table by `endedUnderLiveMount`.
    lost_while_mounted: std.atomic.Value(bool) = .init(false),
    /// `failStop` ran (once per session).
    fail_stopped: bool = false,
    /// `failStop` set out to end the connection itself: the sideband's
    /// disconnect is then ours, never the kernel's verdict, and the mount
    /// stays ours to unmount (release in `failStop`, acquire at the
    /// sideband's disconnect).
    self_aborted: std.atomic.Value(bool) = .init(false),
    /// fusectl's `abort` for our connection was written.
    abort_written: bool = false,
    /// The zc bounce slab (one per session) and its budget component.
    slab: ?worker.Slab = null,
    slab_component: mem_budget.Component = undefined,
    kmbuf_component: mem_budget.Component = undefined,
    ready: std.atomic.Value(bool) = .init(false),
    destroyed: std.atomic.Value(bool) = .init(false),
    closed: bool = false,
    test_error_logs: usize = 0,

    // ---- the duck-typed transport face (handler/transport_seam.zig) ----
    pub const SlotRef = fsv.ReplySlot;
    pub const PayloadLease = slot.PayloadLease;

    /// A slot address resolved to its ring: the group among the queue's
    /// `rings_per_queue` (consecutive from `qid_group[qid]`) whose share
    /// holds the queue-level slot index, and the gent there. Null for an
    /// address outside the plan.
    const Located = struct { g: *worker.Group, gent: u32 };

    fn locate(self: *Session, qid: u16, slot_idx: u16) ?Located {
        if (qid >= self.plan.nqueues or slot_idx >= self.plan.depth) return null;
        const first = self.qid_group[qid];
        const ms = self.qid_member[qid];
        for (self.groups[first .. first + self.rings_per_queue]) |g| {
            if (g.ownsSlot(slot_idx)) return .{ .g = g, .gent = g.gentOfSlot(ms, slot_idx) };
        }
        return null;
    }

    /// The ent's payload window (empty on the classical venue).
    pub fn replyWindow(self: *Session, s: SlotRef) []u8 {
        switch (s) {
            .classical => return &.{},
            .ring => |r| {
                const at = self.locate(r.qid, r.ent_idx) orelse return &.{};
                return at.g.window(at.gent);
            },
        }
    }

    /// THE reply: `header ‖ out ‖ body` to the slot.
    pub fn submitReply(self: *Session, s: SlotRef, r: codec.Reply) void {
        switch (s) {
            .classical => self.sideband.replyBlocking(r),
            .ring => |ref| self.ringSubmit(ref, commit_queue.Msg.reply(ref, r)),
        }
    }

    /// The zc fd→slot bridge; the body stands in on a UserEnts queue and on
    /// the classical venue.
    pub fn submitReplyFdBody(self: *Session, s: SlotRef, r: codec.Reply, fd: linux.fd_t, off: u64) void {
        switch (s) {
            .classical => self.sideband.replyBlocking(r),
            .ring => |ref| self.ringSubmit(ref, commit_queue.Msg.fdBody(ref, r, fd, off)),
        }
    }

    /// Header only: `n` bytes already sit in the slot's payload.
    pub fn submitReplyPrefilled(self: *Session, s: SlotRef, unique: u64, n: u32) void {
        switch (s) {
            .classical => {
                // Nothing can be "prefilled" classically: an EIO names the bug.
                self.sideband.replyBlocking(codec.replyError(unique, .IO));
            },
            .ring => |ref| self.ringSubmit(ref, commit_queue.Msg.prefilledMsg(ref, unique, n)),
        }
    }

    /// The one non-reply exit: header-only `-errno`.
    pub fn failEnt(self: *Session, s: SlotRef, unique: u64, errno: linux.E) void {
        switch (s) {
            .classical => self.sideband.replyBlocking(codec.replyError(unique, errno)),
            .ring => |ref| self.ringSubmit(ref, commit_queue.Msg.fail(ref, unique, errno)),
        }
    }

    /// The RETAIN commit (zero-copy retention); a plain reply classically
    /// and on a session without retention (the worker counts the refusal).
    pub fn submitReplyRetain(self: *Session, s: SlotRef, r: codec.Reply) void {
        switch (s) {
            .classical => self.sideband.replyBlocking(r),
            .ring => |ref| self.ringSubmit(ref, commit_queue.Msg.replyRetain(ref, r)),
        }
    }

    /// `RELEASE_PAYLOAD` for a retained slot: rides the worker's release
    /// inbox (never a commit cell — the retain reply may still hold it), the
    /// worker orders it after the pass's commits.
    pub fn releaseRetained(self: *Session, s: SlotRef) void {
        const ref = switch (s) {
            .classical => return,
            .ring => |r| r,
        };
        const at = self.groupOf(ref) orelse return;
        releases_requested.inc();
        at.g.requestRelease(at.gent);
    }

    pub fn releaseLease(self: *Session, l: PayloadLease) void {
        _ = self;
        l.release();
    }

    /// The HELD WRITE payload's direct store (charter §4 "paged WRITE
    /// payloads are HELD in the slot … so the handler can `WRITE_FIXED(device
    /// ← slot)`"): bridge `src`'s slot straight into `fd @ off` on the ring
    /// that registered it, parking the caller's fiber until the CQE. The
    /// data plane's device seam calls this through `heldStoreFace`.
    pub fn storeHeld(self: *Session, src: slot.HeldSource, fd: linux.fd_t, off: u64, io: Io) slot.ExtractError!void {
        const at = self.locate(src.qid, src.buf_index) orelse return error.SlotNotHeld;
        if (!self.shared.isActive()) return error.SessionClosed;
        return at.g.storeFromForeign(at.gent, fd, off, src.len, io);
    }

    /// Whether `storeHeld` can land anything: a zero-copy session (a held
    /// payload exists only there).
    pub fn heldStoreArmed(self: *const Session) bool {
        return self.mode == .zero_copy;
    }

    /// The fd-source serve lever as INIT resolved it.
    pub fn readZcServe(self: *const Session) bool {
        return self.read_zc_serve;
    }

    /// The group (and gent) owning a live slot that still names
    /// `ref.commit_id` (the economy check; the worker's slot table is the
    /// authority).
    fn groupOf(self: *Session, ref: slot.SlotRef) ?Located {
        const at = self.locate(ref.qid, ref.ent_idx) orelse {
            replies_dropped_no_slot.inc();
            return null;
        };
        const ent = at.g.members[self.qid_member[ref.qid]].slots.ent(at.g.entIdxOf(at.gent));
        if (!self.shared.isActive() or ent.owed_commit_id.load(.acquire) != ref.commit_id) {
            replies_dropped_no_slot.inc();
            return null;
        }
        return at;
    }

    fn ringSubmit(self: *Session, ref: slot.SlotRef, msg: commit_queue.Msg) void {
        const at = self.groupOf(ref) orelse return;
        _ = at.g.cells.submit(at.gent, msg);
    }

    // ---- the Tx face handlers reply through ----

    fn txSubmit(ctx: *anyopaque, h: fsv.Handle, r: codec.Reply) void {
        const self: *Session = @ptrCast(@alignCast(ctx));
        self.submitReply(h.slot, r);
    }
    fn txSubmitFdBody(ctx: *anyopaque, h: fsv.Handle, r: codec.Reply, fd: linux.fd_t, off: u64) void {
        const self: *Session = @ptrCast(@alignCast(ctx));
        self.submitReplyFdBody(h.slot, r, fd, off);
    }
    fn txSubmitPrefilled(ctx: *anyopaque, h: fsv.Handle, n: u32) void {
        const self: *Session = @ptrCast(@alignCast(ctx));
        self.submitReplyPrefilled(h.slot, h.unique, n);
    }
    fn txFail(ctx: *anyopaque, h: fsv.Handle, errno: linux.E) void {
        const self: *Session = @ptrCast(@alignCast(ctx));
        self.failEnt(h.slot, h.unique, errno);
    }
    fn txSubmitRetain(ctx: *anyopaque, h: fsv.Handle, r: codec.Reply) void {
        const self: *Session = @ptrCast(@alignCast(ctx));
        self.submitReplyRetain(h.slot, r);
    }
    fn txReleaseRetained(ctx: *anyopaque, h: fsv.Handle) void {
        const self: *Session = @ptrCast(@alignCast(ctx));
        self.releaseRetained(h.slot);
    }

    // ---- notify (the sideband carries them) ----

    /// `FUSE_NOTIFY_INVAL_INODE`: `off < 0` = attributes only; `(0, 0)` = every page.
    pub fn notifyInvalInode(self: *Session, ino: u64, off: i64, len: i64) error{QueueFull}!void {
        return self.sideband.notify(codec.notifyInvalInode, .{ ino, off, len });
    }

    /// `FUSE_NOTIFY_INVAL_ENTRY` (`flags`: 0 or `FUSE_EXPIRE_ONLY`).
    pub fn notifyInvalEntry(self: *Session, parent: u64, name: []const u8, flags: u32) error{QueueFull}!void {
        return self.sideband.notify(codec.notifyInvalEntry, .{ parent, name, flags });
    }

    /// `FUSE_NOTIFY_PRUNE` (uapi 7.45): a pre-7.45 kernel answers EINVAL,
    /// counted `transport_notify_failed`.
    pub fn notifyPrune(self: *Session, nodeids: []const u64) error{QueueFull}!void {
        return self.sideband.notify(codec.notifyPrune, .{nodeids});
    }

    /// `FUSE_NOTIFY_INVAL_ENTRY` that is never dropped (decision 0003
    /// §AB.2): no free cell ⇒ the sideband writes an `INC_EPOCH` instead.
    pub fn notifyInvalEntryOwed(self: *Session, parent: u64, name: []const u8) void {
        self.sideband.notifyEntryOrEpoch(parent, name);
    }

    /// `FUSE_NOTIFY_INC_EPOCH` (every dentry stale at its next walk):
    /// owed to the sideband loop, which writes it from a reserved cell —
    /// the caller never waits (a recall handler, a journal lane).
    pub fn notifyIncEpoch(self: *Session) void {
        self.sideband.oweEpoch();
    }

    /// Attrs-only `FUSE_NOTIFY_INVAL_INODE`, WAITING for a reserved cell:
    /// for a caller nothing waits on (the kernel view's notifier) — the
    /// sideband thread itself may wait on a commit (a FORGET's destroy).
    pub fn notifyAttrs(self: *Session, ino: u64) void {
        _ = self.sideband.notifyNb(codec.notifyInvalInode, .{ ino, @as(i64, -1), @as(i64, 0) });
    }

    /// `notifyAttrs` that returns once the kernel took it (a ring write that
    /// grew the file announces its size before its ack: POSIX-8).
    pub fn notifyAttrsAwaited(self: *Session, ino: u64) void {
        _ = self.sideband.notifyNbAwaited(codec.notifyInvalInode, .{ ino, @as(i64, -1), @as(i64, 0) });
    }

    /// The token plane's kernel-invalidation face (a recalled object's
    /// `INVAL_INODE` + `PRUNE`, a recalled name's `INVAL_ENTRY`, an unnamed
    /// recall's `INC_EPOCH` ride the sideband like every notify).
    pub fn tokenNotifySeam(self: *Session) read_divert.NotifySeam {
        return .{ .ctx = self, .invalInode = &notifyInvalInodeFn, .prune = &notifyPruneFn, .invalEntry = &notifyInvalEntryFn, .incEpoch = &notifyIncEpochFn };
    }

    fn notifyInvalEntryFn(ctx: *anyopaque, parent: u64, name: []const u8) void {
        const self: *Session = @ptrCast(@alignCast(ctx));
        self.notifyInvalEntryOwed(parent, name);
    }

    fn notifyIncEpochFn(ctx: *anyopaque) void {
        const self: *Session = @ptrCast(@alignCast(ctx));
        self.notifyIncEpoch();
    }

    fn notifyInvalInodeFn(ctx: *anyopaque, ino: u64, off: i64, len: i64) error{QueueFull}!void {
        const self: *Session = @ptrCast(@alignCast(ctx));
        return self.notifyInvalInode(ino, off, len);
    }

    fn notifyPruneFn(ctx: *anyopaque, nodeids: []const u64) error{QueueFull}!void {
        const self: *Session = @ptrCast(@alignCast(ctx));
        return self.notifyPrune(nodeids);
    }

    // ---- bring-up ----

    fn nodeOfCpu(ctx: ?*const anyopaque, cpu: u32) ?u32 {
        const topo: *const placement.Topology = @ptrCast(@alignCast(ctx.?));
        if (cpu >= placement.max_cpus) return null;
        const n = topo.node_of[cpu];
        return if (n == placement.no_node) null else n;
    }

    /// `SQUEEZEFS_FUSE_STREAM_LANES` (the registry's one parser; its
    /// allowed set is the enum's field names).
    fn streamLanesKnob(env: *const std.process.Environ.Map) worker.Shared.StreamLanes {
        const s = knobs.enumKnob("SQUEEZEFS_FUSE_STREAM_LANES", env);
        return std.meta.stringToEnum(worker.Shared.StreamLanes, s) orelse .cpu;
    }

    /// THE ORDERED BRING-UP. `fs.init` runs once the geometry is planned.
    /// `cfg.fuse_fd` (fusermount3's, already mounted) is this call's on
    /// every path: a failure closes it and detaches the mount.
    pub fn mount(gpa: std.mem.Allocator, exec_io: *SqzIo, fs: fsv.Filesystem, mountpoint: []const u8, env: *const std.process.Environ.Map, facts: derive.Facts, cfg: Config) MountError!*Session {
        // Until the session holds it, a failure closes the device FIRST — the
        // connection's only reference, so its close ends the connection — and
        // only then detaches (decision 0003 §AN).
        var adopted = false;
        errdefer if (!adopted) if (cfg.fuse_fd) |fd| {
            _ = linux.close(fd);
            var pb: [linux.PATH_MAX]u8 = undefined;
            if (cfg.unmounter) |u| if (std.fmt.bufPrintSentinel(&pb, "{s}", .{mountpoint}, 0)) |p| {
                if (!u.call(u.ctx, p, true)) log.err("transport: fusermount3 -u -z {s} failed — a mount is left behind", .{mountpoint});
            } else |_| log.err("transport: {s} does not fit a path buffer — a mount is left behind", .{mountpoint});
        };
        const self = try gpa.create(Session);
        errdefer gpa.destroy(self);
        self.* = .{
            .gpa = gpa,
            .io = exec_io.io(),
            .env = env,
            .facts = facts,
            .exec = exec_io,
            .fs = fs,
            .mountpoint = try gpa.dupeSentinel(u8, mountpoint, 0),
            .source = undefined,
            .test_error_logs = cfg.test_error_logs,
        };
        errdefer gpa.free(self.mountpoint);
        self.source = try gpa.dupeSentinel(u8, cfg.opts.fsName(), 0);
        errdefer gpa.free(self.source);
        self.tx = .{ .ctx = self, .submit = txSubmit, .submit_fd_body = txSubmitFdBody, .submit_prefilled = txSubmitPrefilled, .fail = txFail, .submit_retain = txSubmitRetain, .release_retained = txReleaseRetained };
        self.unmounter = cfg.unmounter;
        self.on_shutdown = cfg.on_shutdown;

        // 0. The device and the mount: root opens `/dev/fuse` and calls
        //    `mount(2)`; an unprivileged mount arrives with the fd
        //    `fusermount3` sent (already mounted).
        if (cfg.fuse_fd) |fd| {
            self.fuse_fd = fd;
        } else {
            const fd_rc = linux.open(dev_fuse_path, .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
            if (linux.errno(fd_rc) != .SUCCESS) {
                log.err("transport: open({s}) failed ({t})", .{ dev_fuse_path, linux.errno(fd_rc) });
                return error.DevFuseOpenFailed;
            }
            self.fuse_fd = @intCast(fd_rc);
        }
        errdefer self.closeFd();
        if (cfg.fuse_fd == null) {
            var data_buf: [512]u8 = undefined;
            var w: Io.Writer = .fixed(&data_buf);
            cfg.opts.writeKernelData(self.fuse_fd, &w) catch return error.MountFailed;
            w.writeByte(0) catch return error.MountFailed;
            const mount_rc = linux.mount(self.source.ptr, self.mountpoint.ptr, "fuse", cfg.opts.msFlags(), @intFromPtr(w.buffered().ptr));
            if (linux.errno(mount_rc) != .SUCCESS) {
                log.err("transport: mount(2) of {s} at {s} failed ({t}); data '{s}'", .{ self.source, self.mountpoint, linux.errno(mount_rc), w.buffered()[0 .. w.buffered().len - 1] });
                return error.MountFailed;
            }
        }
        self.mounted.store(true, .release);
        errdefer {
            self.failStartup();
            self.unmountDetached(.startup);
        }
        adopted = true;
        // The mount's identity: every later `umount2` by path first proves
        // the mount at the path is still THIS one (a kernel-ended mount
        // may have a successor at the same path by the time we tear down).
        if (self.mountEntry()) |e| self.mount_id = e.id;

        // 1. The classical INIT read on the sideband ring. The buffer covers
        //    any max_write the plan can reach (`limit × page`) plus a header
        //    page, so the same buffer serves the sideband for the mount's life.
        const max_pages_limit = geometry.readMaxPagesLimit(self.io);
        const limit = geometry.pagesLimit(max_pages_limit);
        const buf_len: usize = @as(usize, limit) * facts.page_size + 4096;
        const ncells = sidebandCells(facts.possible_cpus);
        self.sideband = try Sideband.init(self, gpa, self.fuse_fd, buf_len, ncells);
        self.have_sideband = true;
        errdefer self.sideband.deinit(gpa);
        const n = try self.sideband.readOnce();
        const delivery = codec.Delivery.fromClassical(self.sideband.buf, n) catch return error.InitReadFailed;
        const first = codec.parse(delivery) catch return error.InitReadFailed;
        const init_in = switch (first) {
            .init => |i| i.in,
            else => |other| {
                log.err("transport: the first request on /dev/fuse was {s}, not INIT", .{@tagName(other)});
                self.sideband.replyInline(codec.replyError(delivery.header.unique, .IO));
                return error.InitNotFirst;
            },
        };
        self.init_unique = delivery.header.unique;

        // 2. Geometry BEFORE the reply is serialized; then the PROBES and
        //    the lattice decision (the drain-group width follows the mode:
        //    1 under kmbuf/zc), then the drain-group plan.
        self.plan = geometry.plan(geometry.resolveInputs(facts, env, cfg.block_size, cfg.mem_budget_bytes, max_pages_limit, cfg.opts.max_background, cfg.opts.congestion_threshold));
        self.topo = placement.Topology.probe(self.io);
        const qid_is_cpu = self.plan.nqueues == facts.possible_cpus;
        // 3. `fuse.enable_uring=Y` BEFORE the reply lands: since 7.2.4 the
        //    kernel creates the connection's ring at INIT-reply time iff the
        //    parameter reads Y then; a later REGISTER on a ring-less
        //    connection is -EINVAL. Written BEFORE the probes: the zc verdict
        //    needs the ring the reply will create, not the INIT request's
        //    offer (`probes.ringAtReply`).
        ensureKernelUringEnabled(self.io);
        try self.resolveMode(env, init_in.folded());
        const topology: geometry.Topology = .{ .ctx = &self.topo, .node_of = nodeOfCpu };
        self.drain_groups = try geometry.drainGroupPlan(gpa, self.plan.nqueues, qid_is_cpu, topology, geometry.forcedWidth(self.mode, geometry.drainGroupWidthOverride(env)));
        errdefer self.drain_groups.deinit(gpa);

        // 4. The reply word; the filesystem's init hook sees the negotiation.
        const asked_timeout_s = geometry.resolveRequestTimeoutS(geometry.requestTimeoutOverride(env), cfg.request_timeout_s);
        const wants: codec.InitWants = .{ .writeback_cache = cfg.opts.write_back and !cfg.opts.read_only, .request_timeout_s = asked_timeout_s, .atomic_o_trunc = !cfg.no_open };
        self.init_reply = codec.buildInitReply(init_in, self.plan, wants) catch {
            log.err("transport: unsupported FUSE protocol {d}.{d} (need 7.{d}+)", .{ init_in.major, init_in.minor, codec.MIN_SUPPORTED_MINOR });
            self.sideband.replyInline(codec.replyError(self.init_unique, .INVAL));
            return error.InitUnsupported;
        };
        geometry.gauges.publish(self.plan, init_in.max_readahead);
        geometry.gauges.publishDrainGroups(&self.drain_groups);
        self.time_limits = probes.timeLimitsVerdict(init_in.folded());
        time_limits_gauge.set(@intFromBool(self.init_reply.timeLimits()));
        self.request_timeout_s = if (self.init_reply.requestTimeoutS() != 0) geometry.kernelRequestTimeoutS(self.init_reply.requestTimeoutS(), geometry.readMaxRequestTimeout(self.io)) else 0;
        request_timeout_gauge.set(self.request_timeout_s);
        if (asked_timeout_s != 0) self.warnRequestTimeout(asked_timeout_s, init_in.folded());
        const read_zc_serve = knobs.boolKnob("SQUEEZEFS_READ_ZC_SERVE", env) and self.mode == .zero_copy;
        self.read_zc_serve = read_zc_serve;

        // 5. Write the reply classically (the kernel refuses REGISTER until
        //    `fch->initialized`), then the RETENTION probe: after the reply,
        //    before any REGISTER (its verdict composes the REGISTER flags),
        //    only on a zero-copy session.
        {
            var out: codec.OutBuf = undefined;
            const r = codec.replyInit(&out, self.init_unique, self.init_reply);
            var frame: [codec.INIT_REPLY_LEN]u8 = undefined;
            frame[0..codec.OUT_HEADER_LEN].* = abi.encode(r.header);
            @memcpy(frame[codec.OUT_HEADER_LEN..], r.out);
            try self.sideband.writeOnce(&frame);
        }
        if (self.mode == .zero_copy) self.retention = probes.driver.probeRetention(self.fuse_fd);
        // The FEATURES probe (series patch 0030), after the reply like the
        // retention probe: it decides how many rings each queue gets.
        self.rings_feature = probes.ringsPerQueueVerdict(probes.driver.probeFeatures(self.fuse_fd));
        try self.resolveRingsPerQueue(env);
        probes.publish(self.verdicts, self.retention, self.rings_feature, self.time_limits, probes.driver.capSysAdmin(self.io, env), self.mode);
        geometry.gauges.publishRingsPerQueue(self.rings_per_queue);
        fs.init(.{ .negotiated = self.init_reply.negotiated, .minor = self.init_reply.minor(), .plan = self.plan, .io = self.io, .buffer_mode = self.mode, .read_zc_serve = read_zc_serve, .retention = self.retention == .present });
        errdefer {
            self.failStartup();
            self.destroyOnce();
        }
        log.info("transport: INIT 7.{d} → 7.{d}: flags 0x{x} (over_io_uring={}, time_limits={}, request_timeout {d} s), max_write {d}, max_pages {d}, max_background {d}, queues {d} × depth {d}, payload {d} B, {d} drain group(s) (width {d}) × {d} ring(s) per queue, buffer mode {t} (kmbuf {?d}, retention {t}, rings per queue {t})", .{ init_in.minor, self.init_reply.minor(), self.init_reply.negotiated, self.init_reply.has(abi.FUSE_OVER_IO_URING), self.init_reply.timeLimits(), self.request_timeout_s, self.plan.max_write, self.plan.max_pages, self.plan.max_background, self.plan.nqueues, self.plan.depth, self.plan.payload_sz, self.drain_groups.count(), self.drain_groups.maxWidth(), self.rings_per_queue, self.mode, if (self.verdicts.kmbuf.opcodes()) |p| p.register else null, self.retention, self.rings_feature });
        if (knobs.boolKnob("SQUEEZEFS_TEST_FUSE_FAIL_AFTER_INIT_REPLY", env)) {
            log.err("transport: SQUEEZEFS_TEST_FUSE_FAIL_AFTER_INIT_REPLY — the bring-up fails after the INIT reply, before any queue registers", .{});
            return error.InjectedStartupFailure;
        }

        // 6. The zc bounce slab (one memfd per queue; a non-sheddable
        //    budget component), then the workers: one per drain group, the
        //    arena as ONE budget component, the arm barrier counting
        //    submissions.
        if (self.mode == .zero_copy) {
            self.slab = worker.Slab.init(self.gpa, self.plan.nqueues, self.plan.depth, self.plan.payload_sz) catch return error.SlabFailed;
            zc_bounce_slab_bytes.set(self.slab.?.bytes());
            // Non-sheddable (a populated window is a delivery in flight or
            // its body); floor 0 — the slab is lazily populated and the idle
            // windows return under the idle-trim law (the groups punch them).
            self.slab_component = mem_budget.Component.init("transport_zc_bounce", 0, 0, .{ .gauge = &worker.zc_bounce_resident_bytes }, null).withTrim(.{ .ctx = self, .call = &trimBounceHook });
            if (cfg.budget) |b| b.register(self.slab_component);
        }
        errdefer if (self.slab) |*s| {
            s.deinit();
            zc_bounce_slab_bytes.set(0);
        };
        const read_fusion_lever = knobs.boolKnob("SQUEEZEFS_FUSE_READ_FUSION", env);
        self.shared = .{
            .fuse_fd = self.fuse_fd,
            .plan = self.plan,
            .fs = fs,
            .exec = exec_io,
            .handler_group = &self.handler_group,
            .tx = &self.tx,
            .nqueues = self.plan.nqueues,
            .qid_is_cpu = qid_is_cpu,
            // Arm (b) exists only on a zero-copy session; on the lattice's
            // other rungs the lever is inert.
            .read_fusion_on = read_fusion_lever and self.mode == .zero_copy,
            .sqpoll = knobs.boolKnob("SQUEEZEFS_FUSE_SQPOLL", env),
            .node_of = nodeOfCpu,
            .node_ctx = &self.topo,
            .mode = self.mode,
            .kmbuf_opcodes = self.verdicts.kmbuf.opcodes(),
            .retention = self.retention == .present,
            .page = facts.page_size,
            .block_size = cfg.block_size,
            .io = self.io,
            .slab = if (self.slab) |*s| s else null,
            .test_error_logs = cfg.test_error_logs,
            .handlers_on_worker = knobs.boolKnob("SQUEEZEFS_FUSE_HANDLERS_ON_WORKER", env),
            .owed_spin = knobs.boolKnob("SQUEEZEFS_FUSE_OWED_SPIN", env),
            .rings_per_queue = self.rings_per_queue,
            .write_own_depth = knobs.intKnob("SQUEEZEFS_FUSE_WRITE_OWN_DEPTH", u32, env),
            .stream_lanes = streamLanesKnob(env),
            .no_open = cfg.no_open,
            .budget_bytes = cfg.mem_budget_bytes,
        };
        if (self.shared.stream_lanes != .cpu) log.info("transport: SQUEEZEFS_FUSE_STREAM_LANES={t} — the streaming write's lane placement (the measured A/B arm; `cpu` is the shipped one)", .{self.shared.stream_lanes});
        if (read_fusion_lever and !self.shared.read_fusion_on) log.info("transport: SQUEEZEFS_FUSE_READ_FUSION=1 accepted; inert on a {t} session (the fused poll is the zero-copy mode's)", .{self.mode});
        if (self.shared.sqpoll) log.info("transport: SQUEEZEFS_FUSE_SQPOLL=1 — the kernel submission poller (measured not recommended: the A/B control arm)", .{});
        self.shared.register_refuse_remaining.store(knobs.intKnob("SQUEEZEFS_TEST_FUSE_REGISTER_REFUSE", u32, env), .monotonic);
        self.shared.test_kmbuf_enomem = knobs.boolKnob("SQUEEZEFS_TEST_KMBUF_REGISTER_ENOMEM", env);
        // The registry admits exactly the enum's spellings (the startup gate refused the rest).
        self.shared.test_read_zero_fill = std.meta.stringToEnum(worker.ReadZeroFill, knobs.enumKnob("SQUEEZEFS_TEST_READ_ZERO_FILL", env)).?;
        if (self.shared.test_read_zero_fill != .off) log.warn("transport: SQUEEZEFS_TEST_READ_ZERO_FILL={t} — every O_DIRECT READ is answered on the worker with no filesystem (the FUSE boundary ruler); the reader gets zeros or stale bytes", .{self.shared.test_read_zero_fill});
        self.armBridgeSeams(env);
        errdefer self.closeStallPipe();

        // One group (worker, ring) per drain group PER RING: a queue's rings
        // sit at consecutive group indices from `qid_group[qid]`, each
        // taking its `ringShare` of the depth.
        const rings = self.rings_per_queue;
        const ngroups = self.drain_groups.count() * rings;
        self.groups = try gpa.alloc(*worker.Group, ngroups);
        errdefer gpa.free(self.groups);
        self.qid_group = try gpa.alloc(u16, self.plan.nqueues);
        errdefer gpa.free(self.qid_group);
        self.qid_member = try gpa.alloc(u16, self.plan.nqueues);
        errdefer gpa.free(self.qid_member);
        const pcfg = placement.Config.fromEnv(env);
        var built: usize = 0;
        errdefer for (self.groups[0..built]) |g| g.deinit();
        var total_arena: u64 = 0;
        var kmbuf_pinned: u64 = 0;
        for (0..self.drain_groups.count()) |di| {
            const qids = self.drain_groups.group(di);
            for (qids, 0..) |q, mi| {
                self.qid_group[q] = @intCast(di * rings);
                self.qid_member[q] = @intCast(mi);
            }
            const affinity: ?placement.CpuSet = if (pcfg.numa and qid_is_cpu) placement.scopedAffinityCpus(pcfg.scope, qids[0], self.topo.mask, &self.topo) else null;
            for (0..rings) |ring| {
                const gi = di * rings + ring;
                const share = geometry.ringShare(self.plan.depth, rings, @intCast(ring));
                self.groups[gi] = worker.Group.init(gpa, &self.shared, @intCast(gi), qids, share, affinity) catch |e| switch (e) {
                    error.OutOfMemory => return error.OutOfMemory,
                    error.ArenaMapFailed => return error.ArenaMapFailed,
                };
                built += 1;
                total_arena += self.groups[gi].arenaBytes();
                kmbuf_pinned += @as(u64, qids.len) * zc.ringEntriesFor(share.depth) * derive.alignUp(self.plan.payload_sz, facts.page_size);
            }
        }
        arena_bytes.set(total_arena);
        // Non-sheddable: the kernel writes into these pages at delivery
        // (the arena on UserEnts; the kmbuf regions on the other rungs).
        self.arena_component = mem_budget.Component.init("transport_payload_arena", total_arena, 0, .{ .gauge = &arena_bytes }, null);
        if (cfg.budget) |b| b.register(self.arena_component);
        if (self.mode != .user_ents) {
            self.kmbuf_component = mem_budget.Component.init("transport_kmbuf_region", kmbuf_pinned, 0, .{ .gauge = &kmbuf.pinned_bytes }, null);
            if (cfg.budget) |b| b.register(self.kmbuf_component);
        }

        var started: usize = 0;
        errdefer {
            self.failStartup();
            self.shutdown();
            for (self.groups[0..started]) |g| g.join();
        }
        for (self.groups) |g| {
            g.start() catch |e| {
                log.err("transport: group {d} failed to start ({s})", .{ g.index, @errorName(e) });
                return switch (e) {
                    error.RingSetupFailed => error.RingSetupFailed,
                    error.ThreadSpawnFailed => error.ThreadSpawnFailed,
                    error.KmbufRingEnomem => {
                        if (cfg.refusal) |r| {
                            const buf_size: u32 = @intCast(derive.alignUp(self.plan.payload_sz, facts.page_size));
                            const buffers = zc.ringEntriesFor(self.plan.depth);
                            r.* = .{ .kmbuf_ring_enomem = .{ .pages = kmbuf.regionPages(buffers, buf_size, facts.page_size), .buffers = buffers, .buf_size = buf_size, .memlock_charged = !probes.driver.capIpcLock(self.io) } };
                        }
                        return error.KmbufRingEnomem;
                    },
                };
            };
            started += 1;
        }
        // 7. The arm barrier: every queue's REGISTERs PUSHED (never completed).
        const verdict = self.shared.barrier.waitTimeout(ARM_BARRIER_NS) orelse {
            log.err("transport: arm barrier: {d} of {d} queue rings registered within {d} s — the mount fails", .{ self.shared.queues_registered.load(.acquire), self.plan.nqueues * self.rings_per_queue, ARM_BARRIER_NS / std.time.ns_per_s });
            return error.ArmBarrierFailed;
        };
        if (verdict == .failed) return error.EveryEntRetired;

        // 8. Ready: the sideband loop takes the classical connection forever.
        try self.sideband.start();
        self.ready.store(true, .release);
        sessions_active.add(1);
        self.publishKernelMaxBackground();
        log.info("transport: FUSE-over-io_uring transport armed at {s}: {d} queue(s) × {d} ent(s) over {d} worker(s) ({d} ring(s) per queue), {t}", .{ self.mountpoint, self.plan.nqueues, self.plan.depth, ngroups, self.rings_per_queue, self.mode });
        return self;
    }

    /// The watchdog's worker census (`watchdog.CensusHook`): after the wedge
    /// census line, every drain worker that holds fibers, owes a reply, has
    /// work queued or stands inside a pass — its pass age, its lane's live
    /// fibers, ready depth and inbox, the lane's wake coalescer and whether
    /// this ring's READ of the lane's eventfd stands. Atomics only (any
    /// thread). Bounded to one screen of suspects: the census is read by a
    /// person, and a wedge names one or two.
    const census_lines_max: u32 = 16;

    pub fn logWorkerCensus(ctx: *anyopaque) void {
        const self: *Session = @ptrCast(@alignCast(ctx));
        const now = clock.monotonicNs();
        var named: u32 = 0;
        for (self.groups) |g| {
            const v = g.censusView(now);
            if (!worker.Group.censusSuspect(v)) continue;
            log.err("worker census: group {d} (qid {d}, slots {d}..): pass {s} {d} ms ago, owed {d}; lane live {d} ready {d} inbox {d}, coalescer {s}, lane-wake READ {s}", .{ g.index, v.qid, v.ring_base, if (v.in_pass) "began" else "parked", v.pass_age_ns / std.time.ns_per_ms, v.owed, v.lane_live, v.lane_ready, v.lane_inbox, if (v.lane_coalescer_armed) "armed" else "disarmed", if (v.lane_wake_armed) "standing" else "DOWN" });
            named += 1;
            if (named == census_lines_max) break;
        }
        if (named == 0) log.err("worker census: no drain worker holds a fiber, owes a reply or stands inside a pass", .{});
    }

    pub fn censusHook(self: *Session) watchdog.CensusHook {
        return .{ .ctx = self, .call = &logWorkerCensus };
    }

    /// The probes and the lattice decision (ONCE per mount): kmbuf by the
    /// opcode ladder on a scratch ring, zc by `CapEff` + the sparse table,
    /// the lever from the registry; an explicit rung the probes refuse fails
    /// the mount loud naming the probe (the only degrade point is the probe).
    fn resolveMode(self: *Session, env: *const std.process.Environ.Map, offered: u64) MountError!void {
        const page: u32 = @intCast(self.facts.page_size);
        const kmbuf_v: probes.KmbufVerdict = if (knobs.boolKnob("SQUEEZEFS_TEST_KMBUF_ABSENT", env)) .absent else probes.driver.probeKmbuf(page);
        const cap = probes.driver.capSysAdmin(self.io, env);
        // The ring the reply will create (the parameter after our write,
        // `probes.ringAtReply`); the sparse table is the zc slot shape
        // (depth + the headers slot).
        const ring_at_reply = probes.ringAtReply(offered, readEnableUring(self.io));
        const sparse_ok = ring_at_reply and probes.driver.probeSparse(self.plan.depth + 1);
        self.verdicts = .{ .kmbuf = kmbuf_v, .zc = probes.zcVerdict(kmbuf_v, cap, sparse_ok) };
        const lever = probes.leverFromKnob(knobs.enumKnob("SQUEEZEFS_FUSE_BUFFER_MODE", env));
        self.mode = probes.resolveBufferMode(self.verdicts, lever) catch |e| {
            log.err("transport: SQUEEZEFS_FUSE_BUFFER_MODE={t} refused by the {s} probe (kmbuf {t}, CAP_SYS_ADMIN {}, sparse table {}) — the mount fails: the probe is the only degrade point (charter §3)", .{ lever, switch (e) {
                error.KmbufAbsent => "kmbuf",
                error.ZeroCopyAbsent => "zero-copy",
            }, self.verdicts.kmbuf, cap, sparse_ok });
            return error.BufferModeRefused;
        };
        if (lever != .auto) log.info("transport: SQUEEZEFS_FUSE_BUFFER_MODE={t} — the A/B pin (auto would take {t})", .{ lever, probes.resolveBufferMode(self.verdicts, .auto) catch unreachable });
    }

    /// `SQUEEZEFS_FUSE_RINGS_PER_QUEUE` over the derivation: an explicit
    /// count above what the FEATURES probe admits fails the mount loud
    /// (the probe is the only degrade point, charter §3).
    fn resolveRingsPerQueue(self: *Session, env: *const std.process.Environ.Map) MountError!void {
        const feature = self.rings_feature == .present;
        const derived = geometry.ringsPerQueue(self.mode, feature);
        const explicit = knobs.optIntKnob("SQUEEZEFS_FUSE_RINGS_PER_QUEUE", u32, env);
        if (explicit) |n| if (n > 1 and !feature) {
            log.err("transport: SQUEEZEFS_FUSE_RINGS_PER_QUEUE={d} refused by the FEATURES probe (FUSE_URING_FEAT_RINGS_PER_QUEUE absent: the kernel lacks series patch 0030, so a second ring on a queue would have its buffers recycled into the first's) — the mount fails: the probe is the only degrade point (charter §3)", .{n});
            return error.RingsPerQueueRefused;
        };
        if (explicit) |n| if (n > 1 and self.mode == .user_ents) {
            log.info("transport: SQUEEZEFS_FUSE_RINGS_PER_QUEUE={d} on a UserEnts session — a ring there serves a drain group of queues; the count is the derivation's 1", .{n});
            self.rings_per_queue = 1;
            return;
        };
        self.rings_per_queue = geometry.resolveRingsPerQueue(explicit, derived);
        if (explicit != null and self.rings_per_queue != derived) log.info("transport: SQUEEZEFS_FUSE_RINGS_PER_QUEUE={d} — the A/B pin (derived {d})", .{ self.rings_per_queue, derived });
    }

    /// `SQUEEZEFS_TEST_ZC_BRIDGE_STALL` / `_DEADLINE_MS`: a pipe with no
    /// writer stands in for the bridge source, and a short deadline.
    /// The idle-trim hook (`mem_budget.TrimFn`): every drain group punches
    /// its idle bounce windows in its next pass.
    fn trimBounceHook(ctx: *anyopaque, min_idle_ticks: u64) void {
        const self: *Session = @ptrCast(@alignCast(ctx));
        for (self.groups) |g| g.requestBounceTrim(min_idle_ticks);
    }

    fn armBridgeSeams(self: *Session, env: *const std.process.Environ.Map) void {
        const stall = knobs.intKnob("SQUEEZEFS_TEST_ZC_BRIDGE_STALL", u32, env);
        const deadline_ms = knobs.intKnob("SQUEEZEFS_TEST_ZC_BRIDGE_DEADLINE_MS", u64, env);
        if (deadline_ms != 0) self.shared.bridge_deadline_ns = deadline_ms * std.time.ns_per_ms;
        const ceiling = knobs.intKnob("SQUEEZEFS_TEST_ZC_BRIDGE_CEILING", u32, env);
        if (ceiling != 0) self.shared.test_bridge_ceiling = ceiling;
        if (stall == 0) return;
        var fds: [2]linux.fd_t = undefined;
        if (linux.errno(linux.pipe2(&fds, .{ .CLOEXEC = true })) != .SUCCESS) return;
        self.shared.stall_pipe = fds;
        self.shared.bridge_stall_remaining.store(stall, .monotonic);
        log.warn("transport: SQUEEZEFS_TEST_ZC_BRIDGE_STALL={d} — the next {d} reply bridge(s) read a writer-less pipe (deadline {d} ms)", .{ stall, stall, self.shared.bridge_deadline_ns / std.time.ns_per_ms });
    }

    fn closeStallPipe(self: *Session) void {
        for (&self.shared.stall_pipe) |*fd| if (fd.* >= 0) {
            _ = linux.close(fd.*);
            fd.* = -1;
        };
    }

    /// fusectl's `max_background` for this connection (the mountpoint's
    /// device minor names it); 0 when fusectl is not mounted.
    fn publishKernelMaxBackground(self: *Session) void {
        const minor = self.connectionMinor() orelse return;
        var path: [96]u8 = undefined;
        const p = std.fmt.bufPrint(&path, "{s}/{d}/max_background", .{ fusectl_connections, minor }) catch return;
        var buf: [32]u8 = undefined;
        const text = Io.Dir.cwd().readFile(self.io, p, &buf) catch return;
        const v = std.fmt.parseInt(u64, std.mem.trim(u8, text, " \t\r\n"), 10) catch return;
        kernel_max_background.set(v);
    }

    pub fn isReady(self: *const Session) bool {
        return self.ready.load(.acquire);
    }

    /// Owed replies across every group (derived from the slot states).
    pub fn owedCount(self: *const Session) u32 {
        var n: u32 = 0;
        for (self.groups) |g| n += g.owedCount();
        return n;
    }

    /// Every queue holds ≥ 1 registered entry (the arm barrier's fact).
    /// Queues whose every ring has every REGISTER pushed (the barrier
    /// counts member rings; a queue with one of two rings pushed rounds
    /// down).
    pub fn queuesRegistered(self: *const Session) u32 {
        return self.shared.queues_registered.load(.acquire) / self.rings_per_queue;
    }

    fn destroyOnce(self: *Session) void {
        if (self.destroyed.swap(true, .acq_rel)) return;
        self.fs.destroy();
    }

    /// Idempotent: flip `active`, wake every worker and the sideband, fire
    /// the daemon's hook once.
    pub fn shutdown(self: *Session) void {
        if (!self.shared.shutdown()) return;
        if (self.ready.swap(false, .acq_rel)) sessions_active.sub(1);
        for (self.groups) |g| g.forceWake();
        if (self.have_sideband) self.sideband.requestStop();
        if (self.on_shutdown) |h| h.fire();
    }

    /// Whether the mountpoint is still attached (false once the kernel
    /// unmounted it or `close` detached it).
    pub fn isMounted(self: *const Session) bool {
        return self.mounted.load(.acquire);
    }

    /// The request timeout the kernel applies to this connection, seconds
    /// (0 = none).
    pub fn requestTimeoutS(self: *const Session) u16 {
        return self.request_timeout_s;
    }

    /// The kernel ended the connection while THIS mount is still in the
    /// table — the request timeout or a fusectl abort, never an unmount
    /// (an `umount2`, lazy or not, removes the entry before the superblock
    /// ends the connection): the mount is fail-stopped, every access
    /// ENOTCONN until it is unmounted. Reads the mount table (the caller's
    /// thread, after the session ended).
    pub fn endedUnderLiveMount(self: *Session) bool {
        // acquire pairs with the sideband's release.
        if (!self.lost_while_mounted.load(.acquire)) return false;
        const want = self.mount_id orelse return false;
        const e = self.mountEntry() orelse return false;
        return e.id == want;
    }

    /// The INIT's loud lines about the bound it asked for: a kernel without
    /// FUSE_REQUEST_TIMEOUT (no bound at all, FUSE_SYNCFS declined), or a
    /// host sysctl that caps it below the lattice's.
    fn warnRequestTimeout(self: *const Session, asked_s: u16, offered: u64) void {
        if (self.request_timeout_s == 0) {
            log.warn("transport: FUSE_REQUEST_TIMEOUT is not negotiated on this kernel — a stopped or wedged daemon holds its callers forever{s} (decision 0003 §Z)", .{if (offered & abi.FUSE_HAS_SYNCFS != 0) "; FUSE_SYNCFS declined (every host-wide sync would wait on it)" else ""});
            return;
        }
        if (self.request_timeout_s < asked_s) log.warn("transport: the host caps FUSE request timeouts ({s}) at {d} s, below this mount's {d} s (T_owner + T_park_max, decision 0003 §Z): a request a healthy daemon holds longer — parked on a peer the lattice is still giving up — ends the mount ENOTCONN", .{ geometry.max_request_timeout_path, self.request_timeout_s, asked_s });
    }

    // ---- teardown ----

    /// Phase 1: shutdown → the workers' drains RELEASE every retained slot
    /// (reaping the verdicts: a RELEASE after the unmount meets a stopped
    /// queue, and a retained ent the kernel reclaims pins the exit) and
    /// synthesize an error for every owed slot (the blocked callers return,
    /// so the mount is not busy) → `umount2` → the disconnect ends the
    /// workers' residency and the sideband → join → the filesystem's
    /// `destroy` → close the fd. The one orderly path the drain cannot
    /// cover is an EXTERNAL `umount(8)`: a type-`fuse` mount gets no
    /// DESTROY, the kernel aborts the connection first.
    pub fn close(self: *Session) void {
        if (self.closed) return;
        self.closed = true;
        self.shutdown();
        if (!self.awaitDrains()) {
            // A group still owes a request (a FUSE_SYNCFS a host-wide sync
            // holds `s_umount` around, perhaps): the unmount must not wait
            // on it.
            log.warn("transport: a drain group did not drain within {d} ms — ending the connection before the unmount (decision 0003 §AN)", .{DRAIN_WAIT_NS / std.time.ns_per_ms});
            self.failStop();
        }
        self.unmount();
        for (self.groups) |g| g.join();
        if (self.have_sideband) self.sideband.join();
        self.destroyOnce();
        self.closeFd();
    }

    /// Wait until every started group's drain flushed its synthesized
    /// commits (each group's `drained` word), bounded: the drain itself is
    /// bounded by `DRAIN_LEASE_BUDGET` plus one enter. False when a group
    /// had not drained by the deadline.
    fn awaitDrains(self: *Session) bool {
        const deadline = clock.monotonicNs() + DRAIN_WAIT_NS;
        for (self.groups) |g| {
            if (g.started.load(.acquire) != 1) continue;
            while (g.drained.load(.acquire) == 0) {
                const now = clock.monotonicNs();
                if (now >= deadline) return false;
                _ = futex.waitTimeout(&g.drained, 0, deadline - now);
            }
        }
        return true;
    }

    /// A plain `umount2` first: the drain already answered every blocked
    /// caller, so the only EBUSY left is a caller still returning from its
    /// syscall or closing its fd — a bounded retry. The orderly stop
    /// (decision 0003 §S) runs the holder census BEFORE this teardown and
    /// refuses a busy mount while it still serves; an EBUSY that outlives
    /// the window here is a holder that arrived inside the drain's
    /// milliseconds, and the session is already drained — the lazy detach
    /// is the one remaining honest exit (loud; the straggler's next request
    /// answers ENOTCONN, the connection ends at this daemon's exit).
    fn unmount(self: *Session) void {
        if (!self.mounted.load(.acquire)) return;
        if (!self.stillOurs()) {
            log.info("transport: {s} is no longer this session's mount (detached externally, or a successor mounted there) — nothing to unmount", .{self.mountpoint});
            self.mounted.store(false, .release);
            return;
        }
        if (self.unmounter) |u| {
            // The unprivileged path: `fusermount3 -u` (bounded retries
            // inside); a failure leaves the mount for the operator — a
            // non-root process has no `MNT_DETACH`.
            if (!u.call(u.ctx, self.mountpoint, false)) log.err("transport: fusermount3 -u {s} failed — a mount is left behind", .{self.mountpoint});
            self.mounted.store(false, .release);
            return;
        }
        const deadline = clock.monotonicNs() + UMOUNT_RETRY_NS;
        while (true) {
            const rc = linux.umount2(self.mountpoint.ptr, 0);
            const e = linux.errno(rc);
            if (e == .SUCCESS) {
                self.mounted.store(false, .release);
                return;
            }
            if (e == .BUSY and clock.monotonicNs() < deadline) {
                umount_busy_retries.inc();
                std.Thread.yield() catch {};
                continue;
            }
            if (e == .BUSY) {
                log.warn("transport: umount2({s}) still EBUSY {d} s after the drain — a holder arrived after the stop's census; detaching lazily: its next request answers ENOTCONN and the connection ends at this daemon's exit", .{ self.mountpoint, UMOUNT_RETRY_NS / std.time.ns_per_s });
            } else {
                log.warn("transport: umount2({s}) failed ({t}); detaching lazily", .{ self.mountpoint, e });
            }
            self.unmountDetached(.teardown);
            return;
        }
    }

    /// The head of every bring-up failure path once the mount exists: the
    /// test hold, then the fail-stop — before any teardown step.
    fn failStartup(self: *Session) void {
        if (self.fail_stopped) return;
        if (self.mounted.load(.acquire)) self.holdFailurePath();
        self.failStop();
    }

    /// THE FAIL-STOP ORDER (decision 0003 §AN): a failure path ends its own
    /// connection BEFORE any step that can wait — a worker join, the
    /// filesystem's destroy, an `umount2`. A detach's last reference
    /// deactivates the superblock in the caller's task with `s_umount` held
    /// for write, while a host-wide `sync` holds it for read around its
    /// FUSE_SYNCFS; until every queue is registered that request waits in
    /// `fuse_get_req`, where neither the request timeout (it times queued
    /// requests) nor any reply of ours reaches it — only the connection's
    /// end wakes it. fusectl's `abort` when it is mounted (the mount's
    /// owner may write it); otherwise `unmountDetached` carries the abort
    /// into the detach (`MNT_FORCE`) or closes the device first. Once.
    fn failStop(self: *Session) void {
        if (self.fail_stopped) return;
        self.fail_stopped = true;
        if (!self.mounted.load(.acquire)) return;
        // release pairs with the sideband's acquire at the disconnect.
        self.self_aborted.store(true, .release);
        const minor = self.connectionMinor() orelse return;
        self.abort_written = self.abortConnection(minor);
    }

    /// `SQUEEZEFS_TEST_FUSE_STARTUP_FAIL_HOLD`: the failure path waits at
    /// its head until a writer opens the FIFO — the window a row needs to
    /// park a host-wide sync on this connection before any teardown step.
    fn holdFailurePath(self: *Session) void {
        const path = knobs.parseStr(self.env.get("SQUEEZEFS_TEST_FUSE_STARTUP_FAIL_HOLD")) orelse return;
        var pb: [linux.PATH_MAX]u8 = undefined;
        const p = std.fmt.bufPrintSentinel(&pb, "{s}", .{path}, 0) catch return;
        log.warn("transport: SQUEEZEFS_TEST_FUSE_STARTUP_FAIL_HOLD — the bring-up failed after mount(2); its failure path holds until a writer opens {s}", .{path});
        const rc = linux.open(p.ptr, .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
        if (linux.errno(rc) != .SUCCESS) {
            log.warn("transport: the failure-path hold could not open {s} ({t}); not holding", .{ path, linux.errno(rc) });
            return;
        }
        _ = linux.close(@intCast(rc));
    }

    /// The lazy detach, after the fail-stop: a detached superblock lives on
    /// behind any open fd and the uring queues hold the connection's devs,
    /// so only the connection's end frees a holder (a later FLUSH on a
    /// detached, live connection would wait forever) — and the detach's
    /// last reference deactivates the superblock in THIS task, under
    /// `s_umount`, which a waiter parked on the live connection holds.
    fn unmountDetached(self: *Session, phase: enum { startup, teardown }) void {
        if (!self.mounted.load(.acquire)) return;
        self.failStop();
        if (self.unmounter) |u| {
            // No fusectl abort: on the bring-up's failure path the workers
            // and the sideband are gone, so our device fd is the
            // connection's last reference once their rings exit — its close
            // ends it (`fusermount3` has no force).
            if (phase == .startup and !self.abort_written) self.closeFd();
            if (!u.call(u.ctx, self.mountpoint, true)) log.err("transport: fusermount3 -u -z {s} failed — a mount is left behind", .{self.mountpoint});
            self.mounted.store(false, .release);
            return;
        }
        // Without the fusectl abort, MNT_FORCE carries it into the detach
        // (`umount_begin` aborts before the last reference drops); it needs
        // CAP_SYS_ADMIN in the initial namespace — EPERM detaches plainly.
        const flags: u32 = if (self.abort_written) linux.MNT.DETACH else linux.MNT.DETACH | linux.MNT.FORCE;
        var rc = linux.umount2(self.mountpoint.ptr, flags);
        if (linux.errno(rc) == .PERM and flags != linux.MNT.DETACH) {
            rc = linux.umount2(self.mountpoint.ptr, linux.MNT.DETACH);
        } else if (flags != linux.MNT.DETACH and linux.errno(rc) == .SUCCESS) {
            log.warn("transport: ended its own FUSE connection at {s} (MNT_FORCE: no fusectl; `umount_begin` aborts it before the detach drops its last reference — decision 0003 §AN)", .{self.mountpoint});
        }
        if (linux.errno(rc) != .SUCCESS) log.err("transport: umount2({s}, MNT_DETACH) failed ({t}) — a mount is left behind", .{ self.mountpoint, linux.errno(rc) });
        self.mounted.store(false, .release);
        // The workers' residency has nothing to wait for on a detached
        // connection no holder will end (release pairs with the residency
        // loop's acquire); the bring-up's failure path has no workers.
        if (phase == .teardown) self.shared.detached.store(true, .release);
    }

    /// `squeezefs umount --lazy` (decision 0003 §S shape b): detach the
    /// mountpoint NOW — `MNT_DETACH`, `fusermount3 -u -z` unprivileged —
    /// and keep serving every request on the detached superblock until the
    /// kernel ends the connection (the last holder's reference drops → the
    /// sideband's read answers the disconnect → `shutdown` → the daemon's
    /// teardown). False when nothing is attached (already detached or the
    /// detach refused); the caller logs either way.
    pub fn detachLazily(self: *Session) bool {
        if (!self.mounted.load(.acquire)) return false;
        if (!self.stillOurs()) {
            self.mounted.store(false, .release);
            return false;
        }
        if (self.unmounter) |u| {
            const ok = u.call(u.ctx, self.mountpoint, true);
            if (ok) self.mounted.store(false, .release);
            return ok;
        }
        const rc = linux.umount2(self.mountpoint.ptr, linux.MNT.DETACH);
        if (linux.errno(rc) != .SUCCESS) {
            log.err("transport: umount2({s}, MNT_DETACH) failed ({t}) — the mount stays attached", .{ self.mountpoint, linux.errno(rc) });
            return false;
        }
        self.mounted.store(false, .release);
        return true;
    }

    const MountIdentity = struct { id: u64, dev_minor: u32 };

    /// The mount table's entry at our mountpoint (the last line wins), by
    /// value: the table's buffer does not outlive the call.
    fn mountEntry(self: *Session) ?MountIdentity {
        const buf = self.gpa.alloc(u8, mountinfo.READ_BYTES) catch return null;
        defer self.gpa.free(buf);
        const text = mountinfo.read(self.io, buf);
        const e = mountinfo.entryAt(text, self.mountpoint) orelse return null;
        return .{ .id = e.id, .dev_minor = e.dev_minor };
    }

    /// Is the mount at our path still the one we made? An unreadable table
    /// (no procfs) answers yes: the path is then the only fact there is.
    fn stillOurs(self: *Session) bool {
        const want = self.mount_id orelse return true;
        const e = self.mountEntry() orelse return false;
        return e.id == want;
    }

    /// The mountpoint's device minor = the fusectl connection id, read from
    /// `/proc/self/mountinfo` — NEVER a stat of the mountpoint: that is a
    /// GETATTR to ourselves, which blocks forever on a connection whose INIT
    /// reply was never written (the refused-mode path) and is a request to
    /// our own mount on every other.
    fn connectionMinor(self: *Session) ?u32 {
        const e = self.mountEntry() orelse return null;
        return e.dev_minor;
    }

    /// `/sys/fs/fuse/connections/<minor>/abort` ← 1 (a control write through
    /// the raw syscalls, like `enable_uring`); false when fusectl is not
    /// mounted or refuses (`unmountDetached` then carries the abort).
    fn abortConnection(self: *Session, minor: u32) bool {
        var path: [96]u8 = undefined;
        const p = std.fmt.bufPrintSentinel(&path, "{s}/{d}/abort", .{ fusectl_connections, minor }, 0) catch return false;
        const fd_rc = linux.open(p.ptr, .{ .ACCMODE = .WRONLY, .CLOEXEC = true }, 0);
        if (linux.errno(fd_rc) != .SUCCESS) return false;
        const fd: linux.fd_t = @intCast(fd_rc);
        defer _ = linux.close(fd);
        if (linux.errno(linux.write(fd, "1", 1)) != .SUCCESS) return false;
        log.warn("transport: ended its own FUSE connection {d} (fusectl abort) before the unmount of {s} — nothing parked on it can hold the detach (decision 0003 §AN)", .{ minor, self.mountpoint });
        return true;
    }

    fn closeFd(self: *Session) void {
        if (self.fuse_fd >= 0) _ = linux.close(self.fuse_fd);
        self.fuse_fd = -1;
    }

    /// Phase 2 (after `close`): wait for every handler fiber (so no lease
    /// can outlive its arena), then free everything.
    pub fn deinit(self: *Session) void {
        self.close();
        self.handler_group.await(self.io) catch {};
        for (self.groups) |g| g.deinit();
        if (self.slab) |*s| {
            s.deinit();
            self.slab = null;
            zc_bounce_slab_bytes.set(0);
        }
        self.closeStallPipe();
        self.gpa.free(self.groups);
        self.gpa.free(self.qid_group);
        self.gpa.free(self.qid_member);
        self.drain_groups.deinit(self.gpa);
        if (self.have_sideband) self.sideband.deinit(self.gpa);
        self.gpa.free(self.source);
        self.gpa.free(self.mountpoint);
        self.gpa.destroy(self);
    }

    // ---- stats ----

    /// Every `transport_*` family as one JSON object (the `.stats` inode's
    /// rung-5 writer concatenates families).
    pub fn writeStatsJson(self: *Session, w: *Io.Writer) Io.Writer.Error!void {
        try w.writeAll("{");
        const g = &geometry.gauges;
        try field(w, "transport_queues", g.queues.load());
        try field(w, "transport_q_depth", g.q_depth.load());
        try field(w, "transport_payload_buffer_bytes", g.payload_buffer_bytes.load());
        try field(w, "transport_payload_buffers", g.payload_buffers.load());
        try field(w, "transport_max_background", g.max_background.load());
        try field(w, "transport_max_write", g.max_write.load());
        try field(w, "transport_max_pages", g.max_pages.load());
        try field(w, "transport_max_readahead", g.max_readahead.load());
        try field(w, "transport_drain_groups", g.drain_groups.load());
        try field(w, "transport_drain_group_width", g.drain_group_width.load());
        try field(w, "transport_arena_bytes", arena_bytes.load());
        try field(w, "transport_buffer_mode", probes.mode_gauge.load());
        try field(w, "transport_probe_kmbuf", probes.kmbuf_gauge.load());
        try field(w, "transport_probe_zc", probes.zc_gauge.load());
        try field(w, "transport_probe_retention", probes.retention_gauge.load());
        try field(w, "transport_probe_rings_per_queue", probes.rings_per_queue_gauge.load());
        try field(w, "fuse_uring_rings_per_queue", g.rings_per_queue.load());
        try field(w, "transport_probe_time_limits", probes.time_limits_gauge.load());
        try field(w, "transport_probe_cap_sys_admin", probes.cap_gauge.load());
        try field(w, "transport_time_limits", time_limits_gauge.load());
        try field(w, "transport_request_timeout_s", request_timeout_gauge.load());
        try field(w, "transport_kernel_max_background", kernel_max_background.load());
        try field(w, "transport_kmbuf_pinned_bytes", kmbuf.pinned_bytes.load());
        try field(w, "transport_kmbuf_register_retries", kmbuf.register_retries.load());
        try field(w, "transport_kmbuf_attached_deliveries", worker.kmbuf_attached_deliveries.load());
        try field(w, "transport_kmbuf_attachment_breach", worker.kmbuf_attachment_breach.load());
        try field(w, "transport_zc_bounce_bytes", worker.zc_bounce_resident_bytes.load());
        try field(w, "transport_zc_bounce_reserve_bytes", zc_bounce_slab_bytes.load());
        try field(w, "transport_zc_bounce_trimmed_bytes", worker.zc_bounce_trimmed_bytes.load());
        try field(w, "transport_zc_replies", worker.zc_replies.load());
        try field(w, "transport_zc_serve_bytes", worker.zc_serve_bytes.load());
        try field(w, "transport_zc_bounce_bytes_bridged", worker.zc_bounce_bytes.load());
        try field(w, "transport_zc_bounce_copies", worker.zc_bounce_copies.load());
        try field(w, "transport_zc_fd_body_replies", worker.zc_fd_body_replies.load());
        try field(w, "transport_zc_fd_body_fallbacks", worker.zc_fd_body_fallbacks.load());
        try field(w, "transport_zc_direct_legs", worker.zc_direct_legs.load());
        try field(w, "transport_zc_direct_leg_fallbacks", worker.zc_direct_leg_fallbacks.load());
        try field(w, "transport_zc_direct_leg_recheck_withdrawn", worker.zc_direct_leg_recheck_withdrawn.load());
        try field(w, "transport_zc_direct_leg_dma_bytes", worker.zc_direct_leg_dma_bytes.load());
        try field(w, "transport_zc_direct_leg_serve_bytes", worker.zc_direct_leg_serve_bytes.load());
        try field(w, "transport_write_window_bytes", worker.write_window_bytes.load());
        try field(w, "transport_zc_write_directs", worker.zc_write_directs.load());
        try field(w, "transport_zc_write_direct_bytes", worker.zc_write_direct_bytes.load());
        try field(w, "transport_zc_write_direct_failures", worker.zc_write_direct_failures.load());
        try field(w, "transport_zc_write_direct_refusals", worker.zc_write_direct_refusals.load());
        try field(w, "transport_zc_fd_source_copies", worker.zc_fd_source_copies.load());
        try field(w, "transport_zc_fallbacks", worker.zc_fallbacks.load());
        try field(w, "transport_zc_readdir_latch", worker.zc_readdir_latch.load());
        try field(w, "transport_zc_bridges", worker.zc_bridges.load());
        try field(w, "transport_zc_bridge_splits", worker.zc_bridge_splits.load());
        try field(w, "transport_zc_bridge_pieces", worker.zc_bridge_pieces.load());
        try field(w, "transport_nowait_ceiling_probes", worker.nowait_ceiling_probes.load());
        try field(w, "transport_nowait_ceiling_table_full", worker.nowait_ceiling_table_full.load());
        try field(w, "transport_zc_bridge_pends", worker.zc_bridge_pends.load());
        try field(w, "transport_zc_bridge_cancels", worker.bridge_cancels.load());
        try field(w, "transport_zc_bridge_lost", worker.bridge_lost.load());
        try field(w, "transport_zc_bridge_orphans", worker.bridge_orphans.load());
        try field(w, "transport_zc_bridge_restamps", worker.bridge_restamps.load());
        try field(w, "transport_zc_scan_max_age_ms", worker.scan_max_age_ms.load());
        try field(w, "transport_zc_direction_refusals", slot.zc_direction_refusals.load());
        try field(w, "transport_zc_write_holds", slot.zc_write_holds.load());
        try field(w, "transport_zc_write_hold_bytes", slot.zc_write_hold_bytes.load());
        try w.writeAll("\"transport_zc_write_hold_ns\":");
        try slot.zc_write_hold_ns.snapshot().writeJson(w);
        try w.writeByte(',');
        try w.writeAll("\"transport_held_write_phase_ns\":{\"to_store\":");
        try worker.held_write_to_store_ns.snapshot().writeJson(w);
        try w.writeAll(",\"store\":");
        try worker.held_write_store_ns.snapshot().writeJson(w);
        try w.writeAll(",\"to_commit\":");
        try worker.held_write_to_commit_ns.snapshot().writeJson(w);
        try w.writeAll("},");
        try field(w, "transport_zc_write_extractions", slot.zc_write_extractions.load());
        try field(w, "transport_zc_write_extract_bytes", slot.zc_write_extract_bytes.load());
        try field(w, "transport_zc_write_lazy_extractions", slot.zc_write_lazy_extractions.load());
        try field(w, "transport_zc_write_lazy_extract_bytes", slot.zc_write_lazy_extract_bytes.load());
        try field(w, "transport_zc_retain_commits", slot.retain_commits.load());
        try field(w, "transport_zc_releases", slot.releases.load());
        try field(w, "transport_zc_retained_outstanding", slot.retained_outstanding.load());
        try field(w, "transport_zc_release_failures", slot.release_failures.load());
        try field(w, "transport_zc_retain_refused", slot.retain_refused.load());
        try field(w, "transport_zc_retain_refused_class", slot.retain_refused_class.load());
        try field(w, "transport_zc_release_inert", worker.zc_release_inert.load());
        try field(w, "transport_zc_releases_inflight", worker.releases_inflight.load());
        try field(w, "transport_retained_released_at_teardown", worker.retained_released_at_teardown.load());
        try field(w, "transport_retained_kernel_drained", worker.retained_kernel_drained.load());
        try field(w, "transport_releases_requested", releases_requested.load());
        try field(w, "transport_fused_read_late_demotions", worker.fused_read_late_demotions.load());
        try field(w, "transport_fused_polls", worker.fused_polls.load());
        try field(w, "transport_sessions_active", sessions_active.load());
        try field(w, "transport_queues_registered", self.queuesRegistered());
        try field(w, "transport_owed", self.owedCount());
        try field(w, "transport_ring_requests", worker.ring_requests.load());
        try field(w, "transport_ring_replies", worker.ring_replies.load());
        try field(w, "transport_ring_reads", worker.ring_reads.load());
        try field(w, "transport_registers", worker.registers.load());
        try field(w, "transport_register_refusals", slot.register_refusals.load());
        try field(w, "transport_ents_retired", slot.ents_retired.load());
        try field(w, "transport_cqe_errors", worker.cqe_errors.load());
        try field(w, "transport_forget_autocommits", worker.forget_autocommits.load());
        try field(w, "transport_meta_inline_serves", worker.meta_inline_serves.load());
        try field(w, "transport_flush_inline_serves", worker.flush_inline_serves.load());
        try field(w, "transport_flush_inline_demotes", worker.flush_inline_demotes.load());
        try field(w, "transport_meta_inline_demotes", worker.meta_inline_demotes.load());
        try field(w, "transport_fast_dispatch_serves", worker.fast_dispatch_serves.load());
        try field(w, "transport_fast_dispatch_demotes", worker.fast_dispatch_demotes.load());
        try field(w, "transport_fused_read_fusions", worker.fused_read_fusions.load());
        try field(w, "transport_fused_read_demotions", worker.fused_read_demotions.load());
        try field(w, "transport_dispatch_refused", worker.dispatch_refused.load());
        try field(w, "transport_handler_dispatches_own", worker.handler_dispatches_own.load());
        try field(w, "transport_handler_dispatches_foreign", worker.handler_dispatches_foreign.load());
        try field(w, "transport_write_spills", worker.write_spills.load());
        try field(w, "transport_stream_spreads", worker.stream_spreads.load());
        try field(w, "transport_lane_pumps", worker.lane_pumps.load());
        try field(w, "transport_payload_leases", slot.payload_leases.load());
        try field(w, "transport_dest_dma_leases", slot.dest_dma_leases.load());
        try field(w, "transport_leases_outstanding", slot.leases_outstanding.load());
        try field(w, "transport_lease_max_age_ms", slot.lease_max_age_ms.load(.monotonic));
        try field(w, "transport_lease_overlong", slot.lease_overlong.load());
        try field(w, "transport_parked_commits", slot.parked_commits.load());
        try field(w, "transport_unparked_commits", slot.unparked_commits.load());
        try field(w, "transport_wake_writes", slot.wake_writes.load());
        try field(w, "transport_wakes_elided", slot.wakes_elided.load());
        try field(w, "transport_requests_failed_synthetic", slot.requests_failed_synthetic.load());
        try field(w, "transport_requests_abandoned", slot.requests_abandoned.load());
        try field(w, "transport_requests_displaced", slot.requests_displaced.load());
        try field(w, "transport_replies_refused_stale", slot.replies_refused_stale.load());
        try field(w, "transport_replies_dropped_no_slot", replies_dropped_no_slot.load());
        try field(w, "transport_replies_oversize", slot.replies_oversize.load());
        try field(w, "transport_commit_cells_busy", commit_queue.cells_busy.load());
        try field(w, "transport_commit_queue_overflow", commit_queue.queue_overflow.load());
        try field(w, "transport_fd_body_fallbacks", commit_queue.fd_body_fallbacks.load());
        try field(w, "transport_cq_overflows", worker.cq_overflows.load());
        try field(w, "transport_cqe_tag_unknown", worker.cqe_tag_unknown.load());
        try field(w, "transport_delivery_on_leased_ent", worker.delivery_on_leased.load());
        try field(w, "transport_sq_full", worker.sq_full.load());
        try field(w, "transport_park_backstop_ticks", worker.park_backstop_ticks.load());
        try field(w, "transport_park_tick_commit_lost_wakes", worker.park_tick_commit_lost_wakes.load());
        try field(w, "transport_park_tick_late_commits", worker.park_tick_late_commits.load());
        try field(w, "transport_park_tick_cqe_lost_wakes", worker.park_tick_cqe_lost_wakes.load());
        try field(w, "transport_park_tick_late_cqes", worker.park_tick_late_cqes.load());
        try field(w, "transport_enters", worker.enters.load());
        try field(w, "transport_passes", worker.passes.load());
        try field(w, "transport_owed_spins", worker.owed_spins.load());
        try field(w, "transport_owed_spin_hits", worker.owed_spin_hits.load());
        try field(w, "transport_owed_spin_falls", worker.owed_spin_falls.load());
        try field(w, "transport_owed_spin_skips", worker.owed_spin_skips.load());
        try field(w, "transport_owed_spin_ns", worker.owed_spin_ns.load());
        // The rule's inputs on the ring that caught the most completions.
        var reading: worker.Group.SpinReading = .{};
        for (self.groups) |grp| {
            const r = grp.spinReading();
            if (r.caught > reading.caught) reading = r;
        }
        try field(w, "transport_owed_spin_dev_ns", reading.dev_ns);
        try field(w, "transport_owed_spin_wake_ns", reading.wake_ns);
        try field(w, "transport_residency_replies", worker.residency_replies.load());
        try field(w, "transport_umount_busy_retries", umount_busy_retries.load());
        try field(w, "transport_umount_busy_refusals", umount_busy_refusals.load());
        try field(w, "transport_commit_batch_flushes", worker.commit_batch_flushes.load());
        try field(w, "transport_commit_batch_commits", worker.commit_batch_commits.load());
        try w.writeAll("\"transport_commit_batch\":{");
        for (worker.commit_batch_labels, 0..) |label, i| {
            if (i > 0) try w.writeAll(",");
            try w.print("\"{s}\":{d}", .{ label, worker.commit_batch[i].load(.monotonic) });
        }
        try w.writeAll("},");
        try field(w, "transport_classical_sideband", sideband_deliveries.load());
        try field(w, "transport_sideband_forgets", sideband_forgets.load());
        try field(w, "transport_sideband_interrupts", sideband_interrupts.load());
        try field(w, "transport_sideband_requests", sideband_requests.load());
        try field(w, "transport_sideband_replies", sideband_replies.load());
        try field(w, "transport_sideband_cells_exhausted", sideband_cells_exhausted.load());
        try field(w, "transport_notify_sent", notify_sent.load());
        try field(w, "transport_notify_enoent", notify_enoent.load());
        try field(w, "transport_notify_failed", notify_failed.load());
        try field(w, "transport_notify_epochs_owed", notify_epochs_owed.load());
        try field(w, "transport_notify_epochs_sent", notify_epochs_sent.load());
        try field(w, "transport_notify_nb_waits", notify_nb_waits.load());
        try field(w, "transport_sideband_recycle_respins", sideband_recycle_respins.load());
        try field(w, "transport_notify_pending", notify_pending.load());
        try w.print("\"transport_notify_dropped\":{d}", .{notify_dropped.load()});
        try w.writeAll("}");
    }

    fn field(w: *Io.Writer, name: []const u8, v: u64) Io.Writer.Error!void {
        try w.print("\"{s}\":{d},", .{ name, v });
    }
};

/// Read `enable_uring`; write `Y` when it does not read enabled. A one-shot
/// control write through the raw syscalls (sysfs refuses `O_TRUNC` opens
/// that a generic file writer would make; charter §1.2's sanctioned
/// control-path exception, not an I/O path). A refusal only warns: the
/// REGISTER's verdict is the loud failure.
pub fn ensureKernelUringEnabled(io: Io) void {
    if (readEnableUring(io)) |on| if (on) return;
    const fd_rc = linux.open(enable_uring_path, .{ .ACCMODE = .WRONLY, .CLOEXEC = true }, 0);
    if (linux.errno(fd_rc) != .SUCCESS) {
        log.warn("transport: cannot open {s} for writing ({t}); the kernel will refuse REGISTER if the ring was not created at INIT", .{ enable_uring_path, linux.errno(fd_rc) });
        return;
    }
    const fd: linux.fd_t = @intCast(fd_rc);
    defer _ = linux.close(fd);
    const rc = linux.write(fd, "Y", 1);
    if (linux.errno(rc) != .SUCCESS) log.warn("transport: writing Y to {s} failed ({t})", .{ enable_uring_path, linux.errno(rc) });
}

/// The device MINOR of the mount at `mountpoint` from a `mountinfo` text
/// (`core/mountinfo.zig`: the last matching line wins; a malformed line
/// is no record).
pub fn mountinfoMinor(text: []const u8, mountpoint: []const u8) ?u32 {
    const e = mountinfo.entryAt(text, mountpoint) orelse return null;
    return e.dev_minor;
}

/// The parameter's current verdict (null when unreadable: no FUSE module
/// or a kernel without the ring).
pub fn readEnableUring(io: Io) ?bool {
    var buf: [16]u8 = undefined;
    const text = Io.Dir.cwd().readFile(io, enable_uring_path, &buf) catch return null;
    const t = std.mem.trim(u8, text, " \t\r\n");
    if (t.len == 0) return null;
    return switch (t[0]) {
        'Y', 'y', '1' => true,
        else => false,
    };
}

// ---------------------------------------------------------------------------
// Tests: the REAL MOUNT (Linux venue, root, 7.2.6-sqz). Every test mounts
// `EchoFs` at a unique `/build/tmp/mnt-<pid>-<n>`, tears down in `defer`,
// and leaves no mount behind (a last-resort lazy detach guards failures).
// ---------------------------------------------------------------------------

const testing = std.testing;
const EchoFs = fsv.EchoFs;

const Rig = struct {
    gpa: std.mem.Allocator,
    env: std.process.Environ.Map,
    io_exec: *SqzIo,
    fs: *EchoFs,
    path: [:0]u8,
    session: ?*Session = null,

    fn init(gpa: std.mem.Allocator, tag: u32, env_pairs: []const [2][]const u8) !*Rig {
        const self = try gpa.create(Rig);
        self.* = .{ .gpa = gpa, .env = .init(gpa), .io_exec = undefined, .fs = undefined, .path = undefined };
        for (env_pairs) |kv| try self.env.put(kv[0], kv[1]);
        _ = linux.mkdir("/build/tmp", 0o755);
        self.path = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/mnt-{d}-{d}", .{ linux.getpid(), tag }, 0);
        _ = linux.mkdir(self.path.ptr, 0o755);
        const facts = derive.facts();
        self.io_exec = try SqzIo.init(gpa, .{
            .env = &self.env,
            .budget_bytes = derive.GiB,
            .fuse = .{ .nqueues = facts.possible_cpus, .q_depth = geometry.Q_DEPTH_DESIRED, .rings_per_queue = geometry.MAX_RINGS_PER_QUEUE },
            .meta_lanes = 1,
            .blocking_cap = 4,
        });
        self.fs = try gpa.create(EchoFs);
        self.fs.* = .{};
        return self;
    }

    fn mountIt(self: *Rig, cfg_in: Config) !*Session {
        var cfg = cfg_in;
        cfg.mem_budget_bytes = derive.GiB;
        const s = try Session.mount(self.gpa, self.io_exec, self.fs.filesystem(), self.path, &self.env, derive.facts(), cfg);
        self.session = s;
        return s;
    }

    fn deinit(self: *Rig) void {
        if (self.session) |s| s.deinit();
        // Last resort: never leave a mount behind.
        _ = linux.umount2(self.path.ptr, linux.MNT.DETACH);
        _ = linux.rmdir(self.path.ptr);
        self.io_exec.deinit();
        self.fs.deinit();
        self.gpa.destroy(self.fs);
        self.env.deinit();
        self.gpa.free(self.path);
        self.gpa.destroy(self);
    }

    fn child(self: *Rig, buf: []u8, name: []const u8) [:0]const u8 {
        return std.fmt.bufPrintSentinel(buf, "{s}/{s}", .{ self.path, name }, 0) catch unreachable;
    }
};

/// Root, `/dev/fuse` and CAP_SYS_ADMIN (`mount(2)`): the venue recipe
/// grants the capability (`scripts/venue up`); a container without it
/// skips the mount tests LOUDLY rather than failing them with EPERM.
fn venueMissing() ?testkit.Why {
    return testkit.venue.mount(std.testing.io, &.{});
}

/// The kernel side of BufRing by the session's own kmbuf ladder: a kernel
/// without the register opcode (a stock distro kernel) mounts UserEnts only —
/// no BufRing, and so no zero-copy and no retention, both riding it.
fn kmbufMissing(buf: []u8) ?testkit.Why {
    if (probes.driver.probeKmbuf(@intCast(derive.facts().page_size)).opcodes() != null) return null;
    return testkit.venue.kernelFeatureAbsent(buf, "the kmbuf register opcode (BufRing; zero-copy and retention ride it)");
}

fn statSize(path: [*:0]const u8) !u64 {
    var st: linux.Statx = undefined;
    const rc = linux.statx(linux.AT.FDCWD, path, 0, linux.STATX.BASIC_STATS, &st);
    if (linux.errno(rc) != .SUCCESS) return error.StatFailed;
    return st.size;
}

fn readWhole(path: [*:0]const u8, out: []u8) !usize {
    const rc = linux.open(path, .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) return error.OpenFailed;
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    var total: usize = 0;
    while (total < out.len) {
        const n = linux.read(fd, out[total..].ptr, out.len - total);
        if (linux.errno(n) != .SUCCESS) return error.ReadFailed;
        if (n == 0) break;
        total += n;
    }
    return total;
}

fn writeWhole(path: [*:0]const u8, data: []const u8) !usize {
    const rc = linux.open(path, .{ .ACCMODE = .WRONLY, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) return error.OpenFailed;
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    const n = linux.write(fd, data.ptr, data.len);
    if (linux.errno(n) != .SUCCESS) return error.WriteFailed;
    return n;
}

const Snapshot = struct {
    requests: u64,
    reads: u64,
    serves: u64,
    fusions: u64,
    demotes: u64,
    parked: u64,
    unparked: u64,
    abandoned: u64,
    overflows: u64,
    ticks: u64,
    commit_lost_wakes: u64,
    late_commits: u64,
    cqe_lost_wakes: u64,
    late_cqes: u64,
    writes: u64,
    elided: u64,
    enters: u64,
    replies: u64,
    sideband_reqs: u64,
    sideband_fgt: u64,
    synthetic: u64,
    refusals: u64,
    retired: u64,
    registers: u64,
    oversize: u64,
    tag_unknown: u64,
    on_leased: u64,
    bridge_cancels: u64,
    bridge_lost: u64,
    bridge_orphans: u64,
    release_failures: u64,
    retain_refused: u64,
    breach: u64,
    attached: u64,
    zc_replies: u64,
    zc_serve_bytes: u64,
    zc_bounce_bytes: u64,
    zc_bounce_copies: u64,
    fd_body_replies: u64,
    fd_body_fallbacks: u64,
    holds: u64,
    hold_bytes: u64,
    extractions: u64,
    lazy: u64,
    directs: u64,
    direct_bytes: u64,
    direct_failures: u64,
    direct_refusals: u64,
    retain_commits: u64,
    releases: u64,
    retain_refused_class: u64,
    direction_refusals: u64,
    direct_legs: u64,
    bridges: u64,
    late_demotions: u64,
    /// The held write's three phase counts (each a sample per direct store).
    phase_to_store: u64,
    phase_store: u64,
    phase_to_commit: u64,

    fn take() Snapshot {
        return .{
            .phase_to_store = worker.held_write_to_store_ns.snapshot().count,
            .phase_store = worker.held_write_store_ns.snapshot().count,
            .phase_to_commit = worker.held_write_to_commit_ns.snapshot().count,
            .bridge_cancels = worker.bridge_cancels.load(),
            .bridge_lost = worker.bridge_lost.load(),
            .bridge_orphans = worker.bridge_orphans.load(),
            .release_failures = slot.release_failures.load(),
            .retain_refused = slot.retain_refused.load(),
            .breach = worker.kmbuf_attachment_breach.load(),
            .attached = worker.kmbuf_attached_deliveries.load(),
            .zc_replies = worker.zc_replies.load(),
            .zc_serve_bytes = worker.zc_serve_bytes.load(),
            .zc_bounce_bytes = worker.zc_bounce_bytes.load(),
            .zc_bounce_copies = worker.zc_bounce_copies.load(),
            .fd_body_replies = worker.zc_fd_body_replies.load(),
            .fd_body_fallbacks = worker.zc_fd_body_fallbacks.load(),
            .holds = slot.zc_write_holds.load(),
            .hold_bytes = slot.zc_write_hold_bytes.load(),
            .extractions = slot.zc_write_extractions.load(),
            .lazy = slot.zc_write_lazy_extractions.load(),
            .directs = worker.zc_write_directs.load(),
            .direct_bytes = worker.zc_write_direct_bytes.load(),
            .direct_failures = worker.zc_write_direct_failures.load(),
            .direct_refusals = worker.zc_write_direct_refusals.load(),
            .retain_commits = slot.retain_commits.load(),
            .releases = slot.releases.load(),
            .retain_refused_class = slot.retain_refused_class.load(),
            .direction_refusals = slot.zc_direction_refusals.load(),
            .direct_legs = worker.zc_direct_legs.load(),
            .bridges = worker.zc_bridges.load(),
            .late_demotions = worker.fused_read_late_demotions.load(),
            .requests = worker.ring_requests.load(),
            .reads = worker.ring_reads.load(),
            .serves = worker.fast_dispatch_serves.load(),
            .fusions = worker.fused_read_fusions.load(),
            .demotes = worker.fast_dispatch_demotes.load(),
            .parked = slot.parked_commits.load(),
            .unparked = slot.unparked_commits.load(),
            .abandoned = slot.requests_abandoned.load(),
            .overflows = worker.cq_overflows.load(),
            .ticks = worker.park_backstop_ticks.load(),
            .commit_lost_wakes = worker.park_tick_commit_lost_wakes.load(),
            .late_commits = worker.park_tick_late_commits.load(),
            .cqe_lost_wakes = worker.park_tick_cqe_lost_wakes.load(),
            .late_cqes = worker.park_tick_late_cqes.load(),
            .writes = slot.wake_writes.load(),
            .elided = slot.wakes_elided.load(),
            .enters = worker.enters.load(),
            .replies = worker.ring_replies.load(),
            .sideband_reqs = sideband_requests.load(),
            .sideband_fgt = sideband_forgets.load(),
            .synthetic = slot.requests_failed_synthetic.load(),
            .refusals = slot.register_refusals.load(),
            .retired = slot.ents_retired.load(),
            .registers = worker.registers.load(),
            .oversize = slot.replies_oversize.load(),
            .tag_unknown = worker.cqe_tag_unknown.load(),
            .on_leased = worker.delivery_on_leased.load(),
        };
    }
};

/// Every owed slot by name — queue, ent, state, unique, opcode, age and
/// where its reply is (the cell, a lease park, a bridge) — for a test whose
/// owed set did not drain. Read racily off the workers' tables.
fn describeOwed(s: *Session) void {
    const now = clock.monotonicNs();
    for (s.groups) |g| for (g.members, 0..) |*m, ms| for (m.slots.ents, 0..) |*e, ei| {
        const o = e.owing() orelse continue;
        const gent = g.gentOf(@intCast(ms), @intCast(ei));
        const op = if (abi.decodePrefix(abi.fuse_in_header, &g.headers.at(gent).in_out)) |h| @tagName(h.op()) else "?";
        const since = e.owed_since_ns.load(.acquire);
        const cell: commit_queue.CellState = @fromBackingInt(@intCast(g.cells.cells[gent].state.load(.acquire)));
        std.debug.print("owed: queue {d} ent {d} {t} unique {d} commit {d} {s}, owed {d} ms, reply cell {t}, lease-parked {}, bridge {t}\n", .{ m.qid, ei, e.state, o.unique, o.commit_id, op, (now -| since) / std.time.ns_per_ms, cell, m.parked[ei], g.bridge_parked[gent] });
    };
}

/// The must-stay-0 law (as deltas: the slot unit tests fire the oversize
/// tripwire on purpose in the same process) and the partition law.
fn expectLaws(a: Snapshot, b: Snapshot) !void {
    try testing.expectEqual(a.abandoned, b.abandoned);
    try testing.expectEqual(a.overflows, b.overflows);
    try testing.expectEqual(a.oversize, b.oversize);
    try testing.expectEqual(a.tag_unknown, b.tag_unknown);
    try testing.expectEqual(a.on_leased, b.on_leased);
    try testing.expectEqual(a.bridge_lost, b.bridge_lost);
    try testing.expectEqual(a.bridge_orphans, b.bridge_orphans);
    try testing.expectEqual(a.release_failures, b.release_failures);
    try testing.expectEqual(a.retain_refused, b.retain_refused);
    try testing.expectEqual(a.breach, b.breach);
    try testing.expectEqual(b.reads - a.reads, (b.serves - a.serves) + (b.fusions - a.fusions) + (b.demotes - a.demotes));
    // The bridge pool is empty at quiesce.
    try testing.expectEqual(@as(u64, 0), worker.zc_bridge_pends.load());
}

/// The lattice rung under test, as the env pair the rig passes.
fn modeEnv(mode: []const u8) [2][]const u8 {
    return .{ "SQUEEZEFS_FUSE_BUFFER_MODE", mode };
}

fn preadAt(fd: linux.fd_t, buf: []u8, off: u64) !usize {
    const got = linux.pread(fd, buf.ptr, buf.len, @intCast(off));
    if (linux.errno(got) != .SUCCESS) return error.ReadFailed;
    return got;
}

fn openPath(path: [*:0]const u8, flags: linux.O) !linux.fd_t {
    const rc = linux.open(path, flags, 0);
    if (linux.errno(rc) != .SUCCESS) return error.OpenFailed;
    return @intCast(rc);
}

/// Wait (bounded, yielding) until `pred` holds.
fn waitUntil(ctx: anytype, comptime pred: fn (@TypeOf(ctx)) bool, budget_ns: u64) bool {
    const deadline = clock.monotonicNs() + budget_ns;
    while (!pred(ctx)) {
        if (clock.monotonicNs() >= deadline) return false;
        std.Thread.yield() catch {};
    }
    return true;
}

/// The kernel's ratelimited warn when its teardown reclaims a retained zc
/// ent the daemon never RELEASEd (patch 0024): the one line an orderly
/// teardown must never add.
const retained_warn_needle = "teardown with retained zc payloads";

/// A mark on the kernel log ring (`syslog(2)` READ_ALL; root): how many
/// lines carry `needle` and the LAST such line verbatim — two marks are
/// equal iff no new line appeared, even if the ring evicted old ones.
const KernelLogMark = struct {
    count: usize,
    last: [256]u8 = undefined,
    last_len: usize = 0,

    const SYSLOG_ACTION_READ_ALL = 3;
    const SYSLOG_ACTION_SIZE_BUFFER = 10;

    fn take(gpa: std.mem.Allocator, needle: []const u8) !KernelLogMark {
        const size_rc = linux.syscall3(.syslog, SYSLOG_ACTION_SIZE_BUFFER, 0, 0);
        if (linux.errno(size_rc) != .SUCCESS) return error.SyslogFailed;
        const buf = try gpa.alloc(u8, size_rc);
        defer gpa.free(buf);
        const n_rc = linux.syscall3(.syslog, SYSLOG_ACTION_READ_ALL, @intFromPtr(buf.ptr), buf.len);
        if (linux.errno(n_rc) != .SUCCESS) return error.SyslogFailed;
        var mark: KernelLogMark = .{ .count = 0 };
        var lines = std.mem.splitScalar(u8, buf[0..n_rc], '\n');
        while (lines.next()) |line| {
            if (std.mem.indexOf(u8, line, needle) == null) continue;
            mark.count += 1;
            mark.last_len = @min(line.len, mark.last.len);
            @memcpy(mark.last[0..mark.last_len], line[0..mark.last_len]);
        }
        return mark;
    }

    fn lastLine(self: *const KernelLogMark) []const u8 {
        return self.last[0..self.last_len];
    }

    fn sameAs(self: *const KernelLogMark, other: *const KernelLogMark) bool {
        return self.count == other.count and std.mem.eql(u8, self.lastLine(), other.lastLine());
    }
};

/// A teardown bound for a test: a thread that reports LOUDLY (the retained
/// ledger, the owed count, the elapsed time) when `close` has not returned
/// within `bound_ns` and then ends the process — a teardown pinned
/// kernel-side (D-state workers) is otherwise a silent multi-hour hang
/// the gate's `timeout` cannot explain. `stop` before the bound is the
/// normal path and the thread exits quietly.
const TeardownWatchdog = struct {
    done: futex.Word = .init(0),
    thread: std.Thread = undefined,
    bound_ns: u64,
    what: []const u8,

    fn start(self: *TeardownWatchdog) !void {
        self.thread = try std.Thread.spawn(.{}, run, .{self});
    }

    fn run(self: *TeardownWatchdog) void {
        const t0 = clock.monotonicNs();
        while (self.done.load(.acquire) == 0) {
            const now = clock.monotonicNs();
            if (now - t0 >= self.bound_ns) {
                std.debug.print("\n=== teardown watchdog: {s} did not return within {d} ms (retained_outstanding {d}, releases_inflight {d}, released_at_teardown {d}, kernel_drained {d}) — the kernel pinned the teardown; exiting the test process ===\n", .{ self.what, self.bound_ns / std.time.ns_per_ms, slot.retained_outstanding.load(), worker.releases_inflight.load(), worker.retained_released_at_teardown.load(), worker.retained_kernel_drained.load() });
                std.process.exit(3);
            }
            _ = futex.waitTimeout(&self.done, 0, self.bound_ns - (now - t0));
        }
    }

    fn stop(self: *TeardownWatchdog) void {
        self.done.store(1, .release);
        futex.wakeAll(&self.done);
        self.thread.join();
    }
};

test "e2e: a LOOKUP and a GETATTR the filesystem answers from RAM never leave the worker — zero handler-lane dispatches, the probe served each, `serves + demotes ≡ LOOKUP + GETATTR`; a declining probe still answers through the lane (decision 0003 §AB.1)" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    const rig = try Rig.init(testing.allocator, 7, &.{});
    defer rig.deinit();
    rig.fs.meta_probe = true;
    _ = try rig.mountIt(.{ .mem_budget_bytes = 0 });
    const Helper = struct {
        fn run(r: *Rig, result: *anyerror!void) void {
            result.* = body(r);
        }
        fn body(r: *Rig) !void {
            var pb: [256]u8 = undefined;
            try testing.expectEqual(@as(u64, EchoFs.hello_text.len), try statSize(r.child(&pb, "hello")));
            try testing.expectEqual(@as(u64, EchoFs.big_len), try statSize(r.child(&pb, "big")));
            try testing.expectError(error.StatFailed, statSize(r.child(&pb, "nope")));
        }
    };
    const d0 = rig.fs.dispatches.load(.monotonic);
    const p0 = rig.fs.meta_probes_served.load(.monotonic);
    const s0 = worker.meta_inline_serves.load();
    const m0 = worker.meta_inline_demotes.load();
    var result: anyerror!void = {};
    const t = try std.Thread.spawn(.{}, Helper.run, .{ rig, &result });
    t.join();
    try result;
    const served = rig.fs.meta_probes_served.load(.monotonic) - p0;
    try testing.expect(served >= 3);
    try testing.expectEqual(d0, rig.fs.dispatches.load(.monotonic));
    try testing.expectEqual(served, worker.meta_inline_serves.load() - s0);
    try testing.expectEqual(m0, worker.meta_inline_demotes.load());
    try testing.expectEqual(@as(u64, 1), rig.fs.lookups[@intCast(EchoFs.ino_hello)].load(.monotonic));
    // The probe off: the same requests take the lane, counted demotes.
    rig.fs.meta_probe = false;
    const Helper2 = struct {
        fn run(r: *Rig, result2: *anyerror!void) void {
            var pb: [256]u8 = undefined;
            result2.* = if (statSize(r.child(&pb, "nope2"))) |_| error.TestUnexpectedResult else |_| {};
        }
    };
    const d1 = rig.fs.dispatches.load(.monotonic);
    const m1 = worker.meta_inline_demotes.load();
    var result2: anyerror!void = {};
    const t2 = try std.Thread.spawn(.{}, Helper2.run, .{ rig, &result2 });
    t2.join();
    try result2;
    try testing.expect(rig.fs.dispatches.load(.monotonic) > d1);
    try testing.expect(worker.meta_inline_demotes.load() > m1);
}

test "e2e: a no-open session (decision 0003 §AF) — INIT drops FUSE_ATOMIC_O_TRUNC; the first OPEN answered ENOSYS latches the kernel, so 16 opens of /hello send ONE OPEN and no RELEASE; every close sends its FLUSH, answered on the worker by the probe (no handler dispatch); a FLUSH the probe declines (custody) drains on the CPU's lane, never the worker's; an O_TRUNC open reaches the filesystem as a SETATTR(size 0)" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    const rig = try Rig.init(testing.allocator, 41, &.{});
    defer rig.deinit();
    rig.fs.no_open = true;
    // LOOKUP / GETATTR answer on the worker too: the only lane handlers
    // are the ones the row counts.
    rig.fs.meta_probe = true;
    const s = try rig.mountIt(.{ .mem_budget_bytes = 0, .no_open = true });
    try testing.expect(rig.fs.caps.?.negotiated & abi.FUSE_ATOMIC_O_TRUNC == 0);
    const Helper = struct {
        fn readFiles(r: *Rig, result: *anyerror!void) void {
            result.* = readBody(r);
        }
        fn readBody(r: *Rig) !void {
            var pb: [256]u8 = undefined;
            var buf: [64]u8 = undefined;
            var i: usize = 0;
            while (i < 16) : (i += 1) try testing.expectEqual(EchoFs.hello_text.len, try readWhole(r.child(&pb, "hello"), &buf));
        }
        fn truncScratch(r: *Rig, result: *anyerror!void) void {
            result.* = truncBody(r);
        }
        fn truncBody(r: *Rig) !void {
            var pb: [256]u8 = undefined;
            const path = r.child(&pb, "scratch");
            try testing.expectEqual(@as(usize, 5), try writeWhole(path, "hello"));
            try testing.expectEqual(@as(u64, 5), try statSize(path));
            const fd = try openPath(path, .{ .ACCMODE = .WRONLY, .TRUNC = true, .CLOEXEC = true });
            _ = linux.close(fd);
        }
        fn run(r: *Rig, comptime f: fn (*Rig, *anyerror!void) void) !void {
            var result: anyerror!void = {};
            const t = try std.Thread.spawn(.{}, f, .{ r, &result });
            t.join();
            try result;
        }
    };
    const serves0 = worker.flush_inline_serves.load();
    const demotes0 = worker.flush_inline_demotes.load();
    try Helper.run(rig, Helper.readFiles);
    try testing.expectEqual(@as(u64, 1), rig.fs.opens_enosys.load(.monotonic));
    try testing.expectEqual(@as(u64, 16), rig.fs.flush_probes_served.load(.monotonic));
    try testing.expectEqual(@as(u64, 16), worker.flush_inline_serves.load() - serves0);
    try testing.expectEqual(demotes0, worker.flush_inline_demotes.load());
    try testing.expectEqual(@as(u64, 0), rig.fs.flushes_dispatched.load(.monotonic));
    // No RELEASE reaches the daemon — not even for the file whose OPEN
    // latched the connection (`fuse_file_put` ends it locally).
    try testing.expectEqual(@as(u64, 0), rig.fs.file_releases.load(.monotonic));
    // Custody: the probe declines; each FLUSH runs on the CPU's lane.
    rig.fs.flush_custody = true;
    const foreign0 = worker.handler_dispatches_foreign.load();
    try Helper.run(rig, Helper.readFiles);
    try testing.expectEqual(@as(u64, 16), rig.fs.flushes_dispatched.load(.monotonic));
    try testing.expectEqual(@as(u64, 16), worker.flush_inline_demotes.load() - demotes0);
    try testing.expect(worker.handler_dispatches_foreign.load() - foreign0 >= 16);
    // The last handler run was a FLUSH: off every drain worker.
    const tid = rig.fs.handler_tid.load(.acquire);
    for (s.groups) |g| try testing.expect(g.tid.load(.acquire) != tid);
    rig.fs.flush_custody = false;
    // O_TRUNC: no OPEN carries it — the kernel sends the SETATTR(size 0).
    const sets0 = rig.fs.size_sets.load(.monotonic);
    try Helper.run(rig, Helper.truncScratch);
    try testing.expectEqual(sets0 + 1, rig.fs.size_sets.load(.monotonic));
    try testing.expectEqual(@as(u64, 0), rig.fs.scratch_len.load(.acquire));
    try testing.expectEqual(@as(u64, 1), rig.fs.opens_enosys.load(.monotonic));
    try testing.expectEqual(@as(u64, 0), rig.fs.file_releases.load(.monotonic));
}

test "e2e: the write-back timer's close (decision 0003 §AJ) — on the real kernel a FLUSH answered ENOSYS latches `no_flush`: after ONE open and close of /hello (the OPEN and the FLUSH that latch both postures), 32 more fds of it open and close and the closes send NO request at all — `transport_ring_requests` unmoved across them — and the whole session saw ONE OPEN, ONE FLUSH, no RELEASE" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    const rig = try Rig.init(testing.allocator, 41, &.{});
    defer rig.deinit();
    rig.fs.no_open = true;
    rig.fs.flush_enosys = true;
    rig.fs.meta_probe = true;
    _ = try rig.mountIt(.{ .mem_budget_bytes = 0, .no_open = true });
    const Helper = struct {
        fn closes(r: *Rig, result: *anyerror!void) void {
            result.* = body(r);
        }
        fn body(r: *Rig) !void {
            var pb: [256]u8 = undefined;
            const path = r.child(&pb, "hello");
            _ = linux.close(try openPath(path, .{ .ACCMODE = .RDONLY, .CLOEXEC = true }));
            var fds: [32]linux.fd_t = undefined;
            for (&fds) |*fd| fd.* = try openPath(path, .{ .ACCMODE = .RDONLY, .CLOEXEC = true });
            const r0 = worker.ring_requests.load();
            for (fds) |fd| _ = linux.close(fd);
            try testing.expectEqual(r0, worker.ring_requests.load());
        }
    };
    var result: anyerror!void = {};
    const t = try std.Thread.spawn(.{}, Helper.closes, .{ rig, &result });
    t.join();
    try result;
    try testing.expectEqual(@as(u64, 1), rig.fs.opens_enosys.load(.monotonic));
    try testing.expectEqual(@as(u64, 1), rig.fs.flushes_enosys.load(.monotonic));
    try testing.expectEqual(@as(u64, 0), rig.fs.flushes_dispatched.load(.monotonic));
    try testing.expectEqual(@as(u64, 0), rig.fs.flush_probes_served.load(.monotonic));
    try testing.expectEqual(@as(u64, 0), rig.fs.file_releases.load(.monotonic));
}

test "the dispatch law's FLUSH arm (decision 0003 §AF): a no-open FLUSH carries RELEASE's drain and is the data class — the CPU's lane — while the control arm's runs on the worker's lane; a WRITE runs on the worker's lane iff its payload is HELD and the lane has room (the W1 hold gate's slot: its one DMA is this worker's own bridge), an extracted or spilled WRITE and a READ stay the data class; the rest of the law is unchanged; the own-lane depth's shipped default is the measured knee, 8 (the 2026-10-19 ruling: all-own once lever 5 lands)" {
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    try testing.expectEqual(@as(u32, 8), knobs.intKnob("SQUEEZEFS_FUSE_WRITE_OWN_DEPTH", u32, &env));
    const flush: codec.Request = .{ .flush = .{ .fh = 0, .lock_owner = 0 } };
    try testing.expect(worker.onWorkerLane(flush, false, false));
    try testing.expect(!worker.onWorkerLane(flush, true, false));
    const write: codec.Request = .{ .write = .{ .in = .{ .fh = 0, .offset = 4096, .size = 4096, .write_flags = 0, .lock_owner = 0, .flags = 0, .padding = 0 }, .data = &.{} } };
    const read: codec.Request = .{ .read = .{ .fh = 0, .offset = 0, .size = 4096, .read_flags = 0, .lock_owner = 0, .flags = 0, .padding = 0 } };
    for ([_]bool{ false, true }) |no_open| {
        try testing.expect(worker.onWorkerLane(.{ .lookup = .{ .name = "a" } }, no_open, false));
        try testing.expect(!worker.onWorkerLane(.{ .release = .{ .fh = 0, .flags = 0, .release_flags = 0, .lock_owner = 0 } }, no_open, false));
        try testing.expect(worker.onWorkerLane(.{ .open = .{ .flags = 0, .open_flags = 0 } }, no_open, false));
        try testing.expect(!worker.onWorkerLane(.{ .open = .{ .flags = @bitCast(linux.O{ .TRUNC = true }), .open_flags = 0 } }, no_open, false));
        try testing.expect(worker.onWorkerLane(write, no_open, true));
        try testing.expect(!worker.onWorkerLane(write, no_open, false));
        // The held flag names a WRITE's payload alone: it admits nothing else.
        try testing.expect(!worker.onWorkerLane(read, no_open, true));
        try testing.expect(!worker.onWorkerLane(.{ .release = .{ .fh = 0, .flags = 0, .release_flags = 0, .lock_owner = 0 } }, no_open, true));
    }
}

test "e2e: a namespace request's handler runs on the thread that delivered it — the worker drives its own lane: every OPENDIR / READDIR / RELEASEDIR / OPEN lands on it, the handler's thread IS a drain worker's, the reply commits with no cross-thread wake; the data class (RELEASE, a demoted READ) stays on the handler lane; the control arm (SQUEEZEFS_FUSE_HANDLERS_ON_WORKER=0) crosses a thread each way (decision 0003 §AB.3)" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    const Helper = struct {
        fn listRoot(r: *Rig, result: *anyerror!void) void {
            result.* = listBody(r);
        }
        fn listBody(r: *Rig) !void {
            var buf: [4096]u8 align(8) = undefined;
            var i: usize = 0;
            while (i < 64) : (i += 1) {
                const rc = linux.open(r.path.ptr, .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .CLOEXEC = true }, 0);
                if (linux.errno(rc) != .SUCCESS) return error.OpenFailed;
                const fd: linux.fd_t = @intCast(rc);
                defer _ = linux.close(fd);
                while (true) {
                    const n = linux.getdents64(fd, &buf, buf.len);
                    if (linux.errno(n) != .SUCCESS) return error.ReaddirFailed;
                    if (n == 0) break;
                }
            }
        }
        fn readFiles(r: *Rig, result: *anyerror!void) void {
            result.* = readBody(r);
        }
        fn readBody(r: *Rig) !void {
            var pb: [256]u8 = undefined;
            var buf: [64]u8 = undefined;
            var i: usize = 0;
            while (i < 16) : (i += 1) _ = try readWhole(r.child(&pb, "hello"), &buf);
        }
        /// The handler's thread is one of the session's drain workers.
        fn onWorker(r: *Rig, s: *Session) bool {
            const tid = r.fs.handler_tid.load(.acquire);
            for (s.groups) |g| if (g.tid.load(.acquire) == tid) return true;
            return false;
        }
        /// THIS session's handler dispatches since its mount, summed over its
        /// drain workers: no other session in the process moves them.
        fn dispatches(s: *Session) struct { own: u64, foreign: u64 } {
            var own: u64 = 0;
            var foreign: u64 = 0;
            for (s.groups) |g| {
                own += g.dispatches_own.load(.monotonic);
                foreign += g.dispatches_foreign.load(.monotonic);
            }
            return .{ .own = own, .foreign = foreign };
        }
        /// Until every dispatch since the mount has run its handler — the
        /// rig's EchoFs serves this session alone, so its count and the
        /// session's need no baselines (two baselines' loads straddled by a
        /// whole request count it in one and not the other). Bounded: a
        /// stuck one fails the equalities below.
        fn settle(r: *Rig, s: *Session) struct { handled: u64, own: u64, foreign: u64 } {
            const deadline = clock.monotonicNs() + 2 * std.time.ns_per_s;
            while (true) {
                const handled = r.fs.dispatches.load(.monotonic);
                const d = dispatches(s);
                if (handled == d.own + d.foreign or clock.monotonicNs() >= deadline) return .{ .handled = handled, .own = d.own, .foreign = d.foreign };
                std.Thread.yield() catch {};
            }
        }
        fn run(r: *Rig, comptime f: fn (*Rig, *anyerror!void) void) !void {
            var result: anyerror!void = {};
            const t = try std.Thread.spawn(.{}, f, .{ r, &result });
            t.join();
            try result;
        }
    };
    {
        const rig = try Rig.init(testing.allocator, 38, &.{});
        defer rig.deinit();
        const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
        // The namespace class: every handler on the delivering worker.
        const own0 = worker.handler_dispatches_own.load();
        const w0 = slot.wake_writes.load();
        try Helper.run(rig, Helper.listRoot);
        // The last RELEASEDIR is asynchronous (close(2) returns before it
        // is answered): wait until every counted dispatch has run.
        const settled = Helper.settle(rig, s);
        try testing.expect(settled.handled >= 128);
        try testing.expectEqual(settled.handled, settled.own);
        try testing.expectEqual(@as(u64, 0), settled.foreign);
        // The process-wide stat counts them too (other sessions only add).
        try testing.expect(worker.handler_dispatches_own.load() - own0 >= 128);
        try testing.expect(Helper.onWorker(rig, s));
        // No reply crossed a thread: not one eventfd write to a worker.
        try testing.expectEqual(w0, slot.wake_writes.load());
        // The data class: each file's RELEASE on the handler lane, its OPEN
        // on the worker (a READ the worker answers inline under the READ
        // law's arm (a) dispatches nothing; a demoted one is foreign too).
        const d1 = Helper.dispatches(s);
        try Helper.run(rig, Helper.readFiles);
        // The RELEASEs are asynchronous too: wait for the sixteenth.
        const deadline = clock.monotonicNs() + 2 * std.time.ns_per_s;
        while (Helper.dispatches(s).foreign - d1.foreign < 16 and clock.monotonicNs() < deadline) std.Thread.yield() catch {};
        const d2 = Helper.dispatches(s);
        try testing.expect(d2.foreign - d1.foreign >= 16);
        try testing.expect(d2.own - d1.own >= 16);
    }
    {
        const rig = try Rig.init(testing.allocator, 39, &.{.{ "SQUEEZEFS_FUSE_HANDLERS_ON_WORKER", "0" }});
        defer rig.deinit();
        const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
        const w0 = slot.wake_writes.load();
        try Helper.run(rig, Helper.listRoot);
        try testing.expectEqual(@as(u64, 0), Helper.dispatches(s).own);
        try testing.expect(!Helper.onWorker(rig, s));
        // The lane thread's replies wake the worker across threads.
        try testing.expect(slot.wake_writes.load() > w0);
    }
}

test "e2e: EchoFs mounts over the ring — INIT negotiated FUSE_OVER_IO_URING, enable_uring reads Y, every queue armed, stat/open/read from a helper thread arrive over the RING (the sideband saw no request), replies byte-exact, the geometry the kernel accepted equals the plan" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    const rig = try Rig.init(testing.allocator, 1, &.{});
    defer rig.deinit();
    const before = Snapshot.take();
    const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
    try testing.expect(s.isReady());
    try testing.expect(s.init_reply.has(abi.FUSE_OVER_IO_URING));
    try testing.expectEqual(@as(?bool, true), readEnableUring(rig.io_exec.io()));
    try testing.expectEqual(derive.facts().possible_cpus, s.plan.nqueues);
    try testing.expectEqual(s.plan.nqueues, s.queuesRegistered());
    try testing.expectEqual(s.plan.nqueues, geometry.gauges.queues.load());
    try testing.expectEqual(s.plan.max_write, s.init_reply.out.max_write);
    try testing.expectEqual(s.plan.max_pages, s.init_reply.out.max_pages);
    try testing.expectEqual(geometry.kernelRingMaxPayloadSz(s.plan.max_pages_limit, s.plan.max_write, s.plan.max_pages, s.plan.page), @as(u64, s.plan.payload_sz));
    try testing.expectEqual(@as(u64, 1), rig.fs.inited.load());

    const Helper = struct {
        fn run(r: *Rig, result: *anyerror!void) void {
            result.* = body(r);
        }
        fn body(r: *Rig) !void {
            var pb: [256]u8 = undefined;
            try testing.expectEqual(@as(u64, EchoFs.hello_text.len), try statSize(r.child(&pb, "hello")));
            try testing.expectEqual(@as(u64, EchoFs.big_len), try statSize(r.child(&pb, "big")));
            try testing.expectError(error.StatFailed, statSize(r.child(&pb, "nope")));
            var small: [64]u8 = undefined;
            const n = try readWhole(r.child(&pb, "hello"), &small);
            try testing.expectEqualSlices(u8, EchoFs.hello_text, small[0..n]);
            const big = try testing.allocator.alloc(u8, EchoFs.big_len);
            defer testing.allocator.free(big);
            try testing.expectEqual(EchoFs.big_len, try readWhole(r.child(&pb, "big"), big));
            for (big, 0..) |b, i| if (b != EchoFs.bigByte(i)) return error.BigPatternMismatch;
        }
    };
    var result: anyerror!void = {};
    const t = try std.Thread.spawn(.{}, Helper.run, .{ rig, &result });
    t.join();
    try result;

    // The helper's last close returns before its RELEASE (a background
    // request) is answered: the requests ≡ replies law is read once the
    // worker pushed a reply for every request it delivered (bounded).
    const Settled = struct {
        fn pred(b: *const Snapshot) bool {
            return worker.ring_replies.load() - b.replies == worker.ring_requests.load() - b.requests;
        }
    };
    // A background request can still land between the settled read and the
    // snapshot (a worker answering on its own thread settles first): the
    // snapshot itself must show the equality (bounded).
    const settle_deadline = clock.monotonicNs() + 5 * std.time.ns_per_s;
    var after = Snapshot.take();
    while (after.requests - before.requests != after.replies - before.replies and clock.monotonicNs() < settle_deadline) {
        _ = waitUntil(&before, Settled.pred, settle_deadline -| clock.monotonicNs());
        after = Snapshot.take();
    }
    try expectLaws(before, after);
    try testing.expect(after.requests - before.requests > 0);
    try testing.expect(after.reads - before.reads >= 2);
    try testing.expect(after.serves - before.serves >= 2);
    try testing.expectEqual(before.sideband_reqs, after.sideband_reqs);
    try testing.expectEqual(after.requests - before.requests, after.replies - before.replies);
    var json: Io.Writer.Allocating = .init(testing.allocator);
    defer json.deinit();
    try s.writeStatsJson(&json.writer);
    try testing.expect(std.mem.indexOf(u8, json.written(), "\"transport_ring_requests\":") != null);
    const measured = after.requests - before.requests;
    std.debug.print("\ne2e: {d} ring requests, {d} reads ({d} served inline, {d} demoted), {d} enters ({d}.{d:0>2} per op), wake writes {d} / elided {d}\n", .{ measured, after.reads - before.reads, after.serves - before.serves, after.demotes - before.demotes, after.enters - before.enters, (after.enters - before.enters) / @max(measured, 1), ((after.enters - before.enters) * 100 / @max(measured, 1)) % 100, after.writes - before.writes, after.elided - before.elided });
    s.close();
    try testing.expectEqual(@as(u64, 1), rig.fs.destroyed.load());
    try testing.expectEqual(after.abandoned, slot.requests_abandoned.load());
}

test "e2e: a FUSE_WRITE payload is a zero-copy lease; a handler that replies while holding it parks the commit, the drop wakes the worker and the commit lands: parked ≡ unparked, bytes landed" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    const rig = try Rig.init(testing.allocator, 2, &.{});
    defer rig.deinit();
    rig.fs.hold_lease_across_reply = true;
    _ = try rig.mountIt(.{ .mem_budget_bytes = 0 });
    const before = Snapshot.take();
    const leases_before = slot.payload_leases.load();
    const data = "the lease outlives its reply, briefly";
    var pb: [256]u8 = undefined;
    const Helper = struct {
        fn run(path: [*:0]const u8, d: []const u8, result: *anyerror!usize) void {
            result.* = writeWhole(path, d);
        }
    };
    var result: anyerror!usize = 0;
    const t = try std.Thread.spawn(.{}, Helper.run, .{ rig.child(&pb, "scratch").ptr, @as([]const u8, data), &result });
    t.join();
    try testing.expectEqual(data.len, try result);
    const after = Snapshot.take();
    try expectLaws(before, after);
    try testing.expectEqual(@as(u64, 1), rig.fs.writes_seen.load(.monotonic));
    try testing.expectEqual(@as(u64, data.len), rig.fs.write_bytes.load(.monotonic));
    try testing.expectEqualSlices(u8, data, rig.fs.scratch[0..data.len]);
    try testing.expectEqual(leases_before + 1, slot.payload_leases.load());
    try testing.expectEqual(@as(u64, 1), rig.fs.lease_waits_parked.load(.monotonic));
    try testing.expectEqual(@as(u64, 1), after.parked - before.parked);
    try testing.expectEqual(after.parked - before.parked, after.unparked - before.unparked);
    try testing.expectEqual(@as(u64, 0), slot.leases_outstanding.load());
    try testing.expectEqual(@as(u64, data.len), try statSize(rig.child(&pb, "scratch")));
}

test "e2e: FORGETs arrive and are committed (classically on this kernel) after a lookup storm and drop_caches; the sideband saw FORGET-class traffic only" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    const rig = try Rig.init(testing.allocator, 3, &.{});
    defer rig.deinit();
    const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
    const before = Snapshot.take();
    var pb: [256]u8 = undefined;
    var i: u32 = 0;
    while (i < 8) : (i += 1) {
        for (EchoFs.names) |name| _ = try statSize(rig.child(&pb, name));
    }
    try testing.expect(rig.fs.lookups[EchoFs.ino_hello].load(.monotonic) >= 1);
    // Evict the dentries: `FUSE_NOTIFY_INVAL_ENTRY` drops each dentry and
    // (fuse's `drop_inode = generic_delete_inode`) evicts the inode at once,
    // which queues its FORGET — deterministic, unlike a drop_caches that a
    // container may not expose.
    for (EchoFs.names) |name| try s.notifyInvalEntry(EchoFs.ino_root, name, 0);
    // The FORGETs are asynchronous: wait for the filesystem's own ledger
    // (a bounded poll, never a sleep).
    const deadline = clock.monotonicNs() + 5 * std.time.ns_per_s;
    while (rig.fs.forgets.load(.monotonic) < EchoFs.names.len and clock.monotonicNs() < deadline) std.Thread.yield() catch {};
    const after = Snapshot.take();
    try expectLaws(before, after);
    try testing.expect(rig.fs.forgets.load(.monotonic) >= EchoFs.names.len);
    try testing.expectEqual(@as(u64, 0), rig.fs.lookups[EchoFs.ino_hello].load(.monotonic));
    try testing.expectEqual(rig.fs.forgets.load(.monotonic), (after.sideband_fgt - before.sideband_fgt) + worker.forget_entries.load());
    try testing.expectEqual(before.sideband_reqs, after.sideband_reqs);
}

test "e2e: teardown with an in-flight request — the drain synthesizes the error, the blocked reader returns, umount is not EBUSY, nothing is abandoned" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    const rig = try Rig.init(testing.allocator, 4, &.{});
    defer rig.deinit();
    const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
    const before = Snapshot.take();
    var pb: [256]u8 = undefined;
    const Helper = struct {
        fn run(path: [*:0]const u8, result: *anyerror!usize) void {
            var buf: [64]u8 = undefined;
            result.* = readWhole(path, &buf);
        }
    };
    var result: anyerror!usize = 0;
    const t = try std.Thread.spawn(.{}, Helper.run, .{ rig.child(&pb, "park").ptr, &result });
    // The handler is parked on the lane (a fiber wait), the slot owed.
    const deadline = clock.monotonicNs() + 5 * std.time.ns_per_s;
    while (rig.fs.parked_readers.load(.monotonic) == 0 and clock.monotonicNs() < deadline) std.Thread.yield() catch {};
    try testing.expectEqual(@as(u32, 1), rig.fs.parked_readers.load(.monotonic));
    try testing.expect(s.owedCount() >= 1);
    // Phase 1: the drain answers the owed slot; umount succeeds.
    s.close();
    try testing.expect(!s.isMounted());
    t.join();
    try testing.expectError(error.ReadFailed, result);
    const after = Snapshot.take();
    try testing.expectEqual(after.abandoned, before.abandoned);
    try testing.expect(after.synthetic - before.synthetic >= 1);
    try testing.expectEqual(@as(u32, 0), s.owedCount());
    // Release the parked handler: its late reply is dropped (the session
    // owes nothing), then phase 2 joins it. The baseline is read before
    // the release (the handler's drop may land at once).
    const dropped_before = replies_dropped_no_slot.load();
    rig.fs.releaseParked();
    s.deinit();
    rig.session = null;
    try testing.expect(replies_dropped_no_slot.load() >= dropped_before + 1);
}

test "e2e: teardown with a LOOKUP parked in its handler — the request's bytes (its name, in the payload buffer) stay mapped until every handler has finished: the handler released after `close` joined the workers reads its name intact, never a fault in an unmapped buffer region (the daemon's SIGSEGV in a `.stats` lookup racing `squeezefs umount`)" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    const rig = try Rig.init(testing.allocator, 9, &.{});
    defer rig.deinit();
    const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
    var pb: [256]u8 = undefined;
    const Helper = struct {
        fn run(path: [*:0]const u8, result: *anyerror!u64) void {
            result.* = statSize(path);
        }
    };
    var result: anyerror!u64 = 0;
    const t = try std.Thread.spawn(.{}, Helper.run, .{ rig.child(&pb, EchoFs.park_lookup_name).ptr, &result });
    const deadline = clock.monotonicNs() + 5 * std.time.ns_per_s;
    while (rig.fs.parked_lookups.load(.monotonic) == 0 and clock.monotonicNs() < deadline) std.Thread.yield() catch {};
    try testing.expectEqual(@as(u32, 1), rig.fs.parked_lookups.load(.monotonic));
    // Phase 1 with the handler mid-request: the drain answers the caller,
    // the workers are joined.
    s.close();
    t.join();
    try testing.expectError(error.StatFailed, result);
    // The handler resumes on a closed session, then phase 2 awaits it.
    rig.fs.releaseParked();
    s.deinit();
    rig.session = null;
    try testing.expectEqual(@as(u64, 0), rig.fs.names_unmapped_after_park.load(.monotonic));
    try testing.expectEqual(@as(u64, 1), rig.fs.names_intact_after_park.load(.monotonic));
}

test "e2e: a 10 s random-op storm from 4 threads never needs the 100 ms backstop (tick rescues stay 0), abandons nothing, overflows nothing; the partition law and parked ≡ unparked hold; the coalescer elides" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    const rig = try Rig.init(testing.allocator, 5, &.{});
    defer rig.deinit();
    rig.fs.probe_declines = true;
    const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
    const before = Snapshot.take();
    const Storm = struct {
        fn run(r: *Rig, seed: u64, ops: *std.atomic.Value(u64), opens: *std.atomic.Value(u64), result: *anyerror!void) void {
            result.* = body(r, seed, ops, opens);
        }
        fn body(r: *Rig, seed: u64, ops: *std.atomic.Value(u64), opens: *std.atomic.Value(u64)) !void {
            var prng = std.Random.DefaultPrng.init(seed);
            const rnd = prng.random();
            var pb: [256]u8 = undefined;
            var buf: [8192]u8 = undefined;
            const deadline = clock.monotonicNs() + 10 * std.time.ns_per_s;
            var n: u64 = 0;
            var o: u64 = 0;
            while (clock.monotonicNs() < deadline) : (n += 1) {
                const kind = rnd.uintLessThan(u8, 4);
                // Every kind but a stat opens (and closes) one file.
                if (kind != 0) o += 1;
                switch (kind) {
                    0 => _ = try statSize(r.child(&pb, EchoFs.names[rnd.uintLessThan(usize, 2)])),
                    1 => _ = try readWhole(r.child(&pb, "hello"), &buf),
                    2 => {
                        const rc = linux.open(r.child(&pb, "big"), .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
                        if (linux.errno(rc) != .SUCCESS) return error.OpenFailed;
                        const fd: linux.fd_t = @intCast(rc);
                        defer _ = linux.close(fd);
                        const off = rnd.uintLessThan(u64, EchoFs.big_len - buf.len);
                        const got = linux.pread(fd, &buf, buf.len, @intCast(off));
                        if (linux.errno(got) != .SUCCESS) return error.ReadFailed;
                        for (buf[0..got], 0..) |b, i| if (b != EchoFs.bigByte(@intCast(off + i))) return error.BigPatternMismatch;
                    },
                    else => _ = try writeWhole(r.child(&pb, "scratch"), buf[0 .. 1 + rnd.uintLessThan(usize, 4095)]),
                }
            }
            _ = ops.fetchAdd(n, .monotonic);
            _ = opens.fetchAdd(o, .monotonic);
        }
    };
    var ops = std.atomic.Value(u64).init(0);
    var opens = std.atomic.Value(u64).init(0);
    var results: [4]anyerror!void = .{ {}, {}, {}, {} };
    var threads: [4]std.Thread = undefined;
    for (&threads, 0..) |*t, i| t.* = try std.Thread.spawn(.{}, Storm.run, .{ rig, @as(u64, i + 1), &ops, &opens, &results[i] });
    for (threads) |t| t.join();
    for (results) |r| try r;
    // `close(2)` sends RELEASE as a background request: when the threads
    // return the kernel may not have delivered the last ones yet, so an
    // empty owed set is not "all answered" (the VM's `found 1`: the settle
    // read 0 before the last delivery, the assertion caught it owed). The
    // observable: every OPEN's RELEASE served, THEN the owed set empty —
    // past that no request can arrive.
    const Drained = struct {
        s: *Session,
        fs: *EchoFs,
        opens: u64,
        fn pred(d: *const @This()) bool {
            return d.fs.file_releases.load(.monotonic) == d.opens and d.s.owedCount() == 0;
        }
    };
    const drained: Drained = .{ .s = s, .fs = rig.fs, .opens = opens.load(.monotonic) };
    const joined_ns = clock.monotonicNs();
    // A hang guard, not a speed bound: the drain takes milliseconds even
    // under 24 × `yes` on kamrui (rung-10-owedstorm.md).
    if (!waitUntil(&drained, Drained.pred, 60 * std.time.ns_per_s)) {
        std.debug.print("\nstorm: {d} opens, {d} RELEASEs served, {d} owed after 60 s\n", .{ drained.opens, rig.fs.file_releases.load(.monotonic), s.owedCount() });
        describeOwed(s);
        return error.TestRequestNeverAnswered;
    }
    const drain_us = (clock.monotonicNs() - joined_ns) / std.time.ns_per_us;
    const after = Snapshot.take();
    try expectLaws(before, after);
    try testing.expectEqual(before.commit_lost_wakes, after.commit_lost_wakes);
    try testing.expectEqual(before.cqe_lost_wakes, after.cqe_lost_wakes);
    try testing.expectEqual(after.parked - before.parked, after.unparked - before.unparked);
    try testing.expect(after.demotes - before.demotes > 0);
    try testing.expectEqual(@as(u64, 0), slot.leases_outstanding.load());
    try testing.expectEqual(@as(u32, 0), s.owedCount());
    const reqs = after.requests - before.requests;
    const writes = after.writes - before.writes;
    const elided = after.elided - before.elided;
    std.debug.print("\nstorm: {d} syscalls, {d} ring requests, {d} enters ({d}.{d:0>2} per request), wake writes {d} / elided {d} ({d} % elided), backstop ticks {d}, lost commit wakes {d}, late commits {d}, lost CQE wakes {d}, late CQEs {d}; {d} RELEASEs drained {d} us after the join\n", .{ ops.load(.monotonic), reqs, after.enters - before.enters, (after.enters - before.enters) / @max(reqs, 1), ((after.enters - before.enters) * 100 / @max(reqs, 1)) % 100, writes, elided, elided * 100 / @max(writes + elided, 1), worker.park_backstop_ticks.load(), after.commit_lost_wakes - before.commit_lost_wakes, after.late_commits - before.late_commits, after.cqe_lost_wakes - before.cqe_lost_wakes, after.late_cqes - before.late_cqes, drained.opens, drain_us });
    try testing.expect(writes + elided >= after.replies - before.replies - (after.serves - before.serves));
}

test "e2e: a completion that lands after a backstop tick's timeout and before the worker's re-check (the seam holds the worker between the two) is a LATE arrival — the kernel's -ETIME says the CQ was empty when the wait gave up — counted transport_park_tick_late_cqes, never the lost-wake tripwire" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    const rig = try Rig.init(testing.allocator, 10, &.{});
    defer rig.deinit();
    const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
    var pb: [256]u8 = undefined;
    const Helper = struct {
        fn run(path: [*:0]const u8, result: *anyerror!usize) void {
            var buf: [64]u8 = undefined;
            result.* = readWhole(path, &buf);
        }
    };
    var result: anyerror!usize = 0;
    const t = try std.Thread.spawn(.{}, Helper.run, .{ rig.child(&pb, "park").ptr, &result });
    // The handler parks on its lane: its group owes a reply, so its worker
    // parks bounded by the backstop.
    const deadline = clock.monotonicNs() + 5 * std.time.ns_per_s;
    while (rig.fs.parked_readers.load(.monotonic) == 0 and clock.monotonicNs() < deadline) std.Thread.yield() catch {};
    try testing.expectEqual(@as(u32, 1), rig.fs.parked_readers.load(.monotonic));
    const before = Snapshot.take();
    // The next tick spends the seam and holds its worker before the reap.
    s.shared.tick_await_cqe_remaining.store(1, .release);
    while (s.shared.tick_await_cqe_remaining.load(.acquire) != 0 and clock.monotonicNs() < deadline + 5 * std.time.ns_per_s) std.Thread.yield() catch {};
    try testing.expectEqual(@as(u32, 0), s.shared.tick_await_cqe_remaining.load(.acquire));
    // The reply's wake READ completes now — after the tick's timeout,
    // before the worker's reap.
    rig.fs.releaseParked();
    t.join();
    _ = try result;
    const after = Snapshot.take();
    try testing.expect(after.ticks > before.ticks);
    try testing.expect(after.late_cqes > before.late_cqes);
    try testing.expectEqual(before.cqe_lost_wakes, after.cqe_lost_wakes);
    try testing.expectEqual(before.commit_lost_wakes, after.commit_lost_wakes);
}

test "e2e: a reply published after a backstop tick's reap and before the next pass's pump (the seam holds the worker between the two) is a LATE commit — its producer armed and wrote the wake, whose READ is still deferred — counted transport_park_tick_late_commits, never the lost-wake tripwire" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    // A cross-thread race: on the merged arm the handler publishes on the
    // worker's own thread and arms no coalescer (decision 0003 §AB.3), so
    // the pin runs on the handler-lane arm, where the race lives.
    const rig = try Rig.init(testing.allocator, 10, &.{.{ "SQUEEZEFS_FUSE_HANDLERS_ON_WORKER", "0" }});
    defer rig.deinit();
    const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
    var pb: [256]u8 = undefined;
    const Helper = struct {
        fn run(path: [*:0]const u8, result: *anyerror!usize) void {
            var buf: [64]u8 = undefined;
            result.* = readWhole(path, &buf);
        }
    };
    var result: anyerror!usize = 0;
    const t = try std.Thread.spawn(.{}, Helper.run, .{ rig.child(&pb, "park").ptr, &result });
    // The handler parks on its lane: its group owes a reply, so its worker
    // parks bounded by the backstop and ticks.
    const deadline = clock.monotonicNs() + 5 * std.time.ns_per_s;
    while (rig.fs.parked_readers.load(.monotonic) == 0 and clock.monotonicNs() < deadline) std.Thread.yield() catch {};
    try testing.expectEqual(@as(u32, 1), rig.fs.parked_readers.load(.monotonic));
    const before = Snapshot.take();
    // The pass after the next tick spends the seam and holds before its pump.
    s.shared.tick_await_publish_remaining.store(1, .release);
    while (s.shared.tick_await_publish_remaining.load(.acquire) != 0 and clock.monotonicNs() < deadline + 5 * std.time.ns_per_s) std.Thread.yield() catch {};
    try testing.expectEqual(@as(u32, 0), s.shared.tick_await_publish_remaining.load(.acquire));
    // The reply is published now: after the tick's reap, before the pump.
    rig.fs.releaseParked();
    t.join();
    _ = try result;
    const after = Snapshot.take();
    try testing.expect(after.ticks > before.ticks);
    try testing.expect(after.late_commits > before.late_commits);
    try testing.expectEqual(before.commit_lost_wakes, after.commit_lost_wakes);
    try testing.expectEqual(before.cqe_lost_wakes, after.cqe_lost_wakes);
}

test "e2e: the REGISTER refusal seam — the first 5 pushes are refused, back off 1 ms doubling, every ent is eventually registered, nothing retires, the mount arms and serves" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    const rig = try Rig.init(testing.allocator, 6, &.{.{ "SQUEEZEFS_TEST_FUSE_REGISTER_REFUSE", "5" }});
    defer rig.deinit();
    const before = Snapshot.take();
    const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
    var pb: [256]u8 = undefined;
    try testing.expectEqual(@as(u64, EchoFs.hello_text.len), try statSize(rig.child(&pb, "hello")));
    // The retries land within the first backoff rungs: wait for the
    // registered count to reach every ent (bounded by the ladder: ≤ 1 ms each).
    const want = @as(u64, s.plan.nqueues) * s.plan.depth;
    const deadline = clock.monotonicNs() + 5 * std.time.ns_per_s;
    while (worker.registers.load() - before.registers < want and clock.monotonicNs() < deadline) std.Thread.yield() catch {};
    const after = Snapshot.take();
    try testing.expectEqual(@as(u64, 5), after.refusals - before.refusals);
    try testing.expectEqual(before.retired, after.retired);
    try testing.expectEqual(want, after.registers - before.registers);
    try testing.expectEqual(s.plan.nqueues, s.queuesRegistered());
    try testing.expectEqual(@as(u64, 0), s.shared.register_refuse_remaining.load(.monotonic));
}

test "e2e: SQUEEZEFS_FUSE_SQPOLL=1 — the lever builds the ring on the SQPOLL ladder and the mount still serves" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    const rig = try Rig.init(testing.allocator, 7, &.{.{ "SQUEEZEFS_FUSE_SQPOLL", "1" }});
    defer rig.deinit();
    const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
    try testing.expect(s.shared.sqpoll);
    try testing.expect(s.groups[0].ring.ring.flags & linux.IORING_SETUP_SQPOLL != 0);
    var pb: [256]u8 = undefined;
    var small: [64]u8 = undefined;
    const n = try readWhole(rig.child(&pb, "hello"), &small);
    try testing.expectEqualSlices(u8, EchoFs.hello_text, small[0..n]);
}

test "e2e: notify frames ride the sideband — INVAL_INODE on a held inode succeeds, on an unknown inode counts ENOENT (never logged), PRUNE is accepted or counted failed on a pre-7.45 kernel" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    const rig = try Rig.init(testing.allocator, 8, &.{});
    defer rig.deinit();
    const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
    var pb: [256]u8 = undefined;
    _ = try statSize(rig.child(&pb, "hello"));
    const sent = notify_sent.load();
    const enoent = notify_enoent.load();
    const failed = notify_failed.load();
    try s.notifyInvalInode(EchoFs.ino_hello, -1, 0);
    try s.notifyInvalInode(777, 0, 0);
    try s.notifyPrune(&.{EchoFs.ino_hello});
    const deadline = clock.monotonicNs() + 5 * std.time.ns_per_s;
    while ((notify_sent.load() + notify_enoent.load() + notify_failed.load()) - (sent + enoent + failed) < 3 and clock.monotonicNs() < deadline) std.Thread.yield() catch {};
    try testing.expectEqual(@as(u64, 3), (notify_sent.load() + notify_enoent.load() + notify_failed.load()) - (sent + enoent + failed));
    try testing.expect(notify_sent.load() - sent >= 1);
    try testing.expect(notify_enoent.load() - enoent >= 1);
}

test "e2e lattice: every probe verdict on this kernel (kmbuf present with its pair, zc present with CapEff, retention present, TIME_LIMITS present); auto mounts ZeroCopy with width 1, retention armed, no user arena; the gauges publish" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    // Gated on the kmbuf ladder alone: on a kernel that has it, every other
    // verdict must read present too — this row is where a regressed probe of
    // the venue kernel fails loudly instead of skipping the rows that use it.
    var kb: [192]u8 = undefined;
    if (kmbufMissing(&kb)) |why| return testkit.skipWhy(@src(), why);
    const rig = try Rig.init(testing.allocator, 20, &.{});
    defer rig.deinit();
    const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
    const pair = s.verdicts.kmbuf.opcodes() orelse return error.KmbufAbsentOnTheVenue;
    std.debug.print("\nlattice: kmbuf {d}/{d}, zc {t}, retention {t}, time_limits {t} ⇒ {t}\n", .{ pair.register, pair.unregister, s.verdicts.zc, s.retention, s.time_limits, s.mode });
    try testing.expect((pair.register == 38 and pair.unregister == 39) or (pair.register == 37 and pair.unregister == 38));
    try testing.expectEqual(probes.Verdict.present, s.verdicts.zc);
    try testing.expectEqual(probes.Verdict.present, s.retention);
    try testing.expectEqual(probes.Verdict.present, s.time_limits);
    try testing.expect(s.init_reply.timeLimits());
    try testing.expectEqual(geometry.BufferMode.zero_copy, s.mode);
    try testing.expectEqual(@as(usize, 1), s.drain_groups.maxWidth());
    try testing.expect(s.shared.retention);
    try testing.expectEqual(@as(u64, 0), arena_bytes.load());
    try testing.expect(kmbuf.pinned_bytes.load() >= @as(u64, s.plan.nqueues) * zc.ringEntriesFor(s.plan.depth) * s.plan.payload_sz);
    try testing.expectEqual(@as(u64, 2), probes.mode_gauge.load());
    try testing.expectEqual(@as(u64, pair.register), probes.kmbuf_gauge.load());
    try testing.expectEqual(@as(u64, 1), probes.zc_gauge.load());
    try testing.expectEqual(@as(u64, 1), probes.retention_gauge.load());
    try testing.expectEqual(@as(u64, 1), probes.time_limits_gauge.load());
    try testing.expectEqual(@as(u64, 1), time_limits_gauge.load());
    try testing.expect(s.slab != null);
    try testing.expectEqual(s.slab.?.bytes(), zc_bounce_slab_bytes.load());
    // The reserve is virtual: the pages the slab holds are the staged
    // windows' (a fresh session has none; the component gauges them).
    try testing.expect(worker.zc_bounce_resident_bytes.load() < s.slab.?.bytes());
    try testing.expectEqual(worker.zc_bounce_resident_bytes.load(), s.slab_component.current());
    try testing.expect(s.slab_component.trim != null);
    try testing.expect(rig.fs.caps.?.buffer_mode == .zero_copy and rig.fs.caps.?.read_zc_serve and rig.fs.caps.?.retention);
    std.debug.print("lattice: kernel max_background (fusectl) {d}, negotiated {d}\n", .{ kernel_max_background.load(), s.plan.max_background });
}

test "e2e lattice: an explicit `bufring` mounts BufRing (no arena, every payload read at the kernel address through the CQE bid); `user` mounts UserEnts (an arena, no attachments); both serve byte-exact and close the laws" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    var kb: [192]u8 = undefined;
    const no_kmbuf = kmbufMissing(&kb);
    for ([_][]const u8{ "bufring", "user" }, 0..) |mode, i| {
        const want: geometry.BufferMode = if (std.mem.eql(u8, mode, "bufring")) .buf_ring else .user_ents;
        // UserEnts is judged on every kernel; the BufRing half needs kmbuf.
        if (want == .buf_ring and no_kmbuf != null) continue;
        const rig = try Rig.init(testing.allocator, 21 + @as(u32, @intCast(i)), &.{modeEnv(mode)});
        defer rig.deinit();
        const before = Snapshot.take();
        const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
        try testing.expectEqual(want, s.mode);
        try testing.expect(s.slab == null);
        var pb: [256]u8 = undefined;
        try testing.expectEqual(@as(u64, EchoFs.big_len), try statSize(rig.child(&pb, "big")));
        const big = try testing.allocator.alloc(u8, EchoFs.big_len);
        defer testing.allocator.free(big);
        try testing.expectEqual(EchoFs.big_len, try readWhole(rig.child(&pb, "big"), big));
        for (big, 0..) |b, j| if (b != EchoFs.bigByte(j)) return error.BigPatternMismatch;
        const data = "bufring bytes";
        try testing.expectEqual(data.len, try writeWhole(rig.child(&pb, "scratch"), data));
        try testing.expectEqualSlices(u8, data, rig.fs.scratch[0..data.len]);
        const after = Snapshot.take();
        try expectLaws(before, after);
        if (want == .buf_ring) {
            try testing.expectEqual(@as(u64, 0), arena_bytes.load());
            try testing.expect(after.attached - before.attached > 0);
            try testing.expectEqual(@as(usize, 1), s.drain_groups.maxWidth());
        } else {
            try testing.expect(arena_bytes.load() > 0);
            try testing.expectEqual(before.attached, after.attached);
        }
        try testing.expectEqual(before.bridges, after.bridges);
    }
    if (no_kmbuf) |why| return testkit.skipWhy(@src(), why);
}

test "e2e lattice: the stock stratum (the SQUEEZEFS_TEST_KMBUF_ABSENT seam, a stock distro kernel's verdicts on any venue) — `auto` mounts UserEnts with its arena, kmbuf/zc/retention read absent and publish 0; reads and writes serve byte-exact and the laws close; an explicit `bufring` and `zc` are refused loud naming kmbuf" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    const seam: [2][]const u8 = .{ "SQUEEZEFS_TEST_KMBUF_ABSENT", "1" };
    {
        const rig = try Rig.init(testing.allocator, 42, &.{seam});
        defer rig.deinit();
        const before = Snapshot.take();
        const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
        try testing.expectEqual(geometry.BufferMode.user_ents, s.mode);
        try testing.expect(s.verdicts.kmbuf == .absent);
        try testing.expectEqual(probes.Verdict.absent, s.verdicts.zc);
        try testing.expectEqual(probes.Verdict.absent, s.retention);
        try testing.expect(!s.shared.retention);
        try testing.expect(s.slab == null);
        try testing.expect(arena_bytes.load() > 0);
        try testing.expectEqual(@as(u64, 0), probes.mode_gauge.load());
        try testing.expectEqual(@as(u64, 0), probes.kmbuf_gauge.load());
        try testing.expectEqual(@as(u64, 0), probes.zc_gauge.load());
        try testing.expectEqual(@as(u64, 0), probes.retention_gauge.load());
        var pb: [256]u8 = undefined;
        const big = try testing.allocator.alloc(u8, EchoFs.big_len);
        defer testing.allocator.free(big);
        try testing.expectEqual(EchoFs.big_len, try readWhole(rig.child(&pb, "big"), big));
        for (big, 0..) |b, j| if (b != EchoFs.bigByte(j)) return error.BigPatternMismatch;
        const data = "stock stratum bytes";
        const w0 = worker.write_window_bytes.load();
        try testing.expectEqual(data.len, try writeWhole(rig.child(&pb, "scratch"), data));
        try testing.expectEqualSlices(u8, data, rig.fs.scratch[0..data.len]);
        // The non-zc term of the admission ledger counts the payload.
        try testing.expectEqual(w0 + data.len, worker.write_window_bytes.load());
        const after = Snapshot.take();
        try expectLaws(before, after);
        try testing.expectEqual(before.attached, after.attached);
        try testing.expectEqual(before.bridges, after.bridges);
    }
    for ([_][]const u8{ "bufring", "zc" }, 0..) |mode, i| {
        const rig = try Rig.init(testing.allocator, 43 + @as(u32, @intCast(i)), &.{ seam, modeEnv(mode) });
        defer rig.deinit();
        log.testing.expectErrorLogs(1);
        try testing.expectError(error.BufferModeRefused, rig.mountIt(.{ .mem_budget_bytes = 0 }));
        try log.testing.expectAllFired();
    }
}

test "e2e lattice: an explicit `zc` on a venue without CAP_SYS_ADMIN (the seam) fails the mount LOUD naming the probe; `auto` on the same seam settles on BufRing — the probe is the only degrade point" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    var kb: [192]u8 = undefined;
    if (kmbufMissing(&kb)) |why| return testkit.skipWhy(@src(), why);
    {
        const rig = try Rig.init(testing.allocator, 23, &.{ modeEnv("zc"), .{ "SQUEEZEFS_TEST_FUSE_CAP_SYS_ADMIN_ABSENT", "1" } });
        defer rig.deinit();
        log.testing.expectErrorLogs(1);
        try testing.expectError(error.BufferModeRefused, rig.mountIt(.{ .mem_budget_bytes = 0 }));
        try log.testing.expectAllFired();
    }
    {
        const rig = try Rig.init(testing.allocator, 24, &.{.{ "SQUEEZEFS_TEST_FUSE_CAP_SYS_ADMIN_ABSENT", "1" }});
        defer rig.deinit();
        const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
        try testing.expectEqual(geometry.BufferMode.buf_ring, s.mode);
        try testing.expectEqual(probes.Verdict.absent, s.verdicts.zc);
        try testing.expectEqual(@as(u64, 0), probes.cap_gauge.load());
    }
}

test "e2e lattice: the kmbuf ring answering ENOMEM after every retry (the seam) fails the mount with the TYPED KmbufRingEnomem refusal whose detail names the region's pages and the levers that shrink it — never an order-N contiguous shortage (the kernel falls back to order-0 pages); nothing stays pinned or mounted" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    var kb: [192]u8 = undefined;
    if (kmbufMissing(&kb)) |why| return testkit.skipWhy(@src(), why);
    const rig = try Rig.init(testing.allocator, 25, &.{.{ "SQUEEZEFS_TEST_KMBUF_REGISTER_ENOMEM", "1" }});
    defer rig.deinit();
    const pinned_before = kmbuf.pinned_bytes.load();
    const retries_before = kmbuf.register_retries.load();
    var refusal: Refusal = .none;
    // The worker thread logs kmbuf's one refusal line; this thread logs the
    // session's "group failed to start".
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.KmbufRingEnomem, rig.mountIt(.{ .mem_budget_bytes = 0, .refusal = &refusal, .test_error_logs = 1 }));
    try log.testing.expectAllFired();
    try testing.expect(refusal == .kmbuf_ring_enomem);
    var buf: [768]u8 = undefined;
    const text = try std.fmt.bufPrint(&buf, "{f}", .{refusal});
    // The precise levers: the region's pages, never an "order-N contiguous"
    // shortage (the kernel falls back to order-0 pages).
    const r = refusal.kmbuf_ring_enomem;
    try testing.expect(std.math.isPowerOfTwo(r.buffers) and r.buf_size % derive.facts().page_size == 0);
    try testing.expectEqual(r.pages, kmbuf.regionPages(r.buffers, r.buf_size, derive.facts().page_size));
    try testing.expect(std.mem.indexOf(u8, text, "contiguous") == null);
    for ([_][]const u8{ "pages at once", "SQUEEZEFS_FUSE_MAX_WRITE", "SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH", "SQUEEZEFS_FUSE_BUFFER_MODE=user" }) |lever| try testing.expect(std.mem.indexOf(u8, text, lever) != null);
    try testing.expectEqual(retries_before + kmbuf.REGISTER_RETRIES, kmbuf.register_retries.load());
    try testing.expectEqual(pinned_before, kmbuf.pinned_bytes.load());
    var mi_buf: [64 * 1024]u8 = undefined;
    const table = try Io.Dir.cwd().readFile(testing.io, "/proc/self/mountinfo", &mi_buf);
    try testing.expectEqual(@as(?u32, null), mountinfoMinor(table, rig.path));
}

test "e2e zc READ: /big moves through the fd-source leg from the fill pool (zc_serve_bytes ≡ user bytes, daemon copies 0, every reply rode the slot); with SQUEEZEFS_READ_ZC_SERVE=0 the bounce leg serves with ONE copy per READ" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    var kb: [192]u8 = undefined;
    if (kmbufMissing(&kb)) |why| return testkit.skipWhy(@src(), why);
    for ([_][]const u8{ "1", "0" }, 0..) |serve, i| {
        const rig = try Rig.init(testing.allocator, 25 + @as(u32, @intCast(i)), &.{.{ "SQUEEZEFS_READ_ZC_SERVE", serve }});
        defer rig.deinit();
        const before = Snapshot.take();
        const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
        try testing.expectEqual(geometry.BufferMode.zero_copy, s.mode);
        const on = serve[0] == '1';
        try testing.expectEqual(on, rig.fs.pool_fd >= 0);
        var pb: [256]u8 = undefined;
        const fd = try openPath(rig.child(&pb, "big"), .{ .ACCMODE = .RDONLY, .CLOEXEC = true });
        defer _ = linux.close(fd);
        var buf: [64 * 1024]u8 = undefined;
        var user_bytes: u64 = 0;
        var reads: u64 = 0;
        var off: u64 = 0;
        while (off < EchoFs.big_len) : (off += buf.len) {
            const n = try preadAt(fd, &buf, off);
            for (buf[0..n], 0..) |b, j| if (b != EchoFs.bigByte(@intCast(off + j))) return error.BigPatternMismatch;
            user_bytes += n;
            reads += 1;
        }
        // EOF: a zero-length paged reply is header-only, never a zero-length READ_FIXED.
        try testing.expectEqual(@as(usize, 0), try preadAt(fd, &buf, EchoFs.big_len));
        const after = Snapshot.take();
        try expectLaws(before, after);
        try testing.expectEqual(@as(u64, EchoFs.big_len), user_bytes);
        try testing.expectEqual(reads, after.zc_replies - before.zc_replies);
        if (on) {
            try testing.expectEqual(user_bytes, after.zc_serve_bytes - before.zc_serve_bytes);
            try testing.expectEqual(reads, after.fd_body_replies - before.fd_body_replies);
            try testing.expectEqual(@as(u64, 0), rig.fs.read_copies.load(.monotonic));
            try testing.expectEqual(before.zc_bounce_bytes, after.zc_bounce_bytes);
        } else {
            try testing.expectEqual(user_bytes, after.zc_bounce_bytes - before.zc_bounce_bytes);
            try testing.expectEqual(reads, rig.fs.read_copies.load(.monotonic));
            try testing.expectEqual(before.zc_serve_bytes, after.zc_serve_bytes);
            // Served in place into the bounce window: no worker memcpy.
            try testing.expectEqual(before.zc_bounce_copies, after.zc_bounce_copies);
        }
        try testing.expectEqual(before.fd_body_fallbacks, after.fd_body_fallbacks);
        try testing.expectEqual(@as(u64, 0), slot.leases_outstanding.load());
        std.debug.print("\nzc read (READ_ZC_SERVE={s}): {d} reads, {d} user bytes, zc_serve {d}, bounce {d}, daemon copies {d}, enters {d}\n", .{ serve, reads, user_bytes, after.zc_serve_bytes - before.zc_serve_bytes, after.zc_bounce_bytes - before.zc_bounce_bytes, rig.fs.read_copies.load(.monotonic), after.enters - before.enters });
    }
}

test "e2e read boundary ruler (SQUEEZEFS_TEST_READ_ZERO_FILL): under `bounce` every O_DIRECT ring READ is answered at its delivery with zeros — the filesystem's probe never serves it and its lane never sees it, each is a fast-dispatch serve whose body rode one bounce bridge (zc_bounce_bytes ≡ the replied bytes); under `prefilled` each is a header-only commit of the requested length with NO bridge; a buffered READ keeps the law under both; `off` serves the text" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    var kb: [192]u8 = undefined;
    if (kmbufMissing(&kb)) |why| return testkit.skipWhy(@src(), why);
    for ([_][]const u8{ "bounce", "prefilled", "off" }, 0..) |arm, i| {
        const rig = try Rig.init(testing.allocator, 48 + @as(u32, @intCast(i)), &.{.{ "SQUEEZEFS_TEST_READ_ZERO_FILL", arm }});
        defer rig.deinit();
        const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
        try testing.expectEqual(geometry.BufferMode.zero_copy, s.mode);
        const want = std.meta.stringToEnum(worker.ReadZeroFill, arm).?;
        try testing.expectEqual(want, s.shared.test_read_zero_fill);
        var pb: [256]u8 = undefined;
        // O_DIRECT: the kernel asks for the buffer (page-rounded), so a
        // reply's length is the app's count (no readahead, no page-cache fill).
        const fd = try openPath(rig.child(&pb, "hello"), .{ .ACCMODE = .RDONLY, .CLOEXEC = true, .DIRECT = true });
        defer _ = linux.close(fd);
        const before = Snapshot.take();
        const probes_before = rig.fs.probes_served.load(.monotonic);
        var buf: [4096]u8 align(4096) = @splat(0xA5);
        const n = try preadAt(fd, &buf, 0);
        const after = Snapshot.take();
        try expectLaws(before, after);
        const reads = after.reads - before.reads;
        try testing.expect(reads >= 1);
        switch (want) {
            .off => {
                try testing.expectEqual(EchoFs.hello_text.len, n);
                try testing.expectEqualSlices(u8, EchoFs.hello_text, buf[0..n]);
                try testing.expectEqual(reads, rig.fs.probes_served.load(.monotonic) - probes_before);
            },
            .bounce, .prefilled => {
                // The ruler answers the ASKED length (the kernel's page-rounded
                // direct read of a short file), never the file's: no
                // filesystem said how long it is.
                try testing.expectEqual(buf.len, n);
                // The filesystem never served the READ: no probe serve, no copy pass.
                try testing.expectEqual(probes_before, rig.fs.probes_served.load(.monotonic));
                try testing.expectEqual(@as(u64, 0), rig.fs.read_copies.load(.monotonic));
                try testing.expectEqual(reads, after.serves - before.serves);
                try testing.expectEqual(reads, after.zc_replies - before.zc_replies);
                if (want == .bounce) {
                    for (buf) |b| try testing.expectEqual(@as(u8, 0), b);
                    try testing.expectEqual(reads, after.bridges - before.bridges);
                    try testing.expectEqual(@as(u64, n), after.zc_bounce_bytes - before.zc_bounce_bytes);
                } else {
                    // No bridge, no bytes: the app's pages keep what they held.
                    for (buf) |b| try testing.expectEqual(@as(u8, 0xA5), b);
                    try testing.expectEqual(before.bridges, after.bridges);
                    try testing.expectEqual(before.zc_bounce_bytes, after.zc_bounce_bytes);
                }
                // A buffered READ of the same file keeps the law: the text.
                const bfd = try openPath(rig.child(&pb, "hello"), .{ .ACCMODE = .RDONLY, .CLOEXEC = true });
                defer _ = linux.close(bfd);
                var small: [64]u8 = undefined;
                const m = try preadAt(bfd, &small, 0);
                try testing.expectEqualSlices(u8, EchoFs.hello_text, small[0..m]);
                try testing.expect(rig.fs.probes_served.load(.monotonic) > probes_before);
            },
        }
        std.debug.print("\nread boundary ruler ({s}): {d} READ(s), {d} bytes, bridges {d}, enters {d}\n", .{ arm, reads, n, after.bridges - before.bridges, after.enters - before.enters });
    }
}

test "e2e zc WRITE: a W1-eligible aligned write is HELD in the slot and extracted lazily by the handler (one bridge, bytes land); an unaligned write is extracted at delivery; a session whose filesystem never declares eligibility extracts every paged write at delivery" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    var kb: [192]u8 = undefined;
    if (kmbufMissing(&kb)) |why| return testkit.skipWhy(@src(), why);
    const rig = try Rig.init(testing.allocator, 27, &.{});
    defer rig.deinit();
    rig.fs.hold_eligible = true;
    const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
    try testing.expectEqual(geometry.BufferMode.zero_copy, s.mode);
    const before = Snapshot.take();
    var pb: [256]u8 = undefined;
    const fd = try openPath(rig.child(&pb, "scratch"), .{ .ACCMODE = .WRONLY, .CLOEXEC = true });
    defer _ = linux.close(fd);
    // Aligned 4 KiB at offset 4096: the W1 shape ⇒ HELD ⇒ lazy extraction.
    var page: [4096]u8 = undefined;
    for (&page, 0..) |*b, i| b.* = @truncate(i * 3 + 1);
    try testing.expectEqual(@as(usize, 4096), linux.pwrite(fd, &page, page.len, 4096));
    try testing.expectEqual(@as(u64, 1), rig.fs.held_writes.load(.monotonic));
    try testing.expectEqual(@as(u64, 1), rig.fs.lazy_extracts.load(.monotonic));
    try testing.expectEqualSlices(u8, &page, rig.fs.scratch[4096..8192]);
    const mid = Snapshot.take();
    try testing.expectEqual(@as(u64, 1), mid.holds - before.holds);
    try testing.expectEqual(@as(u64, 1), mid.lazy - before.lazy);
    try testing.expectEqual(@as(u64, 1), mid.extractions - before.extractions);
    // Unaligned: not a hold candidate ⇒ extracted at delivery.
    const tail = "unaligned tail";
    try testing.expectEqual(@as(usize, tail.len), linux.pwrite(fd, tail, tail.len, 8192 + 7));
    try testing.expectEqualSlices(u8, tail, rig.fs.scratch[8192 + 7 ..][0..tail.len]);
    const after = Snapshot.take();
    try expectLaws(before, after);
    try testing.expectEqual(@as(u64, 1), after.holds - before.holds);
    try testing.expectEqual(@as(u64, 2), after.extractions - before.extractions);
    try testing.expectEqual(@as(u64, 1), after.lazy - before.lazy);
    try testing.expectEqual(@as(u64, 2), rig.fs.writes_seen.load(.monotonic));
    try testing.expectEqual(@as(u64, 0), slot.leases_outstanding.load());
    try testing.expectEqual(@as(u8, @backingInt(zc.PayloadClass.page_cache)), rig.fs.last_write_class.load(.acquire));
    // A filesystem that never declares eligibility: the same aligned write is extracted at delivery.
    rig.fs.hold_eligible = false;
    try testing.expectEqual(@as(usize, 4096), linux.pwrite(fd, &page, page.len, 12288));
    try testing.expectEqualSlices(u8, &page, rig.fs.scratch[12288..16384]);
    const last = Snapshot.take();
    try testing.expectEqual(after.holds, last.holds);
    try testing.expectEqual(@as(u64, 1), last.extractions - after.extractions);
}

/// `EchoFs.HeldStore.store` over the session: the product's `HeldStore`
/// face in `cli/mount.zig` is this thunk.
fn storeHeldThunk(ctx: *anyopaque, src: slot.HeldSource, fd: linux.fd_t, off: u64, io: Io) slot.ExtractError!void {
    const s: *Session = @ptrCast(@alignCast(ctx));
    return s.storeHeld(src, fd, off, io);
}

test "e2e zc WRITE, the overlay class (charter §6.2 item 1): a 1 MiB page-aligned write on a zero-copy session is HELD in its slot and lands by WRITE_FIXED(fd ← slot) straight from the app's pages — the bytes byte-exact in the fd, directs +1 for 1 MiB, extractions +0 (zero daemon copies); a 1 MiB write that straddles the block edge and an unaligned write are extracted at delivery; a filesystem that declines the shape extracts the same 1 MiB at delivery" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    var kb: [192]u8 = undefined;
    if (kmbufMissing(&kb)) |why| return testkit.skipWhy(@src(), why);
    const rig = try Rig.init(testing.allocator, 29, &.{});
    defer rig.deinit();
    // The device stand-in: a memfd the held slot DMAs into.
    const dev_rc = linux.memfd_create("sqz-zc-write-dev", linux.MFD.CLOEXEC);
    try testing.expect(linux.errno(dev_rc) == .SUCCESS);
    const dev_fd: linux.fd_t = @intCast(dev_rc);
    defer _ = linux.close(dev_fd);
    try testing.expect(linux.errno(linux.ftruncate(dev_fd, 8 << 20)) == .SUCCESS);
    rig.fs.hold_eligible = true;
    const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
    try testing.expectEqual(geometry.BufferMode.zero_copy, s.mode);
    rig.fs.held_store = .{ .ctx = s, .fd = dev_fd, .store = &storeHeldThunk };
    const before = Snapshot.take();
    var pb: [256]u8 = undefined;
    const fd = try openPath(rig.child(&pb, "scratch"), .{ .ACCMODE = .WRONLY, .CLOEXEC = true });
    defer _ = linux.close(fd);
    const mib: usize = 1 << 20;
    const buf = try testing.allocator.alignedAlloc(u8, .fromByteUnits(4096), mib);
    defer testing.allocator.free(buf);
    for (buf, 0..) |*b, i| b.* = @truncate((i * 7) ^ (i >> 12));
    // 1 MiB at offset 0 of a 4 MiB block: the overlay shape ⇒ HELD ⇒ the direct store.
    try testing.expectEqual(mib, linux.pwrite(fd, buf.ptr, mib, 0));
    try testing.expectEqual(@as(u64, 1), rig.fs.held_writes.load(.monotonic));
    try testing.expectEqual(@as(u64, 1), rig.fs.direct_stores.load(.monotonic));
    try testing.expectEqual(@as(u64, 0), rig.fs.direct_store_failures.load(.monotonic));
    try testing.expectEqual(@as(u64, 0), rig.fs.lazy_extracts.load(.monotonic));
    const got = try testing.allocator.alloc(u8, mib);
    defer testing.allocator.free(got);
    try testing.expectEqual(mib, try preadAt(dev_fd, got, 0));
    try testing.expectEqualSlices(u8, buf, got);
    const mid = Snapshot.take();
    try testing.expectEqual(@as(u64, 1), mid.holds - before.holds);
    try testing.expectEqual(@as(u64, mib), mid.hold_bytes - before.hold_bytes);
    try testing.expectEqual(@as(u64, 1), mid.directs - before.directs);
    try testing.expectEqual(@as(u64, mib), mid.direct_bytes - before.direct_bytes);
    try testing.expectEqual(@as(u64, 0), mid.extractions - before.extractions);
    try testing.expectEqual(@as(u64, 0), mid.direct_failures - before.direct_failures);
    try testing.expectEqual(@as(u64, 0), mid.direct_refusals - before.direct_refusals);
    // The held write's three phases each sampled the one direct store (delivery → store pushed →
    // its CQE → the commit pushed): the latency split's instrument.
    try testing.expectEqual(@as(u64, 1), mid.phase_to_store - before.phase_to_store);
    try testing.expectEqual(@as(u64, 1), mid.phase_store - before.phase_store);
    try testing.expectEqual(@as(u64, 1), mid.phase_to_commit - before.phase_to_commit);
    // 1 MiB at 3.5 MiB straddles the 4 MiB block edge: two pieces ⇒ extracted at delivery (EFBIG from
    // the scratch array is the echo filesystem's answer, the counters the transport's).
    _ = linux.pwrite(fd, buf.ptr, mib, (3 << 20) + (512 << 10));
    const unaligned = "unaligned tail";
    try testing.expectEqual(@as(usize, unaligned.len), linux.pwrite(fd, unaligned, unaligned.len, 8192 + 7));
    try testing.expectEqualSlices(u8, unaligned, rig.fs.scratch[8192 + 7 ..][0..unaligned.len]);
    const after = Snapshot.take();
    try testing.expectEqual(@as(u64, 1), after.holds - before.holds);
    try testing.expectEqual(@as(u64, 2), after.extractions - before.extractions);
    try testing.expectEqual(@as(u64, 0), after.lazy - before.lazy);
    // The filesystem declines the shape: the same 1 MiB is extracted at delivery, never held.
    rig.fs.hold_eligible = false;
    _ = linux.pwrite(fd, buf.ptr, mib, 0);
    const last = Snapshot.take();
    try testing.expectEqual(after.holds, last.holds);
    try testing.expectEqual(@as(u64, 1), last.extractions - after.extractions);
    try expectLaws(before, last);
    try testing.expectEqual(@as(u64, 0), slot.leases_outstanding.load());
}

test "e2e zc bridge in PIECES (rung-10-seq-parity): under a 256 KiB NOWAIT ceiling (SQUEEZEFS_TEST_ZC_BRIDGE_CEILING) a 1 MiB held write's direct store is FOUR WRITE_FIXED SQEs under one tag — the bytes byte-exact in the fd, directs +1, splits +1, pieces +4, every phase sampled once; a 4 KiB store stays one SQE; a short piece fails the whole bridge (the device stand-in truncated under the DMA) and the write still answers" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    var kb: [192]u8 = undefined;
    if (kmbufMissing(&kb)) |why| return testkit.skipWhy(@src(), why);
    const rig = try Rig.init(testing.allocator, 31, &.{.{ "SQUEEZEFS_TEST_ZC_BRIDGE_CEILING", "262144" }});
    defer rig.deinit();
    const dev_rc = linux.memfd_create("sqz-zc-pieces-dev", linux.MFD.CLOEXEC | linux.MFD.ALLOW_SEALING);
    try testing.expect(linux.errno(dev_rc) == .SUCCESS);
    const dev_fd: linux.fd_t = @intCast(dev_rc);
    defer _ = linux.close(dev_fd);
    try testing.expect(linux.errno(linux.ftruncate(dev_fd, 8 << 20)) == .SUCCESS);
    rig.fs.hold_eligible = true;
    const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
    try testing.expectEqual(geometry.BufferMode.zero_copy, s.mode);
    rig.fs.held_store = .{ .ctx = s, .fd = dev_fd, .store = &storeHeldThunk };
    const before = Snapshot.take();
    const splits0 = worker.zc_bridge_splits.load();
    const pieces0 = worker.zc_bridge_pieces.load();
    var pb: [256]u8 = undefined;
    const fd = try openPath(rig.child(&pb, "scratch"), .{ .ACCMODE = .WRONLY, .CLOEXEC = true });
    defer _ = linux.close(fd);
    const mib: usize = 1 << 20;
    const buf = try testing.allocator.alignedAlloc(u8, .fromByteUnits(4096), mib);
    defer testing.allocator.free(buf);
    for (buf, 0..) |*b, i| b.* = @truncate((i * 13) ^ (i >> 10));
    try testing.expectEqual(mib, linux.pwrite(fd, buf.ptr, mib, 0));
    const got = try testing.allocator.alloc(u8, mib);
    defer testing.allocator.free(got);
    try testing.expectEqual(mib, try preadAt(dev_fd, got, 0));
    try testing.expectEqualSlices(u8, buf, got);
    const mid = Snapshot.take();
    try testing.expectEqual(@as(u64, 1), mid.holds - before.holds);
    try testing.expectEqual(@as(u64, 1), mid.directs - before.directs);
    try testing.expectEqual(@as(u64, mib), mid.direct_bytes - before.direct_bytes);
    try testing.expectEqual(@as(u64, 0), mid.direct_failures - before.direct_failures);
    try testing.expectEqual(@as(u64, 1), worker.zc_bridge_splits.load() - splits0);
    try testing.expectEqual(@as(u64, 4), worker.zc_bridge_pieces.load() - pieces0);
    try testing.expectEqual(@as(u64, 1), mid.phase_to_store - before.phase_to_store);
    try testing.expectEqual(@as(u64, 1), mid.phase_store - before.phase_store);
    try testing.expectEqual(@as(u64, 1), mid.phase_to_commit - before.phase_to_commit);
    // A page-sized held write (the W1 class) is one SQE: no split.
    try testing.expectEqual(@as(usize, 4096), linux.pwrite(fd, buf.ptr, 4096, 4096));
    try testing.expectEqual(@as(u64, 1), worker.zc_bridge_splits.load() - splits0);
    try testing.expectEqual(@as(u64, 2), worker.zc_write_directs.load() - before.directs);
    // The stand-in is sealed at 2.5 pieces into the next 1 MiB: the third piece lands short, the
    // fourth past the seal (EPERM) — ONE failed bridge (direct failures +1, never a torn success), and
    // the handler's fallback answers the write (the echo filesystem's scratch is 64 KiB: EFBIG).
    try testing.expect(linux.errno(linux.ftruncate(dev_fd, (2 << 20) + (640 << 10))) == .SUCCESS);
    try testing.expect(linux.errno(linux.fcntl(dev_fd, linux.F.ADD_SEALS, linux.F.SEAL_GROW)) == .SUCCESS);
    _ = linux.pwrite(fd, buf.ptr, mib, 2 << 20);
    try testing.expectEqual(@as(u64, 1), rig.fs.direct_store_failures.load(.monotonic));
    const after = Snapshot.take();
    try testing.expectEqual(@as(u64, 2), worker.zc_bridge_splits.load() - splits0);
    try testing.expectEqual(@as(u64, 1), after.direct_failures - before.direct_failures);
    try testing.expectEqual(@as(u64, 2), after.directs - before.directs);
    try expectLaws(before, after);
    try testing.expectEqual(@as(u64, 0), slot.leases_outstanding.load());
}

test "e2e zc WRITE on the worker's lane (decision 0003 §AB.3, the write arm): a HELD write's handler runs on the thread that delivered it — dispatched `own`, the handler's thread a drain worker's, its extraction bridge round-trips on that worker's own ring and its reply commits with NO cross-thread wake; an extracted-at-delivery write stays the data class (foreign, off every worker); the control arm (SQUEEZEFS_FUSE_HANDLERS_ON_WORKER=0) crosses a thread each way" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    var kb: [192]u8 = undefined;
    if (kmbufMissing(&kb)) |why| return testkit.skipWhy(@src(), why);
    const Helper = struct {
        fn onWorker(r: *Rig, s: *Session) bool {
            const tid = r.fs.handler_tid.load(.acquire);
            for (s.groups) |g| if (g.tid.load(.acquire) == tid) return true;
            return false;
        }
        fn dispatches(s: *Session) struct { own: u64, foreign: u64 } {
            var own: u64 = 0;
            var foreign: u64 = 0;
            for (s.groups) |g| {
                own += g.dispatches_own.load(.monotonic);
                foreign += g.dispatches_foreign.load(.monotonic);
            }
            return .{ .own = own, .foreign = foreign };
        }
    };
    var page: [4096]u8 = undefined;
    for (&page, 0..) |*b, i| b.* = @truncate(i * 7 + 3);
    {
        const rig = try Rig.init(testing.allocator, 44, &.{});
        defer rig.deinit();
        rig.fs.hold_eligible = true;
        const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
        try testing.expectEqual(geometry.BufferMode.zero_copy, s.mode);
        var pb: [256]u8 = undefined;
        const fd = try openPath(rig.child(&pb, "scratch"), .{ .ACCMODE = .WRONLY, .CLOEXEC = true });
        defer _ = linux.close(fd);
        // The W1 shape ⇒ HELD ⇒ the worker's own lane: the handler's lazy
        // extraction is a bridge on the delivering worker's ring (its wake
        // elided, its CQE that worker's reap) and the reply commits from
        // the same thread — not one eventfd write to a worker.
        const d0 = Helper.dispatches(s);
        const w0 = slot.wake_writes.load();
        const spills0 = worker.write_spills.load();
        const held0 = rig.fs.held_writes.load(.monotonic);
        var i: usize = 0;
        while (i < 16) : (i += 1) {
            try testing.expectEqual(@as(usize, 4096), linux.pwrite(fd, &page, page.len, 4096));
            try testing.expect(Helper.onWorker(rig, s));
        }
        try testing.expectEqual(held0 + 16, rig.fs.held_writes.load(.monotonic));
        try testing.expectEqualSlices(u8, &page, rig.fs.scratch[4096..8192]);
        const d1 = Helper.dispatches(s);
        try testing.expectEqual(d0.own + 16, d1.own);
        try testing.expectEqual(d0.foreign, d1.foreign);
        // One write in flight at a time: the lane always has room.
        try testing.expectEqual(spills0, worker.write_spills.load());
        try testing.expectEqual(w0, slot.wake_writes.load());
        // Unaligned ⇒ extracted at delivery ⇒ the data class: the CPU's lane.
        const tail = "off the worker";
        try testing.expectEqual(@as(usize, tail.len), linux.pwrite(fd, tail, tail.len, 8192 + 7));
        try testing.expectEqualSlices(u8, tail, rig.fs.scratch[8192 + 7 ..][0..tail.len]);
        const d2 = Helper.dispatches(s);
        try testing.expectEqual(d1.own, d2.own);
        try testing.expectEqual(d1.foreign + 1, d2.foreign);
        try testing.expect(!Helper.onWorker(rig, s));
        try testing.expectEqual(@as(u64, 0), slot.leases_outstanding.load());
    }
    {
        const rig = try Rig.init(testing.allocator, 45, &.{.{ "SQUEEZEFS_FUSE_HANDLERS_ON_WORKER", "0" }});
        defer rig.deinit();
        rig.fs.hold_eligible = true;
        const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
        try testing.expectEqual(geometry.BufferMode.zero_copy, s.mode);
        var pb: [256]u8 = undefined;
        const fd = try openPath(rig.child(&pb, "scratch"), .{ .ACCMODE = .WRONLY, .CLOEXEC = true });
        defer _ = linux.close(fd);
        const w0 = slot.wake_writes.load();
        const held0 = rig.fs.held_writes.load(.monotonic);
        var i: usize = 0;
        while (i < 16) : (i += 1) {
            try testing.expectEqual(@as(usize, 4096), linux.pwrite(fd, &page, page.len, 4096));
            try testing.expect(!Helper.onWorker(rig, s));
        }
        try testing.expectEqual(held0 + 16, rig.fs.held_writes.load(.monotonic));
        try testing.expectEqual(@as(u64, 0), Helper.dispatches(s).own);
        // The lane thread's extraction requests and replies wake the worker across threads.
        try testing.expect(slot.wake_writes.load() > w0);
    }
}

test "e2e the owed spin (rung-10-workers-per-queue, the write row): a worker whose ring owes a held write's bridge parks for the first one (the slept predictor's seed), PROBES the next (a spin to the slept bound that catches the completion: the caught predictor's seed) and then spins by the competitive rule — over 64 QD1 held writes the ledger reads hits ≥ 1 and closes (spins ≡ hits + falls), both predictors hold samples and the gauges read the rule's inputs; a pass owing no bridge consults the rule never (the skips stay at the owed parks')" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    var kb: [192]u8 = undefined;
    if (kmbufMissing(&kb)) |why| return testkit.skipWhy(@src(), why);
    const rig = try Rig.init(testing.allocator, 47, &.{});
    defer rig.deinit();
    rig.fs.hold_eligible = true;
    const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
    try testing.expectEqual(geometry.BufferMode.zero_copy, s.mode);
    var pb: [256]u8 = undefined;
    const fd = try openPath(rig.child(&pb, "scratch"), .{ .ACCMODE = .WRONLY, .CLOEXEC = true });
    defer _ = linux.close(fd);
    var page: [4096]u8 = undefined;
    for (&page, 0..) |*b, i| b.* = @truncate(i * 5 + 1);
    const spins0 = worker.owed_spins.load();
    const hits0 = worker.owed_spin_hits.load();
    const falls0 = worker.owed_spin_falls.load();
    const skips0 = worker.owed_spin_skips.load();
    const ns0 = worker.owed_spin_ns.load();
    // Every write is the W1 shape ⇒ HELD ⇒ its lazy extraction is a bridge
    // on the delivering worker's own ring: the pass that pushed it owes a
    // device completion with nothing else in hand — the owed park.
    var i: usize = 0;
    while (i < 64) : (i += 1) {
        try testing.expectEqual(@as(usize, 4096), linux.pwrite(fd, &page, page.len, 4096));
    }
    try testing.expectEqualSlices(u8, &page, rig.fs.scratch[4096..8192]);
    try testing.expectEqual(@as(u64, 64), rig.fs.lazy_extracts.load(.monotonic));
    const spins = worker.owed_spins.load() - spins0;
    const hits = worker.owed_spin_hits.load() - hits0;
    const falls = worker.owed_spin_falls.load() - falls0;
    const skips = worker.owed_spin_skips.load() - skips0;
    // The first owed park measured (no spin: a skip); the probe caught a
    // completion; every spin ended as a hit or a fall; every one of the 64
    // owed parks was ruled on (a request arriving mid-bridge rules again).
    try testing.expect(skips >= 1);
    try testing.expect(hits >= 1);
    try testing.expectEqual(spins, hits + falls);
    try testing.expect(spins + skips >= 64);
    try testing.expect(worker.owed_spin_ns.load() > ns0);
    var slept_samples: u64 = 0;
    var awake_samples: u64 = 0;
    for (s.groups) |g| {
        slept_samples += g.dev_slept.samples.load(.monotonic);
        awake_samples += g.dev_awake.samples.load(.monotonic);
    }
    try testing.expect(slept_samples >= 1);
    try testing.expect(awake_samples >= 1);
    var reading: worker.Group.SpinReading = .{};
    for (s.groups) |g| {
        const r = g.spinReading();
        if (r.caught > reading.caught) reading = r;
    }
    try testing.expect(reading.dev_ns > 0);
    std.debug.print("\nowed spin: {d} writes, {d} spins ({d} hits, {d} falls), {d} skips, {d} µs spun, dev {d} µs, wake {d} µs\n", .{ i, spins, hits, falls, skips, (worker.owed_spin_ns.load() - ns0) / std.time.ns_per_us, reading.dev_ns / std.time.ns_per_us, reading.wake_ns / std.time.ns_per_us });
}

/// `depth` LOOKUPs of distinct park names from threads pinned to ONE CPU:
/// the kernel queues each on that CPU's queue, so every entry of every
/// ring registered there ends up in userspace, parked. Returns the CPU.
const PinnedParkers = struct {
    threads: []std.Thread,
    results: []anyerror!u64,
    names: [][96]u8,

    fn start(gpa: std.mem.Allocator, rig: *Rig, n: usize, cpu: u16) !PinnedParkers {
        const self: PinnedParkers = .{ .threads = try gpa.alloc(std.Thread, n), .results = try gpa.alloc(anyerror!u64, n), .names = try gpa.alloc([96]u8, n) };
        for (0..n) |i| {
            const path = std.fmt.bufPrintSentinel(&self.names[i], "{s}/{s}-{d}", .{ rig.path, EchoFs.park_lookup_name, i }, 0) catch unreachable;
            self.results[i] = 0;
            self.threads[i] = try std.Thread.spawn(.{}, run, .{ path.ptr, cpu, &self.results[i] });
        }
        return self;
    }

    fn run(path: [*:0]const u8, cpu: u16, result: *anyerror!u64) void {
        var set: linux.cpu_set_t = @splat(0);
        set[cpu / @bitSizeOf(usize)] |= @as(usize, 1) << @intCast(cpu % @bitSizeOf(usize));
        linux.sched_setaffinity(0, &set) catch {};
        result.* = statSize(path);
    }

    /// Join every parker; each stat failed (the released lookup answers
    /// negative — ENOENT).
    fn joinAll(self: *PinnedParkers) !void {
        for (self.threads) |t| t.join();
        for (self.results) |r| try testing.expectError(error.StatFailed, r);
    }

    fn deinit(self: *PinnedParkers, gpa: std.mem.Allocator) void {
        gpa.free(self.threads);
        gpa.free(self.results);
        gpa.free(self.names);
    }
};

/// The first CPU this process may run on (the queue the pinned threads hit).
fn firstAllowedCpu() u16 {
    var set: linux.cpu_set_t = @splat(0);
    if (linux.errno(linux.sched_getaffinity(linux.getpid(), @sizeOf(linux.cpu_set_t), &set)) != .SUCCESS) return 0;
    for (set, 0..) |word, w| if (word != 0) return @intCast(w * @bitSizeOf(usize) + @ctz(word));
    return 0;
}

fn waitParkedLookups(rig: *Rig, n: u32) !void {
    const deadline = clock.monotonicNs() + 10 * std.time.ns_per_s;
    while (rig.fs.parked_lookups.load(.monotonic) < n and clock.monotonicNs() < deadline) std.Thread.yield() catch {};
    try testing.expectEqual(n, rig.fs.parked_lookups.load(.monotonic));
}

test "e2e rings per queue (series patch 0030): the FEATURES probe reads RINGS_PER_QUEUE present, under SQUEEZEFS_FUSE_RINGS_PER_QUEUE=2 the session registers TWO rings on every queue (ceil(depth/2) + the rest, both workers named sqz-fuse-ur<qid>), and `depth` requests from one CPU fill both rings' entries — each ring owes exactly its share; the control arm (SQUEEZEFS_FUSE_RINGS_PER_QUEUE=1) owes the whole depth on its one ring; .stats carries fuse_uring_rings_per_queue" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    var kb: [192]u8 = undefined;
    if (kmbufMissing(&kb)) |why| return testkit.skipWhy(@src(), why);
    const cpu = firstAllowedCpu();
    {
        // The lever set: two rings are the A/B arm until the two-ring daemon holds the gate (geometry.ringsPerQueue).
        const rig = try Rig.init(testing.allocator, 46, &.{.{ "SQUEEZEFS_FUSE_RINGS_PER_QUEUE", "2" }});
        defer rig.deinit();
        const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
        try testing.expectEqual(geometry.BufferMode.zero_copy, s.mode);
        if (s.rings_feature != .present) return testkit.skip(@src(), .no_kernel_feature, "FUSE_IO_URING_CMD_FEATURES reads FUSE_URING_FEAT_RINGS_PER_QUEUE absent (the fuse-io-uring several-rings-per-queue patch, sqz series 0030)");
        try testing.expectEqual(@as(u32, 2), s.rings_per_queue);
        try testing.expectEqual(@as(u64, 1), probes.rings_per_queue_gauge.load());
        try testing.expectEqual(@as(u64, 2), geometry.gauges.rings_per_queue.load());
        try testing.expectEqual(s.drain_groups.count() * 2, s.groups.len);
        try testing.expectEqual(s.plan.nqueues, s.queuesRegistered());
        const g0 = s.groups[s.qid_group[cpu]];
        const g1 = s.groups[s.qid_group[cpu] + 1];
        const share0 = geometry.ringShare(s.plan.depth, 2, 0);
        const share1 = geometry.ringShare(s.plan.depth, 2, 1);
        try testing.expectEqual(share0.depth, g0.depth);
        try testing.expectEqual(@as(u16, 0), g0.ring_base);
        try testing.expectEqual(share1.depth, g1.depth);
        try testing.expectEqual(share1.base, g1.ring_base);
        try testing.expectEqual(s.plan.depth, g0.depth + g1.depth);
        try testing.expect(g0.ownsSlot(0) and !g1.ownsSlot(0) and g1.ownsSlot(share1.base) and !g0.ownsSlot(share1.base));
        // Both workers carry the class name (the CPU accounting contract).
        var expect_name: [24]u8 = undefined;
        const want = try std.fmt.bufPrint(&expect_name, "sqz-fuse-ur{d}", .{cpu});
        try testing.expectEqualStrings(want, std.mem.sliceTo(&g0.name_buf, 0));
        try testing.expectEqualStrings(want, std.mem.sliceTo(&g1.name_buf, 0));
        // `depth` parked LOOKUPs from one CPU: every entry of both rings in
        // userspace (the kernel's LIFO pick spills to the second ring only
        // once the first's are all out).
        var parkers = try PinnedParkers.start(testing.allocator, rig, s.plan.depth, cpu);
        defer parkers.deinit(testing.allocator);
        try waitParkedLookups(rig, s.plan.depth);
        try testing.expectEqual(share0.depth, g0.owedCount());
        try testing.expectEqual(share1.depth, g1.owedCount());
        try testing.expect(g1.dispatches_own.load(.monotonic) >= share1.depth);
        var sb: std.Io.Writer.Allocating = .init(testing.allocator);
        defer sb.deinit();
        try s.writeStatsJson(&sb.writer);
        try testing.expect(std.mem.indexOf(u8, sb.written(), "\"fuse_uring_rings_per_queue\":2,") != null);
        try testing.expect(std.mem.indexOf(u8, sb.written(), "\"transport_probe_rings_per_queue\":1,") != null);
        rig.fs.releaseParked();
        try parkers.joinAll();
        try testing.expectEqual(@as(u32, 0), s.owedCount());
    }
    {
        const rig = try Rig.init(testing.allocator, 47, &.{.{ "SQUEEZEFS_FUSE_RINGS_PER_QUEUE", "1" }});
        defer rig.deinit();
        const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
        try testing.expectEqual(@as(u32, 1), s.rings_per_queue);
        try testing.expectEqual(s.drain_groups.count(), s.groups.len);
        const g = s.groups[s.qid_group[cpu]];
        try testing.expectEqual(s.plan.depth, g.depth);
        var parkers = try PinnedParkers.start(testing.allocator, rig, s.plan.depth, cpu);
        defer parkers.deinit(testing.allocator);
        try waitParkedLookups(rig, s.plan.depth);
        try testing.expectEqual(s.plan.depth, g.owedCount());
        rig.fs.releaseParked();
        try parkers.joinAll();
        var sb: std.Io.Writer.Allocating = .init(testing.allocator);
        defer sb.deinit();
        try s.writeStatsJson(&sb.writer);
        try testing.expect(std.mem.indexOf(u8, sb.written(), "\"fuse_uring_rings_per_queue\":1,") != null);
    }
}

test "e2e zc bridges: a bridge that never completes (the stall seam) is cancelled at the deadline — bridge_cancels == 1, the slot still commits header-only -EIO (THE LAW: the reader gets EIO), lost/orphans stay 0, the session serves on" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    var kb: [192]u8 = undefined;
    if (kmbufMissing(&kb)) |why| return testkit.skipWhy(@src(), why);
    const rig = try Rig.init(testing.allocator, 28, &.{ .{ "SQUEEZEFS_TEST_ZC_BRIDGE_STALL", "1" }, .{ "SQUEEZEFS_TEST_ZC_BRIDGE_DEADLINE_MS", "300" } });
    defer rig.deinit();
    // The tripwire's log line and the deadline scan's line.
    rig.fs.probe_declines = true;
    const s = try rig.mountIt(.{ .mem_budget_bytes = 0, .test_error_logs = 2 });
    try testing.expectEqual(geometry.BufferMode.zero_copy, s.mode);
    try testing.expectEqual(300 * std.time.ns_per_ms, s.shared.bridge_deadline_ns);
    const before = Snapshot.take();
    var pb: [256]u8 = undefined;
    const fd = try openPath(rig.child(&pb, "hello"), .{ .ACCMODE = .RDONLY, .CLOEXEC = true });
    defer _ = linux.close(fd);
    var buf: [64]u8 = undefined;
    const t0 = clock.monotonicNs();
    // The first paged READ's bridge reads the writer-less pipe: it never
    // completes; the deadline cancels it; the reader sees EIO.
    try testing.expectError(error.ReadFailed, preadAt(fd, &buf, 0));
    const waited = clock.monotonicNs() - t0;
    try testing.expect(waited >= 300 * std.time.ns_per_ms);
    const mid = Snapshot.take();
    try testing.expectEqual(@as(u64, 1), mid.bridge_cancels - before.bridge_cancels);
    try testing.expectEqual(before.bridge_lost, mid.bridge_lost);
    try testing.expectEqual(before.bridge_orphans, mid.bridge_orphans);
    try testing.expectEqual(before.abandoned, mid.abandoned);
    // The seam is spent: the next READ serves normally.
    const n = try preadAt(fd, &buf, 0);
    try testing.expectEqualSlices(u8, EchoFs.hello_text, buf[0..n]);
    const after = Snapshot.take();
    try expectLaws(before, after);
    try testing.expectEqual(@as(u64, 1), after.bridge_cancels - before.bridge_cancels);
    std.debug.print("\nbridge stall: cancelled after {d} ms (deadline 300), reader EIO, then served\n", .{waited / std.time.ns_per_ms});
}

test "e2e retention: a held page-cache WRITE replies RETAIN — write(2) returns while the slot is Retained (ACK-early observed), the handler extracts AFTER the ACK, releases, the slot is Replied (retain_commits − releases ≡ 0); an O_DIRECT write is `.direct_gup` and its retain is REFUSED (committed plain, counted); teardown with a retained slot RELEASEs it before the unmount and leaves no mount" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    var kb: [192]u8 = undefined;
    if (kmbufMissing(&kb)) |why| return testkit.skipWhy(@src(), why);
    const rig = try Rig.init(testing.allocator, 29, &.{});
    defer rig.deinit();
    rig.fs.hold_eligible = true;
    rig.fs.retain_writes = true;
    rig.fs.retain_ignore_class = true;
    // The worker's class-refusal warn is a warn; the stale-reply refusals of
    // the releases after a refused retain are warns too.
    const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
    // A failed expectation while a handler parks on the gate must not hang
    // `rig.deinit`'s handler-group await.
    defer rig.fs.openRetainGate();
    if (s.retention == .absent) return testkit.skipWhy(@src(), testkit.venue.kernelFeatureAbsent(&kb, "RELEASE_PAYLOAD retention"));
    try testing.expect(s.shared.retention);
    const before = Snapshot.take();
    var pb: [256]u8 = undefined;
    // Both fds are closed BEFORE the session (step 4): an open fd across a
    // lazy detach keeps the superblock alive, and its FLUSH would wait on a
    // connection whose devs the ring queues still hold.
    const fd = try openPath(rig.child(&pb, "scratch"), .{ .ACCMODE = .WRONLY, .CLOEXEC = true });
    var page: [4096]u8 = undefined;
    for (&page, 0..) |*b, i| b.* = @truncate(i * 5 + 2);
    // 1. ACK-early: write(2) returns before the handler extracted or released.
    try testing.expectEqual(@as(usize, 4096), linux.pwrite(fd, &page, page.len, 0));
    const acked = Snapshot.take();
    try testing.expectEqual(@as(u64, 1), acked.retain_commits - before.retain_commits);
    try testing.expectEqual(before.releases, acked.releases);
    try testing.expectEqual(@as(u64, 1), slot.retained_outstanding.load());
    try testing.expectEqual(@as(u64, 1), rig.fs.retained_replies.load(.monotonic));
    try testing.expectEqual(@as(u64, 0), rig.fs.release_calls.load(.monotonic));
    try testing.expectEqual(@as(u64, 0), rig.fs.lazy_extracts.load(.monotonic));
    // The slot: retained (owes no reply), its pages still registered.
    try testing.expectEqual(@as(u32, 0), s.owedCount());
    var retained_slots: u32 = 0;
    for (s.groups) |g| for (g.members) |*m| for (m.slots.ents) |*e| if (e.state == .retained) {
        retained_slots += 1;
    };
    try testing.expectEqual(@as(u32, 1), retained_slots);
    // 2. Open the gate: the handler extracts from the still-registered slot
    //    (the bytes land AFTER the ACK), then releases.
    rig.fs.openRetainGate();
    const Released = struct {
        fn done(r: *Rig) bool {
            return r.fs.release_calls.load(.monotonic) >= 1 and slot.retained_outstanding.load() == 0;
        }
    };
    try testing.expect(waitUntil(rig, Released.done, 5 * std.time.ns_per_s));
    try testing.expectEqualSlices(u8, &page, rig.fs.scratch[0..4096]);
    try testing.expectEqual(@as(u64, 1), rig.fs.lazy_extracts.load(.monotonic));
    const released = Snapshot.take();
    try testing.expectEqual(@as(u64, 1), released.releases - before.releases);
    try testing.expectEqual(released.retain_commits - before.retain_commits, released.releases - before.releases);
    try testing.expectEqual(before.release_failures, released.release_failures);
    try testing.expectEqual(before.retain_refused, released.retain_refused);
    // The released slot is Replied and serves again.
    try testing.expectEqual(@as(usize, 4096), linux.pwrite(fd, &page, page.len, 4096));
    try testing.expect(waitUntil(rig, struct {
        fn done(r: *Rig) bool {
            return r.fs.release_calls.load(.monotonic) >= 2 and slot.retained_outstanding.load() == 0;
        }
    }.done, 5 * std.time.ns_per_s));
    try testing.expectEqualSlices(u8, &page, rig.fs.scratch[4096..8192]);
    // 3. O_DIRECT: `.direct_gup` — the handler snapshots first and the
    //    worker refuses the retain, committing plain.
    const dfd = try openPath(rig.child(&pb, "scratch"), .{ .ACCMODE = .WRONLY, .CLOEXEC = true, .DIRECT = true });
    const aligned = try testing.allocator.alignedAlloc(u8, .fromByteUnits(4096), 4096);
    defer testing.allocator.free(aligned);
    for (aligned, 0..) |*b, i| b.* = @truncate(i * 7 + 3);
    const refused_before = slot.retain_refused_class.load();
    try testing.expectEqual(@as(usize, 4096), linux.pwrite(dfd, aligned.ptr, aligned.len, 8192));
    try testing.expectEqual(@as(u8, @backingInt(zc.PayloadClass.direct_gup)), rig.fs.last_write_class.load(.acquire));
    try testing.expectEqual(refused_before + 1, slot.retain_refused_class.load());
    try testing.expectEqualSlices(u8, aligned, rig.fs.scratch[8192..12288]);
    try testing.expect(waitUntil(rig, struct {
        fn done(r: *Rig) bool {
            return r.fs.release_calls.load(.monotonic) >= 3;
        }
    }.done, 5 * std.time.ns_per_s));
    const after = Snapshot.take();
    try expectLaws(before, after);
    try testing.expectEqual(@as(u64, 2), after.retain_commits - before.retain_commits);
    try testing.expectEqual(@as(u64, 2), after.releases - before.releases);
    try testing.expectEqual(@as(u64, 0), slot.retained_outstanding.load());
    // 4. Teardown with a RETAINED slot: close the gate first so the next
    //    retain parks; close the session; the drain RELEASEs the retained
    //    ent itself (the ledger closes by the release, never by fiat); the
    //    handler's late extract fails and its release is dropped.
    rig.fs.retain_gate = .unset;
    try testing.expectEqual(@as(usize, 4096), linux.pwrite(fd, &page, page.len, 12288));
    try testing.expectEqual(@as(u64, 1), slot.retained_outstanding.load());
    _ = linux.close(dfd);
    _ = linux.close(fd);
    const released_before = worker.retained_released_at_teardown.load();
    s.close();
    try testing.expect(!s.isMounted());
    rig.fs.openRetainGate();
    s.deinit();
    rig.session = null;
    try testing.expectEqual(@as(u64, 1), rig.fs.extract_failures.load(.monotonic));
    try testing.expectEqual(after.abandoned, slot.requests_abandoned.load());
    try testing.expectEqual(released_before + 1, worker.retained_released_at_teardown.load());
    try testing.expectEqual(@as(u64, 0), slot.retained_outstanding.load());
    try testing.expectEqual(@as(u64, 3), slot.releases.load() - before.releases);
}

test "e2e retention teardown: N slots RETAINED at close — the orderly teardown RELEASEs every one while the connection is live (released_at_teardown == N, retained_outstanding == 0, release_failures unchanged, the kernel logs no `retained zc payloads` line), close returns in under 2 s, nothing is abandoned" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    var kb: [192]u8 = undefined;
    if (kmbufMissing(&kb)) |why| return testkit.skipWhy(@src(), why);
    const rig = try Rig.init(testing.allocator, 40, &.{});
    defer rig.deinit();
    rig.fs.hold_eligible = true;
    rig.fs.retain_writes = true;
    const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
    // On EVERY exit the parked handlers are let go before `rig.deinit`
    // awaits the handler group — a failed expectation above the explicit
    // open must not turn into a hang.
    defer rig.fs.openRetainGate();
    if (s.retention == .absent) return testkit.skipWhy(@src(), testkit.venue.kernelFeatureAbsent(&kb, "RELEASE_PAYLOAD retention"));
    try testing.expect(s.shared.retention);
    // Three retained slots on one queue leave one ent free at depth 4 (the
    // FLUSH at close(fd) needs it).
    const n_retained: u64 = 3;
    try testing.expect(s.plan.depth > n_retained);
    const before = Snapshot.take();
    const released_before = worker.retained_released_at_teardown.load();
    const kernel_drained_before = worker.retained_kernel_drained.load();
    const kmsg_before = try KernelLogMark.take(testing.allocator, retained_warn_needle);
    var pb: [256]u8 = undefined;
    const fd = try openPath(rig.child(&pb, "scratch"), .{ .ACCMODE = .WRONLY, .CLOEXEC = true });
    var page: [4096]u8 = undefined;
    for (&page, 0..) |*b, i| b.* = @truncate(i * 3 + 1);
    // Each write(2) returns on its RETAIN commit; the handlers park on the
    // closed gate, so the N slots stay Retained into the teardown.
    var k: u64 = 0;
    while (k < n_retained) : (k += 1) try testing.expectEqual(@as(usize, 4096), linux.pwrite(fd, &page, page.len, @intCast(k * 4096)));
    try testing.expectEqual(n_retained, slot.retained_outstanding.load());
    try testing.expectEqual(n_retained, rig.fs.retained_replies.load(.monotonic));
    try testing.expectEqual(@as(u64, 0), rig.fs.release_calls.load(.monotonic));
    _ = linux.close(fd);
    // FUSE_RELEASE follows close(2) asynchronously: let it land so the only
    // slots the teardown meets are the retained ones.
    try testing.expect(waitUntil(s, struct {
        fn done(sess: *Session) bool {
            return sess.owedCount() == 0;
        }
    }.done, 5 * std.time.ns_per_s));
    var wd: TeardownWatchdog = .{ .bound_ns = 20 * std.time.ns_per_s, .what = "Session.close with retained slots" };
    try wd.start();
    const t0 = clock.monotonicNs();
    s.close();
    const close_ns = clock.monotonicNs() - t0;
    wd.stop();
    try testing.expect(!s.isMounted());
    // The parked handlers finish: their late extract fails on the closed
    // session and their release is dropped (teardown owned the slots).
    rig.fs.openRetainGate();
    s.deinit();
    rig.session = null;
    const kmsg_after = try KernelLogMark.take(testing.allocator, retained_warn_needle);
    std.debug.print("\nretention teardown: close {d} ms; released_at_teardown +{d}, kernel_drained +{d}; kernel log '{s}' lines {d} → {d}\n", .{ close_ns / std.time.ns_per_ms, worker.retained_released_at_teardown.load() - released_before, worker.retained_kernel_drained.load() - kernel_drained_before, retained_warn_needle, kmsg_before.count, kmsg_after.count });
    // The law: every retained slot RELEASEd by us, none left to the kernel.
    try testing.expect(kmsg_after.sameAs(&kmsg_before));
    try testing.expect(close_ns < 2 * std.time.ns_per_s);
    try testing.expectEqual(n_retained, worker.retained_released_at_teardown.load() - released_before);
    try testing.expectEqual(kernel_drained_before, worker.retained_kernel_drained.load());
    try testing.expectEqual(@as(u64, 0), slot.retained_outstanding.load());
    try testing.expectEqual(@as(u64, 0), worker.releases_inflight.load());
    try testing.expectEqual(n_retained, rig.fs.extract_failures.load(.monotonic));
    const after = Snapshot.take();
    try expectLaws(before, after);
    try testing.expectEqual(n_retained, after.retain_commits - before.retain_commits);
    try testing.expectEqual(n_retained, after.releases - before.releases);
}

test "e2e TIME_LIMITS: a utimensat with a time outside our advertised range is clamped by the VFS to the bound before it reaches the daemon" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    const rig = try Rig.init(testing.allocator, 30, &.{});
    defer rig.deinit();
    const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
    // The init-flag bit IS the probe (porting spec §2.4): the kernel's INIT offer.
    var tb: [192]u8 = undefined;
    if (s.time_limits == .absent) return testkit.skipWhy(@src(), testkit.venue.kernelFeatureAbsent(&tb, "FUSE_TIME_LIMITS in the INIT offer"));
    try testing.expect(s.init_reply.timeLimits());
    var pb: [256]u8 = undefined;
    const path = rig.child(&pb, "scratch");
    // Far past our ceiling: the VFS clamps to s_time_max = our TIME_LIMITS_MAX_SEC.
    const times = [2]linux.timespec{ linux.UTIME.OMIT, .{ .sec = std.math.maxInt(i64) / 2, .nsec = 0 } };
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.utimensat(linux.AT.FDCWD, path, &times, 0)));
    try testing.expectEqual(abi.TIME_LIMITS_MAX_SEC, rig.fs.scratch_mtime.load(.acquire));
    var st: linux.Statx = undefined;
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.statx(linux.AT.FDCWD, path, 0, linux.STATX.BASIC_STATS, &st)));
    try testing.expectEqual(abi.TIME_LIMITS_MAX_SEC, st.mtime.sec);
    // And the floor.
    const low = [2]linux.timespec{ linux.UTIME.OMIT, .{ .sec = std.math.minInt(i64) / 2, .nsec = 0 } };
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.utimensat(linux.AT.FDCWD, path, &low, 0)));
    try testing.expectEqual(abi.TIME_LIMITS_MIN_SEC, rig.fs.scratch_mtime.load(.acquire));
}

test "e2e fused poll: SQUEEZEFS_FUSE_READ_FUSION=1 on a zc session — a probe that declines its first poll is served by the fused re-poll (fusions > 0, late demotions 0); the partition law holds; on BufRing the lever is inert (demotions)" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    var kb: [192]u8 = undefined;
    const no_kmbuf = kmbufMissing(&kb);
    for ([_][]const u8{ "auto", "bufring" }, 0..) |mode, i| {
        // `auto` without kmbuf settles on UserEnts (the lever inert, judged
        // below); an explicit `bufring` there is a refused mount.
        if (no_kmbuf != null and std.mem.eql(u8, mode, "bufring")) continue;
        const rig = try Rig.init(testing.allocator, 31 + @as(u32, @intCast(i)), &.{ modeEnv(mode), .{ "SQUEEZEFS_FUSE_READ_FUSION", "1" } });
        defer rig.deinit();
        rig.fs.probe_declines_first = true;
        const before = Snapshot.take();
        const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
        var pb: [256]u8 = undefined;
        var small: [64]u8 = undefined;
        var k: u32 = 0;
        while (k < 8) : (k += 1) {
            const n = try readWhole(rig.child(&pb, "hello"), &small);
            try testing.expectEqualSlices(u8, EchoFs.hello_text, small[0..n]);
        }
        const after = Snapshot.take();
        try expectLaws(before, after);
        if (s.mode == .zero_copy) {
            try testing.expect(s.shared.read_fusion_on);
            try testing.expect(after.fusions - before.fusions >= 8);
            try testing.expectEqual(before.late_demotions, after.late_demotions);
            try testing.expect(worker.fused_polls.load() >= 8);
        } else {
            try testing.expect(!s.shared.read_fusion_on);
            try testing.expectEqual(before.fusions, after.fusions);
        }
        std.debug.print("\nfused ({s}): reads {d}, serves {d}, fusions {d}, demotes {d}, late demotions {d}\n", .{ mode, after.reads - before.reads, after.serves - before.serves, after.fusions - before.fusions, after.demotes - before.demotes, after.late_demotions - before.late_demotions });
    }
    if (no_kmbuf) |why| return testkit.skipWhy(@src(), why);
}

test "e2e measured (scoping): enters per request and daemon copies per READ under UserEnts, BufRing and ZeroCopy side by side — 64 × 16 KiB preads of /big" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    std.debug.print("\n| mode | reads | ring requests | enters | enters/request | daemon READ copies | copies/READ | zc_serve bytes |\n|---|---|---|---|---|---|---|---|\n", .{});
    var kb: [192]u8 = undefined;
    const no_kmbuf = kmbufMissing(&kb);
    for ([_][]const u8{ "user", "bufring", "zc" }, 0..) |mode, i| {
        if (no_kmbuf != null and !std.mem.eql(u8, mode, "user")) continue;
        const rig = try Rig.init(testing.allocator, 33 + @as(u32, @intCast(i)), &.{modeEnv(mode)});
        defer rig.deinit();
        const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
        var pb: [256]u8 = undefined;
        const fd = try openPath(rig.child(&pb, "big"), .{ .ACCMODE = .RDONLY, .CLOEXEC = true });
        defer _ = linux.close(fd);
        var buf: [16 * 1024]u8 = undefined;
        const before = Snapshot.take();
        var k: u64 = 0;
        while (k < 64) : (k += 1) {
            const n = try preadAt(fd, &buf, k * buf.len);
            try testing.expectEqual(buf.len, n);
        }
        const settle = clock.monotonicNs() + 2 * std.time.ns_per_s;
        while (s.owedCount() > 0 and clock.monotonicNs() < settle) std.Thread.yield() catch {};
        const after = Snapshot.take();
        try expectLaws(before, after);
        const reqs = after.requests - before.requests;
        const en = after.enters - before.enters;
        const copies = rig.fs.read_copies.load(.monotonic);
        std.debug.print("| {t} | 64 | {d} | {d} | {d}.{d:0>2} | {d} | {d}.{d:0>2} | {d} |\n", .{ s.mode, reqs, en, en / @max(reqs, 1), (en * 100 / @max(reqs, 1)) % 100, copies, copies / 64, (copies * 100 / 64) % 100, after.zc_serve_bytes - before.zc_serve_bytes });
    }
    if (no_kmbuf) |why| return testkit.skipWhy(@src(), why);
}

test "mountinfo: the connection minor is the mountpoint's major:minor field, the last match wins, a foreign path or a malformed line is null" {
    const text =
        "22 1 0:21 / /proc rw,relatime - proc proc rw\n" ++
        "138 22 0:37 / /build/tmp/mnt-7-1 rw,nosuid,nodev,relatime - fuse echo rw,user_id=0,group_id=0\n" ++
        "short line\n" ++
        "140 22 0:39 / /build/tmp/mnt-7-1 rw,relatime - fuse echo rw\n" ++
        "141 22 x:y / /build/tmp/mnt-7-2 rw,relatime - fuse echo rw\n";
    try testing.expectEqual(@as(?u32, 39), mountinfoMinor(text, "/build/tmp/mnt-7-1"));
    try testing.expectEqual(@as(?u32, 21), mountinfoMinor(text, "/proc"));
    try testing.expectEqual(@as(?u32, null), mountinfoMinor(text, "/build/tmp/mnt-7-2"));
    try testing.expectEqual(@as(?u32, null), mountinfoMinor(text, "/nope"));
    try testing.expectEqual(@as(?u32, null), mountinfoMinor("", "/proc"));
}

test "sideband sizing: cells one per possible straggler plus the notify floor; ring entries cover the READs, a write per cell and the cancel" {
    try testing.expectEqual(@as(usize, 32), sidebandCells(8));
    try testing.expectEqual(@as(usize, 64), sidebandCells(33));
    try testing.expectEqual(@as(u16, 64), sidebandEntries(32));
    try testing.expectEqual(@as(usize, 2048), SIDEBAND_CELL_BYTES);
    try testing.expect(SIDEBAND_CELL_BYTES >= codec.NOTIFY_NAME_FRAME_MAX);
    try testing.expect(SIDEBAND_CELL_BYTES >= codec.OUT_HEADER_LEN + codec.MAX_OUT_LEN);
}

test "a sideband cell given back while another taker sits between its dequeue CAS and its sequence store is retried, never asserted (decision 0003 §AI.4): the free list holds the pool's whole population, so the refused push is a taker mid-pop — not a full ring; the give-back counts one respin and lands once that taker stores" {
    const gpa = testing.allocator;
    var pool = try CellPool.init(gpa, 0, 2);
    defer pool.deinit(gpa);
    // Taker 1 stalls mid-pop: its CAS took position 0; its store (cell 0
    // handed to the producer at position 2) has not run.
    const stalled = pool.free.dequeue_pos.fetchAdd(1, .monotonic);
    try testing.expectEqual(@as(usize, 0), stalled);
    // Taker 2 pops position 1 whole, and gives its cell back: the push at
    // position 2 meets cell 0 still filled.
    try testing.expectEqual(@as(?u32, 1), pool.free.tryPop());
    const respins0 = sideband_recycle_respins.load();
    const giver = try std.Thread.spawn(.{}, CellPool.give, .{ &pool, @as(u32, 1) });
    while (sideband_recycle_respins.load() == respins0) std.Thread.yield() catch {};
    // Taker 1 completes its pop.
    const cell0 = &pool.free.cells[stalled & pool.free.mask];
    const taken = cell0.data;
    cell0.seq.store(stalled +% pool.free.mask +% 1, .release);
    giver.join();
    try testing.expectEqual(@as(u32, 0), taken);
    try testing.expectEqual(respins0 + 1, sideband_recycle_respins.load());
    pool.give(taken);
    // Both cells back, each once.
    try testing.expectEqual(@as(?u32, 1), pool.free.tryPop());
    try testing.expectEqual(@as(?u32, 0), pool.free.tryPop());
    try testing.expectEqual(@as(?u32, null), pool.free.tryPop());
}
