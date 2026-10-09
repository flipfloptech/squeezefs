//! `squeezefs mount` (charter §8; §4 "Bring-up (ordered)"; §5.7 layer A;
//! §9 "`main` drives the CLI with a thread-park `block_on`"): the daemon's
//! bring-up in the charter's order INSIDE `SqzIo.run`, the serve loop, and
//! the teardown in the charter's order. Linux only.
//!
//! Bring-up (the root task on the `main` lane):
//!   resolve the budget (pure) → open the backend (the D0 ladder's layer A:
//!   the flock; a FAILED/refusal class is loud and names its errno through
//!   the ONE table) → the set config (`set_config` in tree 0: present,
//!   decodable, the stamp's set; every data volume's device read and
//!   checked against its record — the §5.4 URI-disagreement refusals; an
//!   encrypted set resolves and verifies its key here — a wrong key file
//!   refuses the mount instead of failing every read) → start the budget
//!   sampler → the handler tables (watchdog registry, op guards, open
//!   table) → the DATA PLANE (`data/plane.zig`: devices, allocators seeded
//!   from the ledger, the router, the reclaimer, staging + the NVMe tier
//!   under the cache paths, the transform keyed from the verified key, the
//!   write and read paths) → `KvFs(KvBackend, DataPlane)` → the cross-
//!   owner plane (open intents rolled forward) → THE CLUSTER WIRE
//!   (`meta/sym/wire_plane.zig`, a symmetric set only: this process won
//!   the D0 ladder of every volume it opened and is their MANAGER — ONE
//!   listener on `SQUEEZEFS_MW_BIND` serving the manager, membership,
//!   death-ledger, token and cross-owner families; the endpoint published
//!   into the RAM table and the durable `endpoint:` record; the owner
//!   shard armed; the read divert bound into the filesystem) → the ring
//!   `Binding` with the plane's queue-thread arms → the FUSE fd: root opens
//!   `/dev/fuse` inside the session's `mount(2)`; anyone else receives it
//!   from `fusermount3` (`fusermount.zig`) → `Session.mount` (INIT,
//!   geometry, `enable_uring`, workers, the arm barrier) → the plane's zc
//!   legs armed from the session's buffer mode → the watchdog loop and the
//!   reclaim cadence on `spawnMeta` → `ready` (logged; written to the
//!   supervisor pipe under `--daemon`).
//!
//! Serve: the main task parks on the daemon's eventfd through its lane ring
//! (`SqzIo.readable`): a signal (`daemon.Signals`) or the session's
//! `on_shutdown` hook (DESTROY from an external `umount`, a disconnect, a
//! failed queue) wakes it. SIGUSR1 logs every `.stats` family through the
//! ONE aggregator (`stats/stats.zig`), one line per family. Stop latches;
//! nothing is aborted from outside.
//!
//! Teardown: `Session.close` (the drains RELEASE every retained zc slot
//! while the connection is live and synthesize every owed reply, then the
//! unmount, the workers join) → the watchdog and the cadence cancelled →
//! the cluster wire stopped (every connection severed, the token hook
//! disarmed on every pass — before the backend it serves over) →
//! `Session.deinit` → the filesystem's dismount sweep → `DataPlane.close`
//! (the dismount pass: staged files promote, packs seal DISMOUNT, the
//! reclaimer drains, the devices close — charter §8 `umount`) →
//! `KvBackend.close` (the final checkpoint: an empty replay window for the
//! next mount) → the sampler → exit 0; a volume that latched FAILED exits
//! 1 naming the reason — AFTER the same `Session.close`, so its slots are
//! released too.
//!
//! Every exit of `mountMain` runs that order: the stop latch (SIGTERM /
//! SIGINT — what `squeezefs umount` sends, having found this daemon's pid
//! in the mount's `.stats`), the session's own end (the `on_shutdown`
//! hook), a failed spawn after the session armed (`defer
//! session.deinit()`), and the FAILED verdict. What no daemon code can
//! cover: an EXTERNAL `umount(8)` (a type-`fuse` mount gets no DESTROY —
//! the kernel aborts the connection and reclaims any retained ent itself,
//! `transport_retained_kernel_drained`) and the signals that run no
//! teardown (`daemon.zig`).

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const Allocator = std.mem.Allocator;
const cli = @import("cli.zig");
const args = @import("args.zig");
const uri = @import("uri.zig");
const daemon = @import("daemon.zig");
const fusermount = @import("fusermount.zig");
const exec = @import("../exec/exec.zig");
const meta_set = @import("../meta/meta_set.zig");
const guard = @import("../meta/guard.zig");
const node_cache = @import("../meta/node_cache.zig");
const dlm = @import("../meta/dlm.zig");
const sym_xv = @import("../meta/sym/xv.zig");
const sym_wire = @import("../meta/sym/wire_plane.zig");
const sym_reader = @import("../meta/sym/reader_plane.zig");
const ro_mount = @import("../meta/sym/tokens/ro_mount.zig");
const wire_endpoint = @import("../wire/endpoint.zig");
const handler = @import("../fuse/handler.zig");
const transport = @import("../fuse/transport.zig");
const fuse_geometry = @import("../fuse/geometry.zig");
const mount_options = @import("../fuse/mount_options.zig");
const mem_budget = @import("../mem/budget.zig");
const key = @import("../meta/key.zig");
const set_config = @import("../data/set_config.zig");
const transform = @import("../data/transform.zig");
const data_superblock = @import("../data/data_superblock.zig");
const plane_mod = @import("../data/plane.zig");
const slot = @import("../fuse/transport/slot.zig");
const counters = @import("../core/counters.zig");
const stats = @import("../stats/stats.zig");
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

