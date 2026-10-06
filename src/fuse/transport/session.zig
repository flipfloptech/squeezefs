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
const commit_queue = @import("commit_queue.zig");
const fsv = @import("filesystem_vtable.zig");
const worker = @import("worker.zig");
const counters = @import("../../core/counters.zig");
const clock = @import("../../core/clock.zig");
const log = @import("../../core/log.zig");
const knobs = @import("../../core/knobs.zig");
const derive = @import("../../core/derive.zig");
const thread_name = @import("../../core/thread_name.zig");
const futex = @import("../../sync/futex.zig");
const mpmc = @import("../../sync/mpmc.zig");
const completion = @import("../../sync/completion.zig");
const tick_law = @import("../../sync/tick.zig");
const exec = @import("../../exec/exec.zig");
const placement = @import("../../exec/placement.zig");
const mem_budget = @import("../../mem/budget.zig");

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
/// The buffer-mode lattice rung this session runs: `UserEnts` (stock);
/// kmbuf and zero-copy land behind their probes.
pub const buffer_mode: geometry.BufferMode = .user_ents;
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
    mounted: bool = false,
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

    pub fn releaseLease(self: *Session, l: PayloadLease) void {
        _ = self;
        l.release();
    }

    fn ringSubmit(self: *Session, ref: slot.SlotRef, msg: commit_queue.Msg) void {
        if (ref.qid >= self.plan.nqueues or ref.ent_idx >= self.plan.depth) {
            replies_dropped_no_slot.inc();
            return;
        }
        const g = self.groups[self.qid_group[ref.qid]];
        const ms = self.qid_member[ref.qid];
        const ent = g.members[ms].slots.ent(ref.ent_idx);
        // The economy check: a slot that no longer owes this commit id takes
        // no cell; the worker's `admitCommit` is the authority.
        if (!self.shared.isActive() or ent.owed_commit_id.load(.acquire) != ref.commit_id) {
            replies_dropped_no_slot.inc();
            return;
        }
        _ = g.cells.submit(g.gentOf(ms, ref.ent_idx), msg);
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
        self.tx = .{ .ctx = self, .submit = txSubmit, .submit_fd_body = txSubmitFdBody, .submit_prefilled = txSubmitPrefilled, .fail = txFail };

        // 0. The device and the mount.
        const fd_rc = linux.open(dev_fuse_path, .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
        if (linux.errno(fd_rc) != .SUCCESS) {
            log.err("transport: open({s}) failed ({t})", .{ dev_fuse_path, linux.errno(fd_rc) });
            return error.DevFuseOpenFailed;
        }
        self.fuse_fd = @intCast(fd_rc);
        errdefer self.closeFd();
        var data_buf: [512]u8 = undefined;
        var w: Io.Writer = .fixed(&data_buf);
        cfg.opts.writeKernelData(self.fuse_fd, &w) catch return error.MountFailed;
        w.writeByte(0) catch return error.MountFailed;
        const mount_rc = linux.mount(self.source.ptr, self.mountpoint.ptr, "fuse", cfg.opts.msFlags(), @intFromPtr(w.buffered().ptr));
        if (linux.errno(mount_rc) != .SUCCESS) {
            log.err("transport: mount(2) of {s} at {s} failed ({t}); data '{s}'", .{ self.source, self.mountpoint, linux.errno(mount_rc), w.buffered()[0 .. w.buffered().len - 1] });
            return error.MountFailed;
        }
        self.mounted = true;
        errdefer self.unmountDetached();

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

        // 2. Geometry BEFORE the reply is serialized; the drain-group plan.
        self.plan = geometry.plan(geometry.resolveInputs(facts, env, cfg.block_size, cfg.mem_budget_bytes, max_pages_limit, cfg.opts.max_background, cfg.opts.congestion_threshold));
        self.topo = placement.Topology.probe(self.io);
        const qid_is_cpu = self.plan.nqueues == facts.possible_cpus;
        const topology: geometry.Topology = .{ .ctx = &self.topo, .node_of = nodeOfCpu };
        self.drain_groups = try geometry.drainGroupPlan(gpa, self.plan.nqueues, qid_is_cpu, topology, geometry.forcedWidth(buffer_mode, geometry.drainGroupWidthOverride(env)));
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
        fs.init(.{ .negotiated = self.init_reply.negotiated, .minor = self.init_reply.minor(), .plan = self.plan, .io = self.io });
        errdefer self.destroyOnce();

        // 5. Write the reply classically (the kernel refuses REGISTER until
        //    `fch->initialized`).
        {
            var out: codec.OutBuf = undefined;
            const r = codec.replyInit(&out, self.init_unique, self.init_reply);
            var frame: [codec.INIT_REPLY_LEN]u8 = undefined;
            frame[0..codec.OUT_HEADER_LEN].* = abi.encode(r.header);
            @memcpy(frame[codec.OUT_HEADER_LEN..], r.out);
            try self.sideband.writeOnce(&frame);
        }
        log.info("transport: INIT 7.{d} → 7.{d}: flags 0x{x} (over_io_uring={}, time_limits={}), max_write {d}, max_pages {d}, max_background {d}, queues {d} × depth {d}, payload {d} B, {d} drain group(s) (width {d})", .{ init_in.minor, self.init_reply.minor(), self.init_reply.negotiated, self.init_reply.has(abi.FUSE_OVER_IO_URING), self.init_reply.timeLimits(), self.plan.max_write, self.plan.max_pages, self.plan.max_background, self.plan.nqueues, self.plan.depth, self.plan.payload_sz, self.drain_groups.count(), self.drain_groups.maxWidth() });

        // 6. The workers: one per drain group, the arena as ONE budget
        //    component, the arm barrier counting submissions.
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
            // Arm (b) exists only on a zero-copy session (the fused lane is
            // the zc mode's); on the lattice's stock rung the lever is inert.
            .read_fusion_on = read_fusion_lever and buffer_mode == .zero_copy,
            .sqpoll = knobs.boolKnob("SQUEEZEFS_FUSE_SQPOLL", env),
            .node_of = nodeOfCpu,
            .node_ctx = &self.topo,
            .test_error_logs = cfg.test_error_logs,
        };
        if (read_fusion_lever and !self.shared.read_fusion_on) log.info("transport: SQUEEZEFS_FUSE_READ_FUSION=1 accepted; inert on a {s} session (the fused lane is the zero-copy mode's)", .{@tagName(buffer_mode)});
        if (self.shared.sqpoll) log.info("transport: SQUEEZEFS_FUSE_SQPOLL=1 — the kernel submission poller (measured not recommended: the A/B control arm)", .{});
        self.shared.register_refuse_remaining.store(knobs.intKnob("SQUEEZEFS_TEST_FUSE_REGISTER_REFUSE", u32, env), .monotonic);

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
        // Non-sheddable: the kernel writes into these pages at delivery.
        self.arena_component = mem_budget.Component.init("transport_payload_arena", total_arena, 0, .{ .gauge = &arena_bytes }, null);
        if (cfg.budget) |b| b.register(self.arena_component);

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
        log.info("transport: FUSE-over-io_uring transport armed at {s}: {d} queue(s) × {d} ent(s) over {d} worker(s)", .{ self.mountpoint, self.plan.nqueues, self.plan.depth, ngroups });
        return self;
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

    /// Idempotent: flip `active`, wake every worker and the sideband.
    pub fn shutdown(self: *Session) void {
        if (!self.shared.shutdown()) return;
        if (self.ready.swap(false, .acq_rel)) sessions_active.sub(1);
        for (self.groups) |g| g.forceWake();
        if (self.have_sideband) self.sideband.requestStop();
    }

    // ---- teardown ----

    /// Phase 1: shutdown → the workers' drains synthesize an error for every
    /// owed slot (the blocked callers return, so the mount is not busy) →
    /// `umount2` → the disconnect ends the workers' residency and the
    /// sideband → join → the filesystem's `destroy` → close the fd.
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
    /// syscall or closing its fd — a bounded retry, never a lazy detach by
    /// default (that would hide an owed reply behind MNT_DETACH).
    fn unmount(self: *Session) void {
        if (!self.mounted) return;
        const deadline = clock.monotonicNs() + UMOUNT_RETRY_NS;
        while (true) {
            const rc = linux.umount2(self.mountpoint.ptr, 0);
            const e = linux.errno(rc);
            if (e == .SUCCESS) {
                self.mounted = false;
                return;
            }
            if (e != .BUSY or clock.monotonicNs() >= deadline) {
                log.warn("transport: umount2({s}) failed ({t}); detaching lazily", .{ self.mountpoint, e });
                self.unmountDetached();
                return;
            }
            umount_busy_retries.inc();
            std.Thread.yield() catch {};
        }
    }

    fn unmountDetached(self: *Session) void {
        if (!self.mounted) return;
        const rc = linux.umount2(self.mountpoint.ptr, linux.MNT.DETACH);
        if (linux.errno(rc) != .SUCCESS) log.err("transport: umount2({s}, MNT_DETACH) failed ({t}) — a mount is left behind", .{ self.mountpoint, linux.errno(rc) });
        self.mounted = false;
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
        try field(w, "transport_park_tick_cqe_rescues", worker.park_tick_cqe_rescues.load());
        try field(w, "transport_enters", worker.enters.load());
        try field(w, "transport_passes", worker.passes.load());
        try field(w, "transport_residency_replies", worker.residency_replies.load());
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
fn venueAvailable() bool {
    if (linux.getuid() != 0) return false;
    const rc = linux.open(dev_fuse_path, .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) return false;
    _ = linux.close(@intCast(rc));
    if (!hasCapSysAdmin()) {
        log.warn("transport e2e: CAP_SYS_ADMIN is not in this container's effective set — mount(2) would be EPERM; recreate the venue with `scripts/venue up` (the recipe adds the capability). Skipping.", .{});
        return false;
    }
    return true;
}

/// `CapEff` bit 21 from `/proc/self/status`.
fn hasCapSysAdmin() bool {
    var buf: [4096]u8 = undefined;
    const text = Io.Dir.cwd().readFile(std.testing.io, "/proc/self/status", &buf) catch return false;
    var it = std.mem.splitScalar(u8, text, '\n');
    while (it.next()) |line| {
        if (!std.mem.startsWith(u8, line, "CapEff:")) continue;
        const hex = std.mem.trim(u8, line["CapEff:".len..], " \t");
        const caps = std.fmt.parseInt(u64, hex, 16) catch return false;
        return caps & (@as(u64, 1) << 21) != 0;
    }
    return false;
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
    commit_rescues: u64,
    cqe_rescues: u64,
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

    fn take() Snapshot {
        return .{
            .requests = worker.ring_requests.load(),
            .reads = worker.ring_reads.load(),
            .serves = worker.fast_dispatch_serves.load(),
            .fusions = worker.fused_read_fusions.load(),
            .demotes = worker.fast_dispatch_demotes.load(),
            .parked = slot.parked_commits.load(),
            .unparked = slot.unparked_commits.load(),
            .abandoned = slot.requests_abandoned.load(),
            .overflows = worker.cq_overflows.load(),
            .commit_rescues = worker.park_tick_commit_rescues.load(),
            .cqe_rescues = worker.park_tick_cqe_rescues.load(),
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
    try testing.expectEqual(b.reads - a.reads, (b.serves - a.serves) + (b.fusions - a.fusions) + (b.demotes - a.demotes));
}

test "e2e: EchoFs mounts over the ring — INIT negotiated FUSE_OVER_IO_URING, enable_uring reads Y, every queue armed, stat/open/read from a helper thread arrive over the RING (the sideband saw no request), replies byte-exact, the geometry the kernel accepted equals the plan" {
    if (!venueAvailable()) return error.SkipZigTest;
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
    if (!venueAvailable()) return error.SkipZigTest;
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
    if (!venueAvailable()) return error.SkipZigTest;
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
    if (!venueAvailable()) return error.SkipZigTest;
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
    try testing.expect(!s.mounted);
    t.join();
    try testing.expectError(error.ReadFailed, result);
    const after = Snapshot.take();
    try testing.expectEqual(after.abandoned, before.abandoned);
    try testing.expect(after.synthetic - before.synthetic >= 1);
    try testing.expectEqual(@as(u32, 0), s.owedCount());
    // Release the parked handler: its late reply is dropped (the session
    // owes nothing), then phase 2 joins it.
    rig.fs.releaseParked();
    const dropped_before = replies_dropped_no_slot.load();
    s.deinit();
    rig.session = null;
    try testing.expect(replies_dropped_no_slot.load() >= dropped_before + 1);
}

test "e2e: a 10 s random-op storm from 4 threads never needs the 100 ms backstop (tick rescues stay 0), abandons nothing, overflows nothing; the partition law and parked ≡ unparked hold; the coalescer elides" {
    if (!venueAvailable()) return error.SkipZigTest;
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
    try testing.expectEqual(before.cqe_rescues, after.cqe_rescues);
    try testing.expectEqual(after.parked - before.parked, after.unparked - before.unparked);
    try testing.expect(after.demotes - before.demotes > 0);
    try testing.expectEqual(@as(u64, 0), slot.leases_outstanding.load());
    try testing.expectEqual(@as(u32, 0), s.owedCount());
    const reqs = after.requests - before.requests;
    const writes = after.writes - before.writes;
    const elided = after.elided - before.elided;
    std.debug.print("\nstorm: {d} syscalls, {d} ring requests, {d} enters ({d}.{d:0>2} per request), wake writes {d} / elided {d} ({d} % elided), backstop ticks {d}, rescues {d}/{d}\n", .{ ops.load(.monotonic), reqs, after.enters - before.enters, (after.enters - before.enters) / @max(reqs, 1), ((after.enters - before.enters) * 100 / @max(reqs, 1)) % 100, writes, elided, elided * 100 / @max(writes + elided, 1), worker.park_backstop_ticks.load(), after.commit_rescues - before.commit_rescues, after.cqe_rescues - before.cqe_rescues });
    try testing.expect(writes + elided >= after.replies - before.replies - (after.serves - before.serves));
}

test "e2e: the REGISTER refusal seam — the first 5 pushes are refused, back off 1 ms doubling, every ent is eventually registered, nothing retires, the mount arms and serves" {
    if (!venueAvailable()) return error.SkipZigTest;
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
    if (!venueAvailable()) return error.SkipZigTest;
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
    if (!venueAvailable()) return error.SkipZigTest;
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

test "sideband sizing: cells one per possible straggler plus the notify floor; ring entries cover the READs, a write per cell and the cancel" {
    try testing.expectEqual(@as(usize, 32), sidebandCells(8));
    try testing.expectEqual(@as(usize, 64), sidebandCells(33));
    try testing.expectEqual(@as(u16, 64), sidebandEntries(32));
    try testing.expectEqual(@as(usize, 2048), SIDEBAND_CELL_BYTES);
    try testing.expect(SIDEBAND_CELL_BYTES >= codec.NOTIFY_NAME_FRAME_MAX);
    try testing.expect(SIDEBAND_CELL_BYTES >= codec.OUT_HEADER_LEN + codec.MAX_OUT_LEN);
}
