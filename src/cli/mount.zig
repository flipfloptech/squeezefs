//! `squeezefs mount` (charter §8; §4 "Bring-up (ordered)"; §5.7 layer A;
//! §9 "`main` drives the CLI with a thread-park `block_on`"): the daemon's
//! bring-up in the charter's order INSIDE `SqzIo.run`, the serve loop, and
//! the teardown in the charter's order. Linux only.
//!
//! Bring-up (the root task on the `main` lane):
//!   resolve the budget (pure) → open the backend (the D0 ladder's layer A:
//!   the flock; a FAILED/refusal class is loud and names its errno through
//!   the ONE table) → start the budget sampler → `KvFs(KvBackend)` + the
//!   handler tables (watchdog registry, op guards, open table) + the ring
//!   `Binding` → the FUSE fd: root opens `/dev/fuse` inside the session's
//!   `mount(2)`; anyone else receives it from `fusermount3` (`fusermount.zig`)
//!   → `Session.mount` (INIT, geometry, `enable_uring`, workers, the arm
//!   barrier) → the watchdog loop and the reclaim cadence on `spawnMeta` →
//!   `ready` (logged; written to the supervisor pipe under `--daemon`).
//!
//! Serve: the main task parks on the daemon's eventfd through its lane ring
//! (`SqzIo.readable`): a signal (`daemon.Signals`) or the session's
//! `on_shutdown` hook (DESTROY from an external `umount`, a disconnect, a
//! failed queue) wakes it. SIGUSR1 dumps every stats family to the log —
//! the `.stats` inode's stand-in until rung 5. Stop latches; nothing is
//! aborted from outside.
//!
//! Teardown: `Session.close` (drains synthesize every owed reply, the
//! unmount, the workers join) → the watchdog and the cadence cancelled →
//! `Session.deinit` → the filesystem → `KvBackend.close` (the final
//! checkpoint: an empty replay window for the next mount) → the sampler →
//! exit 0; a volume that latched FAILED exits 1 naming the reason.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const cli = @import("cli.zig");
const args = @import("args.zig");
const uri = @import("uri.zig");
const daemon = @import("daemon.zig");
const fusermount = @import("fusermount.zig");
const exec = @import("../exec/exec.zig");
const exec_stats = @import("../exec/stats.zig");
const backend = @import("../meta/backend.zig");
const node_cache = @import("../meta/node_cache.zig");
const dlm = @import("../meta/dlm.zig");
const handler = @import("../fuse/handler.zig");
const transport = @import("../fuse/transport.zig");
const fuse_geometry = @import("../fuse/geometry.zig");
const mount_options = @import("../fuse/mount_options.zig");
const mem_budget = @import("../mem/budget.zig");
const counters = @import("../core/counters.zig");
const derive = @import("../core/derive.zig");
const errors = @import("../core/errors.zig");
const log = @import("../core/log.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("cli/mount is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

const SqzIo = exec.SqzIo;
const Session = transport.session.Session;
const BackendFs = handler.BackendFs;
const watchdog = handler.watchdog;
const op_guard = handler.op_guard;
const op_trace = handler.op_trace;
const open_table = handler.open_table;
const filesystem = handler.filesystem;

/// The reclaim cadence: a partial corpse batch (< `kv_fs.RECLAIM_BATCH`)
/// waits at most one watchdog tick (5 s) for its destroy entry. The
/// entries/op economy (≤ 1.05 per unlink, charter §5.2) is measured on
/// FULL batches under storms; a tail of corpses idle for one tick costs
/// one entry, and the tick is the slowest periodic cadence the daemon
/// already runs.
pub const RECLAIM_CADENCE_NS: u64 = watchdog.TICK_NS;
/// The INIT `max_write` desire's block term before any data volume exists
/// (charter §6.1 "Striped: 4 MiB blocks (configurable CHUNK_SIZE)"): the
/// default chunk, so the transport geometry a rung-4 mount negotiates is
/// the one measured today.
pub const DEFAULT_BLOCK_SIZE: u64 = 4 * derive.MiB;

/// The verb's state, pinned in `run`'s frame for the root task.
const Ctx = struct {
    p: cli.Process,
    sqz: *SqzIo,
    facts: derive.Facts,
    budget_bytes: u64,
    budget_host: mem_budget.Host,
    path: []const u8,
    mountpoint: [:0]const u8,
    opts: mount_options.MountOptions,
    /// The raw `-o` string (the fusermount3 path forwards it).
    reporter: ?daemon.Reporter,
    session_ended: std.atomic.Value(bool) = .init(false),

    /// Report a bring-up failure: the log, stderr (foreground) or the
    /// supervisor pipe (daemon); exit 1.
    fn failed(self: *Ctx, comptime fmt: []const u8, a: anytype) u8 {
        var buf: [1024]u8 = undefined;
        const text = std.fmt.bufPrint(&buf, fmt, a) catch buf[0..];
        log.err("mount: {s}", .{text});
        if (self.reporter) |*r| {
            r.fail(text);
            r.close();
        } else {
            self.p.stderr.print("squeezefs mount: {s}\n", .{text}) catch {};
            self.p.stderr.flush() catch {};
        }
        return cli.exit_failed;
    }
};

fn onSessionShutdown(ctx_ptr: *anyopaque) void {
    const ctx: *Ctx = @ptrCast(@alignCast(ctx_ptr));
    // release pairs with the serve loop's acquire after the eventfd wake.
    ctx.session_ended.store(true, .release);
    daemon.Signals.ring();
}

const UnmountCtx = struct {
    io: Io,
    fn call(ctx_ptr: *anyopaque, mountpoint: [:0]const u8) bool {
        const self: *UnmountCtx = @ptrCast(@alignCast(ctx_ptr));
        return fusermount.unmount(self.io, mountpoint);
    }
};

pub fn run(p: cli.Process, m: args.Mount) u8 {
    // The URI: one member at this rung.
    var members: [uri.MAX_MEMBERS][]const u8 = undefined;
    var udiag: uri.Refusal = undefined;
    const meta = uri.parse(.meta, m.meta_uri, &members, &udiag) catch {
        p.stderr.print("squeezefs mount: {f}\n", .{udiag}) catch {};
        return cli.exit_usage;
    };
    if (meta.paths.len != 1) {
        p.stderr.print("squeezefs mount: {s} names {d} metadata volumes; this rung mounts ONE (a metadata set is rung 5's)\n", .{ m.meta_uri, meta.paths.len }) catch {};
        return cli.exit_failed;
    }
    // The mountpoint: an existing directory.
    var mp_buf: [linux.PATH_MAX:0]u8 = undefined;
    const mountpoint = std.fmt.bufPrintSentinel(&mp_buf, "{s}", .{m.mountpoint}, 0) catch {
        p.stderr.writeAll("squeezefs mount: the mountpoint path is too long\n") catch {};
        return cli.exit_usage;
    };
    {
        var st: linux.Statx = undefined;
        const rc = linux.statx(linux.AT.FDCWD, mountpoint.ptr, 0, .{ .TYPE = true }, &st);
        if (linux.errno(rc) != .SUCCESS) {
            p.stderr.print("squeezefs mount: mountpoint {s}: {t}\n", .{ mountpoint, linux.errno(rc) }) catch {};
            return cli.exit_failed;
        }
        if (!linux.S.ISDIR(st.mode)) {
            p.stderr.print("squeezefs mount: mountpoint {s} is not a directory\n", .{mountpoint}) catch {};
            return cli.exit_failed;
        }
    }
    // The options: the env TTLs as the base, the process's ids, root's
    // `allow_other` posture (an unprivileged mount needs
    // `user_allow_other` and asks for it with `-o allow_other`).
    const root = linux.geteuid() == 0;
    var opts: mount_options.MountOptions = .{ .ttls = mount_options.Ttls.fromEnv(p.env), .uid = linux.geteuid(), .gid = linux.getegid(), .allow_other = root };
    if (m.options) |text| switch (mount_options.parseOptionString(&opts, text)) {
        .ok => {},
        .invalid => |d| {
            p.stderr.print("squeezefs mount: {f}\n", .{d}) catch {};
            return cli.exit_usage;
        },
    };

    // `--daemon`: the log file first (its refusal reaches the terminal),
    // then the fork — before any thread exists.
    var reporter: ?daemon.Reporter = null;
    if (m.daemon) {
        var lf_buf: [linux.PATH_MAX:0]u8 = undefined;
        const lfz = std.fmt.bufPrintSentinel(&lf_buf, "{s}", .{m.log_file.?}, 0) catch {
            p.stderr.writeAll("squeezefs mount: the --log-file path is too long\n") catch {};
            return cli.exit_usage;
        };
        const log_fd = daemon.openLogFile(lfz, p.stderr) catch return cli.exit_failed;
        p.stderr.flush() catch {};
        switch (daemon.detach(p.gpa, p.io, .{ .supervise = m.supervise, .mountpoint = mountpoint, .log_fd = log_fd }, p.stderr)) {
            .parent => |code| return code,
            .daemon => |r| reporter = r,
        }
    }
    daemon.Signals.install() catch {
        p.stderr.writeAll("squeezefs mount: eventfd failed\n") catch {};
        return cli.exit_failed;
    };

    // The budget (pure resolution now; the sampler after the backend) and
    // the executor: fuse lanes sized to the transport's desired geometry.
    const facts = derive.facts();
    const host: mem_budget.Host = .{ .io = p.io, .env = p.env, .facts = facts };
    const budget_bytes = mem_budget.authority.resolveBudgetNow(host);
    const sqz = SqzIo.init(p.gpa, .{
        .env = p.env,
        .facts = facts,
        .budget_bytes = budget_bytes,
        .budget = &mem_budget.authority,
        .fuse = .{ .nqueues = facts.possible_cpus, .q_depth = fuse_geometry.Q_DEPTH_DESIRED },
    }) catch |e| {
        p.stderr.print("squeezefs mount: executor init failed ({s})\n", .{@errorName(e)}) catch {};
        if (reporter) |*r| {
            r.fail("executor init failed");
            r.close();
        }
        return cli.exit_failed;
    };
    defer sqz.deinit();
    var ctx: Ctx = .{
        .p = p,
        .sqz = sqz,
        .facts = facts,
        .budget_bytes = budget_bytes,
        .budget_host = host,
        .path = meta.first(),
        .mountpoint = mountpoint,
        .opts = opts,
        .reporter = reporter,
    };
    return sqz.run(mountMain, .{&ctx}) catch |e| ctx.failed("the root task could not start ({s})", .{@errorName(e)});
}

/// The root task (module doc).
fn mountMain(ctx: *Ctx) u8 {
    const gpa = ctx.p.gpa;
    const env = ctx.p.env;
    const io = ctx.sqz.io();
    const facts = ctx.facts;

    // 1. The backend: layer A, the ladder, the lane, the checkpoint task.
    const be = backend.KvBackend.open(gpa, io, ctx.path, .{
        .env = env,
        .cache_budget_bytes = node_cache.nodeCacheBudgetFromEnv(env, ctx.budget_bytes),
        .cpus = facts.possible_cpus,
    }) catch |e| return ctx.failed("cannot open metadata volume {s}: {s} ({t}, {s})", .{ ctx.path, openReason(e), backend.openErrno(e), @tagName(errors.classOf(backend.openCoreError(e))) });
    var be_open = true;
    defer if (be_open) be.close();

    // 2. The budget sampler (the registry already holds the fiber stacks;
    //    the arena joins at the session).
    _ = mem_budget.authority.spawnSampler(ctx.budget_host, mem_budget.sampler_period_ns) catch |e| return ctx.failed("cannot start the memory-budget sampler ({s})", .{@errorName(e)});
    defer mem_budget.authority.stopSampler(ctx.p.io);

    // 3. The filesystem and the handler layer.
    var fs = BackendFs.mount(gpa, io, be, .{
        .ttls = ctx.opts.ttls,
        .page_size = facts.page_size,
        .dlm_stripes = dlm.resolveWidth(env, facts, fuse_geometry.Q_DEPTH_DESIRED),
    }) catch return ctx.failed("out of memory mounting the filesystem", .{});
    defer fs.unmount();
    var registry = watchdog.Registry.init(gpa, watchdog.derivedShards(facts.possible_cpus)) catch return ctx.failed("out of memory (watchdog registry)", .{});
    defer registry.deinit();
    var guards = op_guard.Table.init(gpa, op_guard.INODE_GUARD_STRIPES) catch return ctx.failed("out of memory (op guards)", .{});
    defer guards.deinit();
    var opens = open_table.OpenTable.init(gpa, open_table.derivedCapacity(facts.possible_cpus, fuse_geometry.Q_DEPTH_DESIRED)) catch return ctx.failed("out of memory (open table)", .{});
    defer opens.deinit();
    op_trace.arm(env);
    var binding: handler.ring.Binding = .{};
    binding.bind(.{
        .fs = filesystem.Filesystem.of(BackendFs, &fs),
        .io = io,
        .registry = &registry,
        .guards = &guards,
        .opens = &opens,
        .shared_enabled = op_guard.sharedEnabled(env),
    });

    // 4. The FUSE fd (root: inside the session; else fusermount3) and the
    //    session's ordered bring-up.
    var unmount_ctx: UnmountCtx = .{ .io = io };
    var fuse_fd: ?linux.fd_t = null;
    var unmounter: ?transport.session.Unmounter = null;
    if (linux.geteuid() != 0) {
        var ob: [1024]u8 = undefined;
        var ow: Io.Writer = .fixed(&ob);
        ctx.opts.writeFusermountOptions(&ow) catch return ctx.failed("the -o string is too long", .{});
        fuse_fd = fusermount.mount(gpa, io, env, ctx.mountpoint, ow.buffered()) catch |e| return ctx.failed("fusermount3 could not mount {s} ({s})", .{ ctx.mountpoint, @errorName(e) });
        unmounter = .{ .ctx = &unmount_ctx, .call = UnmountCtx.call };
    }
    const session = Session.mount(gpa, ctx.sqz, binding.filesystem(), ctx.mountpoint, env, facts, .{
        .opts = ctx.opts,
        .block_size = DEFAULT_BLOCK_SIZE,
        .mem_budget_bytes = ctx.budget_bytes,
        .budget = &mem_budget.authority,
        .fuse_fd = fuse_fd,
        .unmounter = unmounter,
        .on_shutdown = .{ .ctx = ctx, .call = onSessionShutdown },
    }) catch |e| {
        if (fuse_fd) |fd| {
            _ = fusermount.unmount(io, ctx.mountpoint);
            _ = linux.close(fd);
        }
        return ctx.failed("the FUSE session could not arm at {s} ({s}) — see the log", .{ ctx.mountpoint, @errorName(e) });
    };
    var session_live = true;
    defer if (session_live) session.deinit();

    // 5. The watchdog loop and the reclaim cadence on the meta pool.
    var wd = watchdog.Watchdog.init(&registry);
    var wd_fut = ctx.sqz.spawnMeta(watchdogMain, .{ &wd, io }) catch return ctx.failed("cannot spawn the watchdog", .{});
    var reclaim: ReclaimCadence = .{ .fs = &fs, .io = io };
    var reclaim_fut = ctx.sqz.spawnMeta(ReclaimCadence.run, .{&reclaim}) catch {
        wd.requestStop();
        wd_fut.cancel(io);
        return ctx.failed("cannot spawn the reclaim cadence", .{});
    };

    // 6. Ready.
    log.info("ready: {s} mounted at {s} (pid {d}; {d} queue(s) × depth {d}; budget {d} MiB; TTLs attr {d} ms entry {d} ms dir_entry {d} ms negative {d} ms)", .{ ctx.path, ctx.mountpoint, linux.getpid(), session.plan.nqueues, session.plan.depth, ctx.budget_bytes >> 20, ctx.opts.ttls.attr_ns / std.time.ns_per_ms, ctx.opts.ttls.entry_ns / std.time.ns_per_ms, ctx.opts.ttls.dir_entry_ns / std.time.ns_per_ms, ctx.opts.ttls.negative_ns / std.time.ns_per_ms });
    if (ctx.reporter) |*r| {
        r.ready();
        r.close();
    }

    // 7. Serve until a stop latches or the session ends.
    serve(ctx, session, be);

    // 8. Teardown in order.
    session.close();
    wd.requestStop();
    wd_fut.cancel(io);
    reclaim.requestStop();
    reclaim_fut.cancel(io);
    const failed = be.isFailed();
    const stranded = counters.tripwireCount("open_count_stranded");
    const abandoned = transport.slot.requests_abandoned.load();
    log.info("teardown: session closed; open_count_stranded={d} transport_requests_abandoned={d} tick_rescues={d}/{d}", .{ stranded, abandoned, transport.worker.park_tick_commit_rescues.load(), transport.worker.park_tick_cqe_rescues.load() });
    session_live = false;
    session.deinit();
    // The backend's final checkpoint next; the `defer`s unwind the
    // filesystem, the tables and the sampler after it.
    be_open = false;
    be.close();
    if (failed) {
        log.err("the volume {s} latched FAILED during this mount (EIO until remount) — exiting 1", .{ctx.path});
        return cli.exit_failed;
    }
    return cli.exit_ok;
}

fn openReason(e: backend.OpenError) []const u8 {
    return switch (e) {
        error.Busy => "another writer holds the single-writer guard (flock)",
        error.NotFormatted => "not formatted (zeroed superblock) — run `squeezefs format` first",
        error.LegacyFormat => "a pre-v4 squeezefs format, which this binary does not mount — reformat required",
        error.UnsupportedFormat => "unknown or later-rung capability bits — reformat or a later binary",
        error.Corrupt => "corruption outside the replay window (see the log)",
        error.DeviceRefused => "the device refused registration (O_DIRECT posture) or does not exist",
        error.Io => "device I/O failed",
        error.OutOfMemory => "out of memory",
        error.LaneSpawnFailed, error.ThreadSpawnFailed => "cannot spawn the journal lane or the checkpoint thread",
    };
}

fn watchdogMain(wd: *watchdog.Watchdog, io: Io) void {
    wd.run(io);
}

/// The periodic drain of the corpse gather (module doc).
const ReclaimCadence = struct {
    fs: *BackendFs,
    io: Io,
    stop: std.atomic.Value(bool) = .init(false),

    fn requestStop(self: *ReclaimCadence) void {
        self.stop.store(true, .release);
    }

    fn run(self: *ReclaimCadence) void {
        while (!self.stop.load(.acquire)) {
            self.io.sleep(.fromNanoseconds(RECLAIM_CADENCE_NS), .awake) catch return;
            if (self.stop.load(.acquire)) return;
            var trace = op_trace.OpTrace.begin(0, .other);
            defer trace.finish();
            var ctx: filesystem.Ctx = .{ .unique = 0, .uid = 0, .gid = 0, .pid = 0, .io = self.io, .trace = &trace, .wd_slot = null, .window = &.{} };
            self.fs.reclaimDrain(&ctx) catch |e| log.err("reclaim cadence: drain failed: {t}", .{e});
        }
    }
};

fn serve(ctx: *Ctx, session: *Session, be: *backend.KvBackend) void {
    const efd = daemon.Signals.eventfd();
    while (true) {
        _ = ctx.sqz.readable(efd) catch |e| {
            log.err("mount: waiting on the signal eventfd failed ({t}) — stopping", .{e});
            return;
        };
        daemon.Signals.drain();
        const bits = daemon.Signals.take();
        if (bits & daemon.SIG_STATS != 0) dumpStats(ctx, session, be);
        if (bits & daemon.SIG_STOP != 0) {
            log.info("stop requested (SIGTERM/SIGINT): unmounting {s}", .{ctx.mountpoint});
            return;
        }
        // acquire pairs with the hook's release.
        if (ctx.session_ended.load(.acquire) or !session.isReady()) {
            log.info("the FUSE session ended ({s}): tearing down", .{if (session.isMounted()) "shutdown" else "unmounted"});
            return;
        }
    }
}

/// Every stats family, one log line each (the `.stats` inode's rung-5 JSON,
/// delivered through the log on SIGUSR1).
fn dumpStats(ctx: *Ctx, session: *Session, be: *backend.KvBackend) void {
    var buf: [3072]u8 = undefined;
    var w: Io.Writer = .fixed(&buf);
    session.writeStatsJson(&w) catch {};
    log.info("stats transport: {s}", .{w.buffered()});
    w = .fixed(&buf);
    ctx.sqz.writeStatsJson(&w) catch {};
    log.info("stats exec: {s}", .{w.buffered()});
    w = .fixed(&buf);
    mem_budget.authority.writeStatsJson(&w) catch {};
    log.info("stats mem_budget: {s}", .{w.buffered()});
    log.info("stats daemon: {{\"open_count_stranded\":{d},\"invariant_tripwires\":{d},\"fuse_ops\":{d},\"fuse_op_watchdog_overdue\":{d},\"fuse_ops_inflight\":{d},\"fuse_write_beyond_inline_refused\":{d},\"fuse_reclaim_destroys\":{d},\"fuse_reclaim_drains\":{d},\"exec_lane_tick_rescues\":{d},\"transport_park_tick_commit_rescues\":{d},\"transport_park_tick_cqe_rescues\":{d},\"volume_failed\":{}}}", .{
        counters.tripwireCount("open_count_stranded"),
        counters.invariantTripwires(),
        handler.lifecycle.fuse_ops.load(),
        watchdog.fuse_op_watchdog_overdue.load(),
        watchdog.fuse_ops_inflight.load(),
        handler.kv_fs.fuse_write_beyond_inline_refused.load(),
        handler.kv_fs.fuse_reclaim_destroys.load(),
        handler.kv_fs.fuse_reclaim_drains.load(),
        exec_stats.lane_tick_rescues.load(),
        transport.worker.park_tick_commit_rescues.load(),
        transport.worker.park_tick_cqe_rescues.load(),
        be.isFailed(),
    });
}

const testing = std.testing;

test "the reclaim cadence is one watchdog tick; the default block term is the 4 MiB chunk" {
    try testing.expectEqual(watchdog.TICK_NS, RECLAIM_CADENCE_NS);
    try testing.expectEqual(@as(u64, 4 << 20), DEFAULT_BLOCK_SIZE);
}