/// The verb's state, pinned in `run`'s frame for the root task.
const Ctx = struct {
    p: cli.Process,
    sqz: *SqzIo,
    facts: derive.Facts,
    budget_bytes: u64,
    budget_host: mem_budget.Host,
    /// The member paths as spelled (canonical order is discovery's).
    paths: []const []const u8,
    /// The raw URIs as the operator spelled them (`.config`, `.stats`).
    meta_uri: []const u8,
    data_uri: []const u8,
    mountpoint: [:0]const u8,
    opts: mount_options.MountOptions,
    /// `--join host:port` as spelled (the manager's listener).
    join: ?[]const u8,
    /// The raw `-o` string (the fusermount3 path forwards it).
    reporter: ?daemon.Reporter,
    session_ended: std.atomic.Value(bool) = .init(false),
    /// The verified volume key of an encrypted set (zeroized once the plane
    /// holds its crypto state).
    derived_key: ?transform.DerivedKey = null,

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
    // The URI: every member of the set, in any order (discovery orders
    // them by their stamps).
    var members: [uri.MAX_MEMBERS][]const u8 = undefined;
    var udiag: uri.Refusal = undefined;
    const meta = uri.parse(.meta, m.meta_uri, &members, &udiag) catch {
        p.stderr.print("squeezefs mount: {f}\n", .{udiag}) catch {};
        return cli.exit_usage;
    };
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
        .paths = meta.paths,
        .meta_uri = m.meta_uri,
        .data_uri = "",
        .mountpoint = mountpoint,
        .opts = opts,
        .join = m.join,
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

    // 1. The set: discovery, then every member's ladder, lane and
    //    checkpoint task in canonical order — or, when volume 0's D0
    //    ladder is held by a live manager of a symmetric set, the JOIN
    //    door (charter §5.7): this process becomes an equal writer over
    //    the cluster wire.
    const join_ep: ?wire_endpoint.EndpointWire = if (ctx.join) |text| wire_endpoint.parseEndpoint(text) catch return ctx.failed("--join {s}: not an addr:port / [v6]:port literal", .{text}) else null;
    const be = meta_set.MetaSet.open(gpa, io, ctx.paths, .{
        .env = env,
        .cache_budget_bytes = node_cache.nodeCacheBudgetFromEnv(env, ctx.budget_bytes),
        .cpus = facts.possible_cpus,
        .read_only = ctx.opts.read_only,
        .join = .{ .mountpoint = ctx.mountpoint, .endpoint = join_ep },
    }) catch |e| return ctx.failed("cannot open metadata set {s}: {s} ({t}, {s})", .{ ctx.meta_uri, openReason(e), meta_set.openErrno(e), @tagName(errors.classOf(meta_set.openCoreError(e))) });
    var be_open = true;
    defer if (be_open) be.close();
    // A READER of a symmetric set (charter §5.5): every kernel TTL derives
    // to 0 — a cache may hold an entry exactly as long as the holder's
    // recall proves it; an explicit `-o *_timeout` refuses naming the law.
    const reader = be.isReader();
    if (reader) ctx.opts.ttls = ro_mount.tokenReaderTtls(&ctx.opts) catch return ctx.failed("{s}: a `-o ro` reader's TTLs derive to 0 (see the log)", .{ctx.meta_uri});

    // 1b. The set config: the one control record, the stamp's set, the key
    //     gate on an encrypted set.
    var cfg = loadSetConfig(ctx, be) orelse return cli.exit_failed;
    defer cfg.deinit(gpa);
    const data_uri = dataUriOf(gpa, &cfg) catch return ctx.failed("out of memory (data uri)", .{});
    defer gpa.free(data_uri);
    ctx.data_uri = data_uri;

    // 2. The budget sampler (the registry already holds the fiber stacks;
    //    the arena joins at the session).
    _ = mem_budget.authority.spawnSampler(ctx.budget_host, mem_budget.sampler_period_ns) catch |e| return ctx.failed("cannot start the memory-budget sampler ({s})", .{@errorName(e)});
    var sampler_live = true;
    defer if (sampler_live) mem_budget.authority.stopSampler(ctx.p.io);

    // 3. The handler tables, the data plane, the filesystem, the binding.
    var registry = watchdog.Registry.init(gpa, watchdog.derivedShards(facts.possible_cpus)) catch return ctx.failed("out of memory (watchdog registry)", .{});
    defer registry.deinit();
    var guards = op_guard.Table.init(gpa, op_guard.INODE_GUARD_STRIPES) catch return ctx.failed("out of memory (op guards)", .{});
    defer guards.deinit();
    var opens = open_table.OpenTable.init(gpa, open_table.derivedCapacity(facts.possible_cpus, fuse_geometry.Q_DEPTH_DESIRED)) catch return ctx.failed("out of memory (open table)", .{});
    defer opens.deinit();
    const plane = plane_mod.DataPlane.open(gpa, io, ctx.sqz, be, &cfg, .{ .env = env, .facts = facts, .budget_bytes = ctx.budget_bytes, .derived_key = ctx.derived_key, .guards = &guards }) catch |e| return ctx.failed("the data plane could not open ({s}) — see the log", .{@errorName(e)});
    if (ctx.derived_key) |*k| k.zeroize();
    var plane_open = true;
    defer if (plane_open) plane.close();
    var fs = BackendFs.mount(gpa, io, be, plane, .{
        .ttls = ctx.opts.ttls,
        .page_size = facts.page_size,
        .dlm_stripes = dlm.resolveWidth(env, facts, fuse_geometry.Q_DEPTH_DESIRED),
        .uid = ctx.opts.uid,
        .gid = ctx.opts.gid,
    }) catch return ctx.failed("out of memory mounting the filesystem", .{});
    defer fs.unmount();
    // The `.stats`/`.config`/`.trace` renderer: every family's source the
    // daemon owns (the session joins once it is up); the op-trace ring
    // armed from its row with the derived geometry.
    var vs: VirtualSource = .{
        .renderer = stats.Renderer.init(gpa, .{
            .build = .{ .train = ctx.p.build.train, .commit = ctx.p.build.commit, .profile = ctx.p.build.profile },
            .mount = .{ .uid = ctx.opts.uid, .gid = ctx.opts.gid, .pid = @intCast(linux.getpid()), .mountpoint = ctx.mountpoint, .sqmeta_uri = ctx.meta_uri, .sqdata_uri = ctx.data_uri, .read_only = ctx.opts.read_only, .writeback = ctx.opts.write_back, .direct_device_true = ctx.opts.direct_device_true },
            .env = env,
            .plane = plane,
            .sqz = ctx.sqz,
            .backend = be,
            .io = io,
        }),
        .cfg = &cfg,
    };
    defer vs.renderer.deinit();
    fs.bindVirtual(vs.face());
    op_trace.armTrace(gpa, env, facts.possible_cpus, ctx.budget_bytes) catch return ctx.failed("out of memory (op-trace ring)", .{});
    defer op_trace.disarmTrace();
    // The cross-owner plane (charter §5.5): an op spanning two members or
    // two holders is ONE intent; the open intents roll forward here,
    // before any request is served. A reader has no intents.
    const xv_bound: ?*sym_xv.set_bind.Bound = if (reader) null else sym_xv.set_bind.Bound.create(gpa, io, be, &fs.locks, env) catch return ctx.failed("out of memory (cross-owner plane)", .{});
    defer if (xv_bound) |xb| xb.destroy(gpa);
    if (xv_bound) |xb| {
        const rolled = fs.bindXv(&xb.plane) catch |e| return ctx.failed("the cross-owner roll-forward at mount failed ({t})", .{e});
        if (rolled != 0) log.info("mount: {d} open cross-owner intent(s) rolled forward before serving", .{rolled});
    }
    // The READER's plane (charter §5.5 "`-o ro` mounts are member-readers
    // + token clients … with NO live writer … the QUIESCENT reader"): the
    // divert over every slot as foreign, or the projection with the
    // fail-stop poll; the FUSE layer wraps the filesystem with the gate.
    const rplane: ?*sym_reader.ReaderPlane = if (reader) sym_reader.ReaderPlane.start(gpa, io, be, env, facts.cpus, ctx.budget_bytes, ctx.mountpoint) catch |e| return ctx.failed("the reader plane could not arm: {s}", .{sym_reader.startReason(e)}) else null;
    var rplane_live = true;
    defer if (rplane_live) if (rplane) |rp| rp.stop();
    if (rplane) |rp| {
        fs.bindDivert(&rp.divert);
        plane.bindForeignLayout(rp.foreignLayout());
        vs.renderer.sources.reader = rp;
        vs.renderer.sources.mount.wire_endpoint = rp.endpointText();
    }
    // The cluster wire (charter §5.5, §5.7): this process won the D0
    // ladder of every volume it opened, so it is their MANAGER — ONE
    // listener serving the manager, membership, ledger, token and
    // cross-owner families, the endpoint published for the joiners. A
    // `--single-writer` set arms no wire; a reader's plane is above.
    const wire: ?*sym_wire.WirePlane = if (xv_bound) |xb| sym_wire.WirePlane.start(gpa, io, be, env, facts.cpus, ctx.budget_bytes, &xb.plane, ctx.opts.read_only) catch |e| return ctx.failed("the cluster wire could not arm: {s}", .{sym_wire.startReason(e)}) else null;
    var wire_live = true;
    defer if (wire_live) if (wire) |wp| wp.stop();
    if (wire) |wp| {
        vs.renderer.sources.mount.wire_endpoint = wp.endpointText();
        vs.renderer.sources.wire = wp;
        // The read divert: a foreign slot's records come under a token or
        // answer the typed EAGAIN, never off this mount's cache.
        fs.bindDivert(&wp.divert);
        // The cross-owner plane's step sessions: a foreign holder's steps
        // ship over the wire plane's dialled links.
        xv_bound.?.bindDialer(wp.peers.xvDialer());
        // A foreign file's layout record comes off its holder's token.
        plane.bindForeignLayout(wp.foreignLayout());
        // The data planes' arms: the manager holds the allocation lease
        // and serves the grants; a joiner mints from pulled grants and
        // ships its terminal frees.
        wp.bindDataPlanes(plane) catch |e| return ctx.failed("the data planes could not arm over the wire ({s}) — see the log", .{@errorName(e)});
    }
    // The corpse sweep (kv_fs module doc): what a crashed daemon's forgets
    // left at nlink 0 is destroyed before any request is served — a
    // writer's duty; a reader destroys nothing.
    if (!reader) _ = fs.sweepCorpsesAtMount() catch |e| return ctx.failed("the corpse sweep at mount failed ({t})", .{e});
    op_trace.arm(env);
    var reader_gate: handler.reader_gate.ReaderGate = .{ .inner = filesystem.Filesystem.of(BackendFs, &fs), .gate = undefined };
    if (rplane) |rp| reader_gate.gate = rp.gate();
    var binding: handler.ring.Binding = .{};
    binding.bind(.{
        .fs = if (rplane != null) reader_gate.filesystem() else filesystem.Filesystem.of(BackendFs, &fs),
        .io = io,
        .registry = &registry,
        .guards = &guards,
        .opens = &opens,
        .probe = plane.probe(),
        .shared_enabled = op_guard.sharedEnabled(env),
    });
    binding.bindData(.{ .plane = plane });

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
        .block_size = cfg.block_size,
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
    vs.renderer.sources.session = session;
    // A recalled token's kernel view rides the session's notify seam; so
    // does a served record verb's (the holder's own kernel view).
    if (wire) |wp| {
        const seam = session.tokenNotifySeam();
        wp.sink.bindSession(seam);
        wp.record_svc.sink.kernel = .{ .ctx = seam.ctx, .invalInode = seam.invalInode, .prune = seam.prune };
    }
    if (rplane) |rp| rp.sink.bindSession(session.tokenNotifySeam());
    // The zc legs: the held-slot direct store on a zero-copy session, the
    // direct device leg, the fd-source probe serve.
    var held_store: HeldStore = .{ .session = session };
    plane.armSession(if (session.heldStoreArmed()) held_store.face() else null, session.mode == .zero_copy, session.readZcServe());

    // 5. The watchdog loop and the reclaim cadence on the meta pool.
    var wd = watchdog.Watchdog.init(&registry);
    var wd_fut = ctx.sqz.spawnMeta(watchdogMain, .{ &wd, io }) catch return ctx.failed("cannot spawn the watchdog", .{});
    var reclaim: ReclaimCadence = .{ .fs = &fs, .io = io, .wire = wire };
    var reclaim_fut = ctx.sqz.spawnMeta(ReclaimCadence.run, .{&reclaim}) catch {
        wd.requestStop();
        wd_fut.cancel(io);
        return ctx.failed("cannot spawn the reclaim cadence", .{});
    };

    // 6. Ready.
    log.info("ready: {s} mounted at {s} (pid {d}; {d} queue(s) × depth {d}; budget {d} MiB; TTLs attr {d} ms entry {d} ms dir_entry {d} ms negative {d} ms)", .{ ctx.meta_uri, ctx.mountpoint, linux.getpid(), session.plan.nqueues, session.plan.depth, ctx.budget_bytes >> 20, ctx.opts.ttls.attr_ns / std.time.ns_per_ms, ctx.opts.ttls.entry_ns / std.time.ns_per_ms, ctx.opts.ttls.dir_entry_ns / std.time.ns_per_ms, ctx.opts.ttls.negative_ns / std.time.ns_per_ms });
    if (ctx.reporter) |*r| {
        r.ready();
        r.close();
    }

    // 7. Serve until a stop latches or the session ends.
    serve(ctx, session, &vs);

    // 8. Teardown in order.
    session.close();
    wd.requestStop();
    wd_fut.cancel(io);
    reclaim.requestStop();
    reclaim_fut.cancel(io);
    // The wire BEFORE the planes and the backend it serves over: every
    // connection severed, the token hook disarmed on every pass — and
    // every seam the planes hold INTO it unbound first (the dismount pass
    // at `plane.close` reads layouts; nothing may reach a stopped plane).
    if (wire) |wp| {
        plane.bindForeignLayout(null);
        xv_bound.?.bindDialer(null);
        wp.stop();
    }
    wire_live = false;
    if (rplane) |rp| {
        plane.bindForeignLayout(null);
        rp.stop();
    }
    rplane_live = false;
    vs.renderer.sources.reader = null;
    vs.renderer.sources.mount.wire_endpoint = "";
    vs.renderer.sources.wire = null;
    const failed = be.isFailed();
    const stranded = counters.tripwireCount("open_count_stranded");
    const abandoned = transport.slot.requests_abandoned.load();
    log.info("teardown: session closed; open_count_stranded={d} transport_requests_abandoned={d} tick_rescues={d}/{d}", .{ stranded, abandoned, transport.worker.park_tick_commit_rescues.load(), transport.worker.park_tick_cqe_rescues.load() });
    session_live = false;
    vs.renderer.sources.session = null;
    session.deinit();
    // The sampler stops BEFORE the plane's components (its gauges) go; the
    // dismount pass BEFORE the backend's final checkpoint (its promotions
    // are layout transactions); the `defer`s unwind the filesystem and the
    // tables after it.
    sampler_live = false;
    mem_budget.authority.stopSampler(ctx.p.io);
    plane_open = false;
    plane.close();
    be_open = false;
    be.close();
    if (failed) {
        log.err("a member of {s} latched FAILED during this mount (EIO until remount) — exiting 1", .{ctx.meta_uri});
        return cli.exit_failed;
    }
    return cli.exit_ok;
}

