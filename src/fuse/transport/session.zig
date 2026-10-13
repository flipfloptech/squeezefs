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
    /// The kmbuf ring's contiguous-page registration answered ENOMEM after
    /// every retry (`kmbuf.Error`): the probe decided the mode, so the law
    /// is fail loud, never degrade; `Config.refusal` carries the order.
    KmbufRingEnomem,
    SlabFailed,
};

/// The operator-facing detail of a typed refusal: `MountError` names the
/// class, this names the numbers the error alone cannot carry. Written by
/// `mount` into `Config.refusal` before the error returns.
pub const Refusal = union(enum) {
    none,
    /// `KmbufRingEnomem`: each ring buffer is `2^order` contiguous pages.
    kmbuf_ring_enomem: struct { order: u8 },

    pub fn format(self: Refusal, w: *Io.Writer) Io.Writer.Error!void {
        switch (self) {
            .none => try w.writeAll("no refusal detail"),
            .kmbuf_ring_enomem => |r| try w.print("kmbuf ring registration ENOMEM (order-{d} contiguous pages); retry, or SQUEEZEFS_FUSE_BUFFER_MODE=user", .{r.order}),
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
    free: mpmc.BoundedMpmc(u32),
    ready: mpmc.BoundedMpmc(u32),
    /// Bumped at every recycle; a submitter out of cells waits on it.
    freed: futex.Word = .init(0),
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
        const cells = try gpa.alloc(SbCell, ncells);
        errdefer gpa.free(cells);
        for (cells) |*c| c.* = .{};
        var free = try mpmc.BoundedMpmc(u32).init(gpa, ncells);
        errdefer free.deinit(gpa);
        var ready = try mpmc.BoundedMpmc(u32).init(gpa, ncells);
        errdefer ready.deinit(gpa);
        for (0..ncells) |i| assert(free.tryPush(@intCast(i)));
        return .{ .session = session, .ring = ring, .fuse_fd = fuse_fd, .fixed = fixed, .buf = buf, .cells = cells, .free = free, .ready = ready };
    }

    fn deinit(self: *Sideband, gpa: std.mem.Allocator) void {
        assert(self.thread == null);
        self.ring.deinit();
        self.ready.deinit(gpa);
        self.free.deinit(gpa);
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
            if (self.free.tryPop()) |i| {
                self.cells[i].done.reset();
                return i;
            }
            if (!wait) return null;
            const gen = self.freed.load(.acquire);
            if (self.free.tryPop()) |i| {
                self.cells[i].done.reset();
                return i;
            }
            // The lock plane's ticked park: a lost recycle costs one tick.
            _ = futex.waitTimeout(&self.freed, gen, tick_law.tick_ns);
        }
    }

    fn recycle(self: *Sideband, idx: u32) void {
        assert(self.free.tryPush(idx));
        _ = self.freed.fetchAdd(1, .release);
        futex.wakeAll(&self.freed);
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
        c.fire_and_forget = true;
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
        c.fire_and_forget = true;
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
            if (!self.read_armed) self.armRead();
            _ = self.ring.armWake();
            _ = self.ring.park(null);
            self.reap();
        }
        // A connection gone without DESTROY (a lazy umount, a sysfs abort,
        // a dead ring): the session ends here too.
        if (self.disconnected or self.ring.isDead()) {
            self.session.mounted.store(false, .release);
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
    /// `SQUEEZEFS_READ_ZC_SERVE` ∧ zero-copy: what INIT told the filesystem.
    read_zc_serve: bool = false,
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

    /// The ent's payload window (empty on the classical venue).
    pub fn replyWindow(self: *Session, s: SlotRef) []u8 {
        switch (s) {
            .classical => return &.{},
            .ring => |r| {
                if (r.qid >= self.plan.nqueues or r.ent_idx >= self.plan.depth) return &.{};
                const g = self.groups[self.qid_group[r.qid]];
                return g.window(g.gentOf(self.qid_member[r.qid], r.ent_idx));
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
        const g = self.groupOf(ref) orelse return;
        releases_requested.inc();
        g.requestRelease(g.gentOf(self.qid_member[ref.qid], ref.ent_idx));
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
        if (src.qid >= self.plan.nqueues or src.buf_index >= self.plan.depth) return error.SlotNotHeld;
        if (!self.shared.isActive()) return error.SessionClosed;
        const g = self.groups[self.qid_group[src.qid]];
        return g.storeFromForeign(g.gentOf(self.qid_member[src.qid], src.buf_index), fd, off, src.len, io);
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

    /// The group owning a live slot that still names `ref.commit_id` (the
    /// economy check; the worker's slot table is the authority).
    fn groupOf(self: *Session, ref: slot.SlotRef) ?*worker.Group {
        if (ref.qid >= self.plan.nqueues or ref.ent_idx >= self.plan.depth) {
            replies_dropped_no_slot.inc();
            return null;
        }
        const g = self.groups[self.qid_group[ref.qid]];
        const ms = self.qid_member[ref.qid];
        const ent = g.members[ms].slots.ent(ref.ent_idx);
        if (!self.shared.isActive() or ent.owed_commit_id.load(.acquire) != ref.commit_id) {
            replies_dropped_no_slot.inc();
            return null;
        }
        return g;
    }

    fn ringSubmit(self: *Session, ref: slot.SlotRef, msg: commit_queue.Msg) void {
        const g = self.groupOf(ref) orelse return;
        _ = g.cells.submit(g.gentOf(self.qid_member[ref.qid], ref.ent_idx), msg);
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

    /// The token plane's kernel-invalidation face (a recalled object's
    /// `INVAL_INODE` + `PRUNE` ride the sideband like every notify).
    pub fn tokenNotifySeam(self: *Session) read_divert.NotifySeam {
        return .{ .ctx = self, .invalInode = &notifyInvalInodeFn, .prune = &notifyPruneFn };
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

    /// THE ORDERED BRING-UP. `fs.init` runs once the geometry is planned.
    pub fn mount(gpa: std.mem.Allocator, exec_io: *SqzIo, fs: fsv.Filesystem, mountpoint: []const u8, env: *const std.process.Environ.Map, facts: derive.Facts, cfg: Config) MountError!*Session {
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
        errdefer self.unmountDetached();
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
        try self.resolveMode(env, init_in.folded());
        const topology: geometry.Topology = .{ .ctx = &self.topo, .node_of = nodeOfCpu };
        self.drain_groups = try geometry.drainGroupPlan(gpa, self.plan.nqueues, qid_is_cpu, topology, geometry.forcedWidth(self.mode, geometry.drainGroupWidthOverride(env)));
        errdefer self.drain_groups.deinit(gpa);

        // 3. `fuse.enable_uring=Y` BEFORE the reply lands: since 7.2.4 the
        //    kernel creates the connection's ring at INIT-reply time iff the
        //    parameter reads Y then; a later REGISTER on a ring-less
        //    connection is -EINVAL.
        ensureKernelUringEnabled(self.io);

        // 4. The reply word; the filesystem's init hook sees the negotiation.
        const wants: codec.InitWants = .{ .writeback_cache = cfg.opts.write_back and !cfg.opts.read_only };
        self.init_reply = codec.buildInitReply(init_in, self.plan, wants) catch {
            log.err("transport: unsupported FUSE protocol {d}.{d} (need 7.{d}+)", .{ init_in.major, init_in.minor, codec.MIN_SUPPORTED_MINOR });
            self.sideband.replyInline(codec.replyError(self.init_unique, .INVAL));
            return error.InitUnsupported;
        };
        geometry.gauges.publish(self.plan, init_in.max_readahead);
        geometry.gauges.publishDrainGroups(&self.drain_groups);
        self.time_limits = probes.timeLimitsVerdict(init_in.folded());
        time_limits_gauge.set(@intFromBool(self.init_reply.timeLimits()));
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
        probes.publish(self.verdicts, self.retention, self.time_limits, probes.driver.capSysAdmin(self.io, env), self.mode);
        fs.init(.{ .negotiated = self.init_reply.negotiated, .minor = self.init_reply.minor(), .plan = self.plan, .io = self.io, .buffer_mode = self.mode, .read_zc_serve = read_zc_serve, .retention = self.retention == .present });
        errdefer self.destroyOnce();
        log.info("transport: INIT 7.{d} → 7.{d}: flags 0x{x} (over_io_uring={}, time_limits={}), max_write {d}, max_pages {d}, max_background {d}, queues {d} × depth {d}, payload {d} B, {d} drain group(s) (width {d}), buffer mode {t} (kmbuf {?d}, retention {t})", .{ init_in.minor, self.init_reply.minor(), self.init_reply.negotiated, self.init_reply.has(abi.FUSE_OVER_IO_URING), self.init_reply.timeLimits(), self.plan.max_write, self.plan.max_pages, self.plan.max_background, self.plan.nqueues, self.plan.depth, self.plan.payload_sz, self.drain_groups.count(), self.drain_groups.maxWidth(), self.mode, if (self.verdicts.kmbuf.opcodes()) |p| p.register else null, self.retention });

        // 6. The zc bounce slab (ONE memfd for every queue; a non-sheddable
        //    budget component), then the workers: one per drain group, the
        //    arena as ONE budget component, the arm barrier counting
        //    submissions.
        if (self.mode == .zero_copy) {
            self.slab = worker.Slab.init(self.plan.nqueues, self.plan.depth, self.plan.payload_sz) catch return error.SlabFailed;
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
        };
        if (read_fusion_lever and !self.shared.read_fusion_on) log.info("transport: SQUEEZEFS_FUSE_READ_FUSION=1 accepted; inert on a {t} session (the fused poll is the zero-copy mode's)", .{self.mode});
        if (self.shared.sqpoll) log.info("transport: SQUEEZEFS_FUSE_SQPOLL=1 — the kernel submission poller (measured not recommended: the A/B control arm)", .{});
        self.shared.register_refuse_remaining.store(knobs.intKnob("SQUEEZEFS_TEST_FUSE_REGISTER_REFUSE", u32, env), .monotonic);
        self.shared.test_kmbuf_enomem = knobs.boolKnob("SQUEEZEFS_TEST_KMBUF_REGISTER_ENOMEM", env);
        self.armBridgeSeams(env);
        errdefer self.closeStallPipe();

        const ngroups = self.drain_groups.count();
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
        for (0..ngroups) |gi| {
            const qids = self.drain_groups.group(gi);
            for (qids, 0..) |q, mi| {
                self.qid_group[q] = @intCast(gi);
                self.qid_member[q] = @intCast(mi);
            }
            const affinity: ?placement.CpuSet = if (pcfg.numa and qid_is_cpu) placement.scopedAffinityCpus(pcfg.scope, qids[0], self.topo.mask, &self.topo) else null;
            self.groups[gi] = worker.Group.init(gpa, &self.shared, @intCast(gi), qids, affinity) catch |e| switch (e) {
                error.OutOfMemory => return error.OutOfMemory,
                error.ArenaMapFailed => return error.ArenaMapFailed,
            };
            built += 1;
            total_arena += self.groups[gi].arenaBytes();
        }
        arena_bytes.set(total_arena);
        // Non-sheddable: the kernel writes into these pages at delivery
        // (the arena on UserEnts; the kmbuf regions on the other rungs).
        self.arena_component = mem_budget.Component.init("transport_payload_arena", total_arena, 0, .{ .gauge = &arena_bytes }, null);
        if (cfg.budget) |b| b.register(self.arena_component);
        if (self.mode != .user_ents) {
            const pinned: u64 = @as(u64, self.plan.nqueues) * zc.ringEntriesFor(self.plan.depth) * derive.alignUp(self.plan.payload_sz, facts.page_size);
            self.kmbuf_component = mem_budget.Component.init("transport_kmbuf_region", pinned, 0, .{ .gauge = &kmbuf.pinned_bytes }, null);
            if (cfg.budget) |b| b.register(self.kmbuf_component);
        }

        var started: usize = 0;
        errdefer {
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
                        if (cfg.refusal) |r| r.* = .{ .kmbuf_ring_enomem = .{ .order = kmbuf.pageOrder(self.plan.payload_sz, facts.page_size) } };
                        return error.KmbufRingEnomem;
                    },
                };
            };
            started += 1;
        }
        // 7. The arm barrier: every queue's REGISTERs PUSHED (never completed).
        const verdict = self.shared.barrier.waitTimeout(ARM_BARRIER_NS) orelse {
            log.err("transport: arm barrier: {d} of {d} queues registered within {d} s — the mount fails", .{ self.shared.queues_registered.load(.acquire), self.plan.nqueues, ARM_BARRIER_NS / std.time.ns_per_s });
            return error.ArmBarrierFailed;
        };
        if (verdict == .failed) return error.EveryEntRetired;

        // 8. Ready: the sideband loop takes the classical connection forever.
        try self.sideband.start();
        self.ready.store(true, .release);
        sessions_active.add(1);
        self.publishKernelMaxBackground();
        log.info("transport: FUSE-over-io_uring transport armed at {s}: {d} queue(s) × {d} ent(s) over {d} worker(s), {t}", .{ self.mountpoint, self.plan.nqueues, self.plan.depth, ngroups, self.mode });
        return self;
    }

    /// The probes and the lattice decision (ONCE per mount): kmbuf by the
    /// opcode ladder on a scratch ring, zc by `CapEff` + the sparse table,
    /// the lever from the registry; an explicit rung the probes refuse fails
    /// the mount loud naming the probe (the only degrade point is the probe).
    fn resolveMode(self: *Session, env: *const std.process.Environ.Map, offered: u64) MountError!void {
        const page: u32 = @intCast(self.facts.page_size);
        const kmbuf_v = probes.driver.probeKmbuf(page);
        const cap = probes.driver.capSysAdmin(self.io, env);
        // The ring exists only when the kernel offered FUSE_OVER_IO_URING;
        // the sparse table is the zc slot shape (depth + the headers slot).
        const ring_offered = offered & abi.FUSE_OVER_IO_URING != 0;
        const sparse_ok = ring_offered and probes.driver.probeSparse(self.plan.depth + 1);
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
    pub fn queuesRegistered(self: *const Session) u32 {
        return self.shared.queues_registered.load(.acquire);
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
        self.awaitDrains();
        self.unmount();
        for (self.groups) |g| g.join();
        if (self.have_sideband) self.sideband.join();
        self.destroyOnce();
        self.closeFd();
    }

    /// Wait until every started group's drain flushed its synthesized
    /// commits (each group's `drained` word), bounded: the drain itself is
    /// bounded by `DRAIN_LEASE_BUDGET` plus one enter.
    fn awaitDrains(self: *Session) void {
        const deadline = clock.monotonicNs() + DRAIN_WAIT_NS;
        for (self.groups) |g| {
            if (g.started.load(.acquire) != 1) continue;
            while (g.drained.load(.acquire) == 0) {
                const now = clock.monotonicNs();
                if (now >= deadline) return;
                _ = futex.waitTimeout(&g.drained, 0, deadline - now);
            }
        }
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
            self.unmountDetached();
            return;
        }
    }

    fn unmountDetached(self: *Session) void {
        if (!self.mounted.load(.acquire)) return;
        if (self.unmounter) |u| {
            if (!u.call(u.ctx, self.mountpoint, true)) log.err("transport: fusermount3 -u -z {s} failed — a mount is left behind", .{self.mountpoint});
            self.mounted.store(false, .release);
            return;
        }
        const minor = self.connectionMinor();
        const rc = linux.umount2(self.mountpoint.ptr, linux.MNT.DETACH);
        if (linux.errno(rc) != .SUCCESS) log.err("transport: umount2({s}, MNT_DETACH) failed ({t}) — a mount is left behind", .{ self.mountpoint, linux.errno(rc) });
        self.mounted.store(false, .release);
        // The workers' residency has nothing to wait for on a detached
        // connection no holder will end (release pairs with the residency
        // loop's acquire).
        self.shared.detached.store(true, .release);
        // A detached superblock lives on behind any open fd, and the uring
        // queues hold the connection's devs, so closing our fd never aborts
        // it: a later FLUSH on that fd would wait forever. fusectl's abort
        // ends it (when fusectl is mounted; the supervisor's last rung).
        if (minor) |m| self.abortConnection(m);
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
    /// the raw syscalls, like `enable_uring`; a missing fusectl is quiet).
    fn abortConnection(self: *Session, minor: u32) void {
        var path: [96]u8 = undefined;
        const p = std.fmt.bufPrintSentinel(&path, "{s}/{d}/abort", .{ fusectl_connections, minor }, 0) catch return;
        const fd_rc = linux.open(p.ptr, .{ .ACCMODE = .WRONLY, .CLOEXEC = true }, 0);
        if (linux.errno(fd_rc) != .SUCCESS) return;
        const fd: linux.fd_t = @intCast(fd_rc);
        defer _ = linux.close(fd);
        if (linux.errno(linux.write(fd, "1", 1)) == .SUCCESS) log.warn("transport: aborted fuse connection {d} through fusectl after the lazy detach of {s}", .{ minor, self.mountpoint });
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
        try field(w, "transport_probe_time_limits", probes.time_limits_gauge.load());
        try field(w, "transport_probe_cap_sys_admin", probes.cap_gauge.load());
        try field(w, "transport_time_limits", time_limits_gauge.load());
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
        try field(w, "transport_zc_write_directs", worker.zc_write_directs.load());
        try field(w, "transport_zc_write_direct_bytes", worker.zc_write_direct_bytes.load());
        try field(w, "transport_zc_write_direct_failures", worker.zc_write_direct_failures.load());
        try field(w, "transport_zc_fd_source_copies", worker.zc_fd_source_copies.load());
        try field(w, "transport_zc_fallbacks", worker.zc_fallbacks.load());
        try field(w, "transport_zc_readdir_latch", worker.zc_readdir_latch.load());
        try field(w, "transport_zc_bridges", worker.zc_bridges.load());
        try field(w, "transport_zc_bridge_pends", worker.zc_bridge_pends.load());
        try field(w, "transport_zc_bridge_cancels", worker.bridge_cancels.load());
        try field(w, "transport_zc_bridge_lost", worker.bridge_lost.load());
        try field(w, "transport_zc_bridge_orphans", worker.bridge_orphans.load());
        try field(w, "transport_zc_bridge_restamps", worker.bridge_restamps.load());
        try field(w, "transport_zc_scan_max_age_ms", worker.scan_max_age_ms.load());
        try field(w, "transport_zc_direction_refusals", slot.zc_direction_refusals.load());
        try field(w, "transport_zc_write_holds", slot.zc_write_holds.load());
        try field(w, "transport_zc_write_extractions", slot.zc_write_extractions.load());
        try field(w, "transport_zc_write_extract_bytes", slot.zc_write_extract_bytes.load());
        try field(w, "transport_zc_write_lazy_extractions", slot.zc_write_lazy_extractions.load());
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
        try field(w, "transport_fast_dispatch_serves", worker.fast_dispatch_serves.load());
        try field(w, "transport_fast_dispatch_demotes", worker.fast_dispatch_demotes.load());
        try field(w, "transport_fused_read_fusions", worker.fused_read_fusions.load());
        try field(w, "transport_fused_read_demotions", worker.fused_read_demotions.load());
        try field(w, "transport_dispatch_refused", worker.dispatch_refused.load());
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
        try field(w, "transport_park_tick_commit_rescues", worker.park_tick_commit_rescues.load());
        try field(w, "transport_park_tick_cqe_lost_wakes", worker.park_tick_cqe_lost_wakes.load());
        try field(w, "transport_park_tick_late_cqes", worker.park_tick_late_cqes.load());
        try field(w, "transport_enters", worker.enters.load());
        try field(w, "transport_passes", worker.passes.load());
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
            .fuse = .{ .nqueues = facts.possible_cpus, .q_depth = geometry.Q_DEPTH_DESIRED },
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
    commit_rescues: u64,
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
    extractions: u64,
    lazy: u64,
    retain_commits: u64,
    releases: u64,
    retain_refused_class: u64,
    direction_refusals: u64,
    direct_legs: u64,
    bridges: u64,
    late_demotions: u64,

    fn take() Snapshot {
        return .{
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
            .extractions = slot.zc_write_extractions.load(),
            .lazy = slot.zc_write_lazy_extractions.load(),
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
            .commit_rescues = worker.park_tick_commit_rescues.load(),
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
    _ = waitUntil(&before, Settled.pred, 5 * std.time.ns_per_s);
    const after = Snapshot.take();
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
        fn run(r: *Rig, seed: u64, ops: *std.atomic.Value(u64), result: *anyerror!void) void {
            result.* = body(r, seed, ops);
        }
        fn body(r: *Rig, seed: u64, ops: *std.atomic.Value(u64)) !void {
            var prng = std.Random.DefaultPrng.init(seed);
            const rnd = prng.random();
            var pb: [256]u8 = undefined;
            var buf: [8192]u8 = undefined;
            const deadline = clock.monotonicNs() + 10 * std.time.ns_per_s;
            var n: u64 = 0;
            while (clock.monotonicNs() < deadline) : (n += 1) {
                switch (rnd.uintLessThan(u8, 4)) {
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
        }
    };
    var ops = std.atomic.Value(u64).init(0);
    var results: [4]anyerror!void = .{ {}, {}, {}, {} };
    var threads: [4]std.Thread = undefined;
    for (&threads, 0..) |*t, i| t.* = try std.Thread.spawn(.{}, Storm.run, .{ rig, @as(u64, i + 1), &ops, &results[i] });
    for (threads) |t| t.join();
    for (results) |r| try r;
    // `close(2)` sends RELEASE as a background request: the last ones are
    // still being answered when the threads return — wait for the slots.
    const settle = clock.monotonicNs() + 5 * std.time.ns_per_s;
    while (s.owedCount() > 0 and clock.monotonicNs() < settle) std.Thread.yield() catch {};
    const after = Snapshot.take();
    try expectLaws(before, after);
    try testing.expectEqual(before.commit_rescues, after.commit_rescues);
    try testing.expectEqual(before.cqe_lost_wakes, after.cqe_lost_wakes);
    try testing.expectEqual(after.parked - before.parked, after.unparked - before.unparked);
    try testing.expect(after.demotes - before.demotes > 0);
    try testing.expectEqual(@as(u64, 0), slot.leases_outstanding.load());
    try testing.expectEqual(@as(u32, 0), s.owedCount());
    const reqs = after.requests - before.requests;
    const writes = after.writes - before.writes;
    const elided = after.elided - before.elided;
    std.debug.print("\nstorm: {d} syscalls, {d} ring requests, {d} enters ({d}.{d:0>2} per request), wake writes {d} / elided {d} ({d} % elided), backstop ticks {d}, commit rescues {d}, lost CQE wakes {d}, late CQEs {d}\n", .{ ops.load(.monotonic), reqs, after.enters - before.enters, (after.enters - before.enters) / @max(reqs, 1), ((after.enters - before.enters) * 100 / @max(reqs, 1)) % 100, writes, elided, elided * 100 / @max(writes + elided, 1), worker.park_backstop_ticks.load(), after.commit_rescues - before.commit_rescues, after.cqe_lost_wakes - before.cqe_lost_wakes, after.late_cqes - before.late_cqes });
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
    try testing.expectEqual(before.commit_rescues, after.commit_rescues);
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
    for ([_][]const u8{ "bufring", "user" }, 0..) |mode, i| {
        const rig = try Rig.init(testing.allocator, 21 + @as(u32, @intCast(i)), &.{modeEnv(mode)});
        defer rig.deinit();
        const before = Snapshot.take();
        const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
        const want: geometry.BufferMode = if (std.mem.eql(u8, mode, "bufring")) .buf_ring else .user_ents;
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
}

test "e2e lattice: an explicit `zc` on a venue without CAP_SYS_ADMIN (the seam) fails the mount LOUD naming the probe; `auto` on the same seam settles on BufRing — the probe is the only degrade point" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
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

test "e2e lattice: the kmbuf ring answering ENOMEM after every retry (the seam) fails the mount with the TYPED KmbufRingEnomem refusal whose detail names the page order and the remedy; nothing stays pinned or mounted" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    if (probes.driver.probeKmbuf(@intCast(derive.facts().page_size)).opcodes() == null) return testkit.skip(@src(), .no_kernel_feature, "no kmbuf register opcode on this kernel");
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
    var buf: [256]u8 = undefined;
    const text = try std.fmt.bufPrint(&buf, "{f}", .{refusal});
    try testing.expect(std.mem.startsWith(u8, text, "kmbuf ring registration ENOMEM (order-"));
    try testing.expect(std.mem.endsWith(u8, text, " contiguous pages); retry, or SQUEEZEFS_FUSE_BUFFER_MODE=user"));
    try testing.expectEqual(retries_before + kmbuf.REGISTER_RETRIES, kmbuf.register_retries.load());
    try testing.expectEqual(pinned_before, kmbuf.pinned_bytes.load());
    var mi_buf: [64 * 1024]u8 = undefined;
    const table = try Io.Dir.cwd().readFile(testing.io, "/proc/self/mountinfo", &mi_buf);
    try testing.expectEqual(@as(?u32, null), mountinfoMinor(table, rig.path));
}

test "e2e zc READ: /big moves through the fd-source leg from the fill pool (zc_serve_bytes ≡ user bytes, daemon copies 0, every reply rode the slot); with SQUEEZEFS_READ_ZC_SERVE=0 the bounce leg serves with ONE copy per READ" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
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

test "e2e zc WRITE: a W1-eligible aligned write is HELD in the slot and extracted lazily by the handler (one bridge, bytes land); an unaligned write is extracted at delivery; a session whose filesystem never declares eligibility extracts every paged write at delivery" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
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

test "e2e zc bridges: a bridge that never completes (the stall seam) is cancelled at the deadline — bridge_cancels == 1, the slot still commits header-only -EIO (THE LAW: the reader gets EIO), lost/orphans stay 0, the session serves on" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
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
    const rig = try Rig.init(testing.allocator, 40, &.{});
    defer rig.deinit();
    rig.fs.hold_eligible = true;
    rig.fs.retain_writes = true;
    const s = try rig.mountIt(.{ .mem_budget_bytes = 0 });
    // On EVERY exit the parked handlers are let go before `rig.deinit`
    // awaits the handler group — a failed expectation above the explicit
    // open must not turn into a hang.
    defer rig.fs.openRetainGate();
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
    for ([_][]const u8{ "auto", "bufring" }, 0..) |mode, i| {
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
}

test "e2e measured (scoping): enters per request and daemon copies per READ under UserEnts, BufRing and ZeroCopy side by side — 64 × 16 KiB preads of /big" {
    if (venueMissing()) |why| return testkit.skipWhy(@src(), why);
    std.debug.print("\n| mode | reads | ring requests | enters | enters/request | daemon READ copies | copies/READ | zc_serve bytes |\n|---|---|---|---|---|---|---|---|\n", .{});
    for ([_][]const u8{ "user", "bufring", "zc" }, 0..) |mode, i| {
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