/// Read + decode `set_config` from tree 0 (the record a format writes;
/// its absence or refusal is a mount refusal by name), discover the data
/// volumes (the record names the stamp's set; every non-retired record's
/// device reads back as itself — order-independent, every disagreement
/// refused by name), and on an encrypted set resolve the key through the
/// ONE resolution order (`SQUEEZEFS_ENCRYPT_KEY_FILE` → the key dir;
/// `mount` grows `--encrypt-key` with the data plane) and verify its id.
/// Logs the summary at ready; null = refused (reported).
fn loadSetConfig(ctx: *Ctx, be: *meta_set.MetaSet) ?set_config.Config {
    const gpa = ctx.p.gpa;
    const value = be.lookup(gpa, key.TREE_CONTROL, set_config.CONTROL_KEY) catch |e| {
        _ = ctx.failed("cannot read the set_config record of {s}: {s}", .{ ctx.meta_uri, @errorName(e) });
        return null;
    } orelse {
        _ = ctx.failed("{s}: member 0 carries no set_config record in its control tree — formatted before the record existed; reformat", .{ctx.meta_uri});
        return null;
    };
    defer gpa.free(value);
    var diag: set_config.Refusal = undefined;
    var cfg = set_config.decode(gpa, value, &diag) catch {
        var buf: [512]u8 = undefined;
        var w: Io.Writer = .fixed(&buf);
        diag.write(&w) catch {};
        _ = ctx.failed("{s}: {s}", .{ ctx.meta_uri, w.buffered() });
        return null;
    };
    {
        // Every data device read once (O_DIRECT, both superblock copies)
        // and released: the data plane's own opens come with its wiring.
        const obs = gpa.alloc(set_config.Observation, cfg.data.len) catch {
            _ = ctx.failed("out of memory (data volume discovery)", .{});
            cfg.deinit(gpa);
            return null;
        };
        defer gpa.free(obs);
        for (cfg.data, obs) |d, *o| {
            if (d.state == .retired) {
                o.* = .blank;
                continue;
            }
            const opened = data_superblock.linux_dev.open(gpa, d.uri);
            o.* = data_superblock.observation(if (opened) |op| op.classified else |e| e);
        }
        var ddiag: set_config.DiscoveryRefusal = undefined;
        set_config.discover(&cfg, be.setUuid(), obs, &ddiag) catch {
            var buf: [1024]u8 = undefined;
            var w: Io.Writer = .fixed(&buf);
            ddiag.write(&w) catch {};
            _ = ctx.failed("data volume discovery: {s}", .{w.buffered()});
            cfg.deinit(gpa);
            return null;
        };
    }
    if (cfg.encryption != .none) {
        var path_buf: [128]u8 = undefined;
        var served: []const u8 = "";
        const derived = transform.linux_keys.resolveVolumeKey(ctx.p.env, null, &cfg.kdf_salt, cfg.key_id, &path_buf, &served) catch |e| {
            var buf: [512]u8 = undefined;
            var w: Io.Writer = .fixed(&buf);
            transform.writeKeyFileRefusal(&w, e, if (served.len != 0) served else transform.defaultKeyPath(&path_buf, cfg.key_id)) catch {};
            _ = ctx.failed("encrypted set: {s}", .{w.buffered()});
            cfg.deinit(gpa);
            return null;
        };
        // The KEK and key id travel to the data plane's crypto state (which
        // copies them); the caller zeroizes this copy right after.
        ctx.derived_key = derived;
        log.info("mount: encryption key verified ({s}, id {x}, from {s})", .{ transform.KEY_WRAP_SCHEME_NAME, &cfg.key_id, served });
    }
    var out: Io.Writer.Allocating = .init(gpa);
    defer out.deinit();
    cfg.writeSummary(&out.writer) catch {};
    log.info("mount: set config\n{s}", .{std.mem.trimEnd(u8, out.written(), "\n")});
    return cfg;
}

/// `sqdata://a,b` over the set's non-retired data volumes (the exact
/// string the CLI verbs accept; empty for a meta-only set).
fn dataUriOf(gpa: Allocator, cfg: *const set_config.Config) Allocator.Error![]u8 {
    var out: Io.Writer.Allocating = .init(gpa);
    errdefer out.deinit();
    var first = true;
    for (cfg.data) |d| {
        if (d.state == .retired) continue;
        out.writer.writeAll(if (first) "sqdata://" else ",") catch return error.OutOfMemory;
        out.writer.writeAll(d.uri) catch return error.OutOfMemory;
        first = false;
    }
    return out.toOwnedSlice();
}

fn openReason(e: meta_set.OpenError) []const u8 {
    return switch (e) {
        error.SetRefused => "the members do not form one set (see the log: the discovery refusal names the volumes)",
        error.Busy => "another writer holds the single-writer guard (the flock, or a live writer claim — see the log)",
        error.InvalidIdentity => "SQUEEZEFS_HOSTNQN / SQUEEZEFS_HOSTID must be configured as a pair or not at all",
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
    /// The cluster wire's cadence (eviction, grace reap, the ledger poll)
    /// rides the same fiber; null on a `--single-writer` set.
    wire: ?*sym_wire.WirePlane,
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
            // The cross-owner cadence rides the same meta fiber: the expiry
            // sweep, the roll-forward of abandoned intents, the gauges.
            self.fs.xvCadenceTick();
            if (self.wire) |wp| wp.tick();
        }
    }
};

/// The plane's `write_fixed` face: the session's `storeHeld`.
const HeldStore = struct {
    session: *Session,

    fn face(self: *HeldStore) plane_mod.HeldStoreFace {
        return .{ .ctx = self, .store = &store };
    }

    fn store(ctx: *anyopaque, src: slot.HeldSource, fd: linux.fd_t, off: u64, io: Io) slot.ExtractError!void {
        const self: *HeldStore = @ptrCast(@alignCast(ctx));
        return self.session.storeHeld(src, fd, off, io);
    }
};

fn serve(ctx: *Ctx, session: *Session, vs: *VirtualSource) void {
    const efd = daemon.Signals.eventfd();
    while (true) {
        _ = ctx.sqz.readable(efd) catch |e| {
            log.err("mount: waiting on the signal eventfd failed ({t}) — stopping", .{e});
            return;
        };
        daemon.Signals.drain();
        const bits = daemon.Signals.take();
        if (bits & daemon.SIG_STATS != 0) dumpStats(vs);
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

/// SIGUSR1: every `.stats` family through the ONE aggregator, one log
/// line per family (`stats <name>: {…}`; the daemon line last).
fn dumpStats(vs: *VirtualSource) void {
    stats.logFamilies(&vs.renderer.sources, vs.renderer.gpa);
}

/// The `.stats`/`.config`/`.trace` renderer bound into the filesystem
/// (`kv_fs.VirtualFace`): ONE `stats.Renderer` over the daemon's sources,
/// the set config for `.config`, the op-trace drain for `.trace`.
const VirtualSource = struct {
    renderer: stats.Renderer,
    cfg: *const set_config.Config,

    fn build(ctx: *anyopaque, class: handler.virtual_inodes.Class, gpa: Allocator) Allocator.Error!handler.virtual_inodes.Owned {
        const self: *VirtualSource = @ptrCast(@alignCast(ctx));
        switch (class) {
            .stats => {
                const p = try self.renderer.render();
                return .{ .buf = p.buf, .len = p.len };
            },
            .config => {
                const p = try self.renderer.renderConfig(writeSet, self);
                return .{ .buf = p.buf, .len = p.len };
            },
            .trace => {
                var out: Io.Writer.Allocating = .init(gpa);
                errdefer out.deinit();
                op_trace.writeTraceJson(gpa, &out.writer) catch return error.OutOfMemory;
                const buf = out.writer.buffer;
                const len = out.writer.end;
                out.writer.buffer = &.{};
                return .{ .buf = buf, .len = len };
            },
        }
    }

    fn writeSet(ctx: *anyopaque, w: *Io.Writer) Io.Writer.Error!void {
        const self: *VirtualSource = @ptrCast(@alignCast(ctx));
        try self.cfg.writeJson(w);
    }

    fn face(self: *VirtualSource) handler.kv_fs.VirtualFace {
        return .{ .ctx = self, .build = build };
    }
};

const testing = std.testing;

test "the reclaim cadence is one watchdog tick" {
    try testing.expectEqual(watchdog.TICK_NS, RECLAIM_CADENCE_NS);
}
