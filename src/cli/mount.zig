//! `squeezefs mount` (charter §8; §4 "Bring-up (ordered)"; §5.7 layer A;
//! §9 "`main` drives the CLI with a thread-park `block_on`"): the daemon's
//! bring-up in the charter's order INSIDE `SqzIo.run`, the serve loop, and
//! the teardown in the charter's order. Linux only.
//!
//! Bring-up (the root task on the `main` lane):
//!   resolve the budget (pure) → open the backend (the D0 ladder's layer A:
//!   the flock; a FAILED/refusal class is loud and names its errno through
//!   the ONE table) → the set config (`set_config` in tree 0: present,
//!   decodable, the stamp's set; a data volume with a `fabric_endpoint`
//!   record is CONNECTED by the daemon under its own host identity first
//!   — or a predecessor's association under that identity reused — and
//!   its uri becomes the live node (`ops/nvmet/mount_fabric.zig`; the
//!   §5.2 ladder judges every other volume under an explicit pair); every
//!   data volume's device read and
//!   checked against its record — the §5.4 URI-disagreement refusals; an
//!   encrypted set resolves and verifies its key here — a wrong key file
//!   refuses the mount instead of failing every read) → the handler
//!   tables (watchdog registry, op guards, open table) → the DATA PLANE
//!   (`data/plane.zig`: devices, allocators seeded from the ledger, the
//!   router, the reclaimer, staging + the NVMe tier under the cache paths,
//!   the transform keyed from the verified key, the write and read paths)
//!   → `KvFs(KvBackend, DataPlane)` → the cross-
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
//!   legs armed from the session's zero-copy verdict → the interception host →
//!   THE BUDGET SAMPLER, last: it reads every registered source without a
//!   lock, so it starts once the last registrant exists and is stopped and
//!   JOINED first on every exit (its `defer` sits below every
//!   registrant's) → the watchdog loop and the reclaim cadence on
//!   `spawnMeta` → `ready` (logged; written to the supervisor pipe under
//!   `--daemon`).
//!
//! Serve: the main task parks on the daemon's eventfd through its lane ring
//! (`SqzIo.readable`): a signal (`daemon.Signals`) or the session's
//! `on_shutdown` hook (DESTROY from an external `umount`, a disconnect, a
//! failed queue) wakes it. SIGUSR1 logs every `.stats` family through the
//! ONE aggregator (`stats/stats.zig`), one line per family. A stop
//! (SIGTERM/SIGINT) is REFUSED while the mount is busy — the holder census
//! names them, `transport_umount_busy_refusals` counts it, the daemon
//! serves on (decision 0003 §S); the lazy stop (SIGUSR2) detaches a held
//! mount and serves it until the kernel ends the connection. Nothing is
//! aborted from outside.
//!
//! Teardown: the sampler stopped and joined (every source still live) →
//! `Session.close` (the drains RELEASE every retained zc slot while the
//! connection is live and synthesize every owed reply, then the unmount,
//! the workers join) → the cadences cancelled (the watchdog runs on to the
//! very end: its tick is the supervisor's heartbeat) → the
//! cluster wire stopped (every connection severed, the token hook
//! disarmed on every pass — before the backend it serves over) →
//! `Session.deinit` → the filesystem's dismount sweep → `DataPlane.close`
//! (the dismount pass: staged files promote, packs seal DISMOUNT, the
//! reclaimer drains, the devices close — charter §8 `umount`) →
//! `KvBackend.close` (the final checkpoint: an empty replay window for the
//! next mount) → the daemon's own fabric associations released
//! (`disconnect --identity-only`: exactly the controllers of its records'
//! subsystems under its pair, after every device fd closed) → exit 0; a
//! volume that latched FAILED exits 1 naming the reason — AFTER the same
//! `Session.close`, so its slots are released too.
//!
//! Every exit of `mountMain` runs that order: the stop latch (SIGTERM /
//! SIGINT — what `squeezefs umount` sends, having found this daemon's pid
//! in the mount's `.stats`), the session's own end (the `on_shutdown`
//! hook), a failed spawn after the session armed (`defer
//! session.deinit()`), and the FAILED verdict. What no daemon code can
//! cover: an EXTERNAL `umount(8)` (a type-`fuse` mount gets no DESTROY —
//! the kernel aborts the connection itself) and the signals that run no
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
const journal_lane = @import("../meta/journal_lane.zig");
const journal_tail = @import("../meta/journal_tail.zig");
const fusermount = @import("fusermount.zig");
const holders = @import("holders.zig");
const exec = @import("../exec/exec.zig");
const meta_set = @import("../meta/meta_set.zig");
const guard = @import("../meta/guard.zig");
const node_cache = @import("../meta/node_cache.zig");
const dlm = @import("../meta/dlm.zig");
const sym_xv = @import("../meta/sym/xv.zig");
const sym_slots = @import("../meta/sym/slots.zig");
const sym_wire = @import("../meta/sym/wire_plane.zig");
const wire_tls = @import("../wire/tls.zig");
const sym_reader = @import("../meta/sym/reader_plane.zig");
const sym_park = @import("../meta/sym/membership/park_reclaim.zig");
const extent_grant = @import("../meta/sym/extent_grant.zig");
const lease_clock = @import("../meta/guard/lease_clock.zig");
const ro_mount = @import("../meta/sym/tokens/ro_mount.zig");
const token_table = @import("../meta/sym/tokens/token_table.zig");
const wire_endpoint = @import("../wire/endpoint.zig");
const handler = @import("../fuse/handler.zig");
const transport = @import("../fuse/transport.zig");
const fuse_geometry = @import("../fuse/geometry.zig");
const mount_options = @import("../fuse/mount_options.zig");
const atime = @import("../fuse/atime.zig");
const mountinfo = @import("../core/mountinfo.zig");
const mem_budget = @import("../mem/budget.zig");
const nvme_dev = @import("../io/nvme_dev.zig");
const cache_core = @import("../mem/cache.zig");
const key = @import("../meta/key.zig");
const set_config = @import("../data/set_config.zig");
const lifecycle_stats = @import("../ops/lifecycle/stats.zig").stats;
const transform = @import("../data/transform.zig");
const data_superblock = @import("../data/data_superblock.zig");
const plane_mod = @import("../data/plane.zig");
const write_admit = @import("../data/write_admit.zig");
const write_epoch = @import("../data/write_epoch.zig");
const slot = @import("../fuse/transport/slot.zig");
const counters = @import("../core/counters.zig");
const stats = @import("../stats/stats.zig");
const ipc_host = @import("../ipc/host.zig");
const ring_inodes = @import("../ipc/ring_inodes.zig");
const ipc_rl = @import("../ipc/rendezvous_linux.zig");
const derive = @import("../core/derive.zig");
const knobs = @import("../core/knobs.zig");
const errors = @import("../core/errors.zig");
const log = @import("../core/log.zig");
const futex = @import("../sync/futex.zig");
const mpmc = @import("../sync/mpmc.zig");
const tick_law = @import("../sync/tick.zig");
const thread_name = @import("../core/thread_name.zig");
const record_mod = @import("../meta/record.zig");
const conveyor = @import("../meta/conveyor.zig");
const tx = @import("../meta/tx.zig");
const route_table = @import("../meta/routing/route_table.zig");
const sym_manager = @import("../meta/sym/manager.zig");
const keys = @import("../ops/keys.zig");
const jobs = @import("../ops/jobs.zig");
const mount_fabric = @import("../ops/nvmet/mount_fabric.zig");
const proc = @import("../meta/guard/proc.zig");
const sym_join = @import("../meta/sym/join.zig");
const sym_joined = @import("../meta/sym/joined.zig");
const fsck_runner = @import("../ops/fsck/runner.zig");
const data_bitmap = @import("../data/sym_planes/data_bitmap.zig");
const checkpoint = @import("../meta/checkpoint.zig");
const sync = @import("../sync/sync.zig");
const clock = @import("../core/clock.zig");

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

/// How long a no-open owed ino's latest write must be past before the
/// reclaim cadence drains it (decision 0003 §AF): the rewrite epoch's own
/// idle horizon (`write_epoch.EPOCH_IDLE_HORIZON_MS`, the metadata cache's
/// TTI / 10 = 30 s) — the charter's bound for write custody nothing else
/// closes. Long enough that a writer pausing between bursts keeps its
/// parked blocks (the W2 fold "at the last moment", the accumulation's
/// whole-block upload); a close's FLUSH drains at once regardless.
pub const OWED_IDLE_NS: u64 = write_epoch.EPOCH_IDLE_HORIZON_MS * std.time.ns_per_ms;

/// The verb's state, pinned in `run`'s frame for the root task.
const Ctx = struct {
    p: cli.Process,
    sqz: *SqzIo,
    facts: derive.Facts,
    budget_bytes: u64,
    /// The write cache (the staging ring in RAM): `SQUEEZEFS_WRITE_CACHE_PCT`
    /// of the system root under the budget; 0 = none.
    write_cache_bytes: u64,
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
    /// The supervisor's shared page (`--daemon --supervise`): the watchdog
    /// tick beats it for the mount's whole life, teardown included.
    heartbeat: ?*cli.supervisor.Heartbeat = null,
    session_ended: std.atomic.Value(bool) = .init(false),
    /// The kernel ended the FUSE connection under the live mount (the
    /// request timeout or a fusectl abort): fail-stopped, the exit is 1.
    connection_aborted: bool = false,
    /// The verified volume key of an encrypted set (zeroized once the plane
    /// holds its crypto state).
    derived_key: ?transform.DerivedKey = null,
    /// An open rotation window's next key (the key dir), likewise.
    derived_next: ?transform.DerivedKey = null,
    /// `--encrypt-key`'s material, read in the INVOKING process before any
    /// fork (`ops/keys.zig`): consumed and zeroized by `loadSetConfig`.
    stash: ?*keys.SecretBuf = null,
    /// The daemon-owned fabric associations of the data volumes with a
    /// `fabric_endpoint` record (`ops/nvmet/mount_fabric.zig`): bound by
    /// `loadSetConfig` before discovery, released after the plane closed.
    fabric: mount_fabric.Plan,

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

/// The watchdog's census hook (`watchdog.CensusHook`): the transport's
/// worker census, then the metadata set's per-member census.
const WedgeCensus = struct {
    session: *Session,
    set: *meta_set.MetaSet,

    fn hook(self: *WedgeCensus) watchdog.CensusHook {
        return .{ .ctx = self, .call = &call };
    }

    fn call(ctx: *anyopaque) void {
        const self: *WedgeCensus = @ptrCast(@alignCast(ctx));
        Session.logWorkerCensus(self.session);
        // The executor's lost-wake absorbers: a park the scheduler itself
        // never woke shows here first.
        log.err("exec census: exec_lane_tick_rescues={d} exec_driven_owner_queued_parks={d}", .{ exec.stats.lane_tick_rescues.load(), exec.stats.driven_owner_queued_parks.load() });
        self.set.logCensus();
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
    fn call(ctx_ptr: *anyopaque, mountpoint: [:0]const u8, lazy: bool) bool {
        const self: *UnmountCtx = @ptrCast(@alignCast(ctx_ptr));
        return if (lazy) fusermount.unmountLazy(self.io, mountpoint) else fusermount.unmount(self.io, mountpoint);
    }
};

pub fn run(p: cli.Process, m: args.Mount) u8 {
    // `cli.run` lifted RLIMIT_RTTIME; a hard limit it could not raise is
    // a SIGKILL waiting for the first boosted thread (`rt_limit`).
    if (cli.rt_limit.lift()) |us| log.warn("RLIMIT_RTTIME is {d} µs and this process may not raise it: a daemon thread the kernel boosts to real time (an RCU reader preempted under load) is SIGKILLed past it — raise it before mounting (`ulimit -R unlimited` as root, `LimitRTTIME=infinity` in a unit)", .{us});
    // The URI: every member of the set, in any order (discovery orders
    // them by their stamps).
    var members: [uri.MAX_MEMBERS][]const u8 = undefined;
    var udiag: uri.Refusal = undefined;
    const meta = uri.parse(.meta, m.meta_uri, &members, &udiag) catch {
        p.stderr.print("squeezefs mount: {f}\n", .{udiag}) catch {};
        return cli.exit_usage;
    };
    // The mountpoint: an existing directory. `AT_STATX_DONT_SYNC`: when a
    // FUSE mount of ours stands there mid-teardown (the previous mount's
    // unmount racing this one — the guard waits it out, `guard/proc.zig`),
    // the cached root attrs answer and no request reaches the exiting
    // daemon (its drains would answer it an error).
    var mp_buf: [linux.PATH_MAX:0]u8 = undefined;
    const mountpoint = std.fmt.bufPrintSentinel(&mp_buf, "{s}", .{m.mountpoint}, 0) catch {
        p.stderr.writeAll("squeezefs mount: the mountpoint path is too long\n") catch {};
        return cli.exit_usage;
    };
    {
        var st: linux.Statx = undefined;
        const rc = linux.statx(linux.AT.FDCWD, mountpoint.ptr, linux.AT.STATX_DONT_SYNC, .{ .TYPE = true }, &st);
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
    // KD-11: an interception mount forces kernel write-through; an explicit
    // writeback beside it is a conflict refused by name.
    opts.write_back = mount_options.resolveInterceptionPosture(opts.interception, opts.write_back) catch {
        p.stderr.writeAll("squeezefs mount: mount option conflict: -o interception forces kernel write-through and cannot be combined with -o writeback; drop one of the two options\n") catch {};
        return cli.exit_usage;
    };

    // `--encrypt-key`: the material into a pinned stash BEFORE the fork
    // (so `-` works for a daemonized mount and a bad key file fails on the
    // caller's console); it crosses the forks in memory, never argv, never
    // the pipe. The set's salt is not known yet — the derivation runs at
    // the set-config gate off the stash.
    var stash_storage: keys.SecretBuf = undefined;
    var stash: ?*keys.SecretBuf = null;
    if (m.encrypt_key) |flag_path| {
        stash_storage = keys.linux_verbs.readStash(p.gpa, flag_path) catch |e| switch (e) {
            error.OutOfMemory => {
                p.stderr.writeAll("squeezefs mount: out of memory (key material)\n") catch {};
                return cli.exit_failed;
            },
            else => |ke| {
                var kb: [512]u8 = undefined;
                var kw: Io.Writer = .fixed(&kb);
                transform.writeKeyFileRefusal(&kw, ke, flag_path) catch {};
                p.stderr.print("squeezefs mount: {s}\n", .{kw.buffered()}) catch {};
                return cli.exit_failed;
            },
        };
        stash = &stash_storage;
    }
    defer if (stash) |st| st.deinit();

    // The cluster wire's channel class (SQUEEZEFS_WIRE_CHANNEL): under
    // `mutual_tls` the CA, the node certificate and its key are loaded
    // NOW — before the fork, so a CA-less or malformed configuration fails
    // on the caller's console, never over the ready pipe; the identity
    // crosses the fork in memory like the encrypt-key stash.
    var wire_posture = wire_tls.linux_files.installFromEnv(p.gpa, p.env) catch |e| {
        var tb: [1024]u8 = undefined;
        var tw: Io.Writer = .fixed(&tb);
        wire_tls.writeInstallRefusal(&tw, e, wire_tls.Paths.fromEnv(p.env)) catch {};
        p.stderr.print("squeezefs mount: cluster wire: {s}\n", .{tw.buffered()}) catch {};
        return cli.exit_failed;
    };
    defer wire_posture.deinit(p.gpa);

    // No write cache without a supervisor (charter §6.1, decision 0003
    // §BR.6): the ring is acked unsynced custody only a supervisor keeps
    // across a crash, so an unsupervised mount runs without it — and one
    // that asks for it by name is refused here, on the caller's console. A
    // `-o ro` reader holds no write cache either way.
    if (!(m.daemon and m.supervise) and !opts.read_only) if (knobs.optIntKnob("SQUEEZEFS_WRITE_CACHE_PCT", u64, p.env)) |pct| if (pct > 0) {
        p.stderr.print("squeezefs mount: SQUEEZEFS_WRITE_CACHE_PCT={d} asks for a write cache, and this mount has no supervisor to hold its ring across a crash (no write cache without a supervisor: a crash would lose its acked unsynced writes and leave their sizes ahead of their bytes) — mount with `--daemon --supervise --log-file <file>`, or unset it\n", .{pct}) catch {};
        return cli.exit_usage;
    };

    // `--daemon`: the log file first (its refusal reaches the terminal),
    // then the fork — before any thread exists.
    var reporter: ?daemon.Reporter = null;
    var heartbeat: ?*cli.supervisor.Heartbeat = null;
    if (m.daemon) {
        var lf_buf: [linux.PATH_MAX:0]u8 = undefined;
        const lfz = std.fmt.bufPrintSentinel(&lf_buf, "{s}", .{m.log_file.?}, 0) catch {
            p.stderr.writeAll("squeezefs mount: the --log-file path is too long\n") catch {};
            return cli.exit_usage;
        };
        const log_fd = daemon.openLogFile(lfz, p.stderr) catch return cli.exit_failed;
        p.stderr.flush() catch {};
        switch (daemon.detach(p.gpa, p.io, .{ .supervise = m.supervise, .mountpoint = mountpoint, .log_fd = log_fd, .policy = cli.supervisor.Policy.fromEnv(p.env), .scrub = if (stash) |st| st.bytes else null }, p.stderr)) {
            // The parent zeroizes its copy right after the fork returns
            // (the `defer` above): the child owns the key material now.
            .parent => |code| return code,
            .daemon => |r| {
                reporter = r.reporter;
                heartbeat = r.heartbeat;
                // C2: re-harden (a forked child's dumpable flag may be
                // cleared again by a later exec-less transition; the
                // reference re-issues it on every call for the same reason).
                if (stash != null) transform.linux_keys.hardenProcess();
            },
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
    // The write cache only where a supervisor holds its ring (the
    // channel exists: `--supervise` and its socketpair).
    const write_cache_bytes = if (journal_tail.hasSupervisor()) cache_core.resolveBound(knobs.intKnob("SQUEEZEFS_WRITE_CACHE_PCT", u64, p.env), facts, budget_bytes) else 0;
    // The pinned device pools follow the budget (before the first open).
    nvme_dev.sizePoolsForBudget(budget_bytes);
    const sqz = SqzIo.init(p.gpa, .{
        .env = p.env,
        .facts = facts,
        .budget_bytes = budget_bytes,
        .budget = &mem_budget.authority,
        .fuse = .{ .nqueues = facts.possible_cpus, .q_depth = fuse_geometry.Q_DEPTH_DESIRED, .rings_per_queue = fuse_geometry.MAX_RINGS_PER_QUEUE },
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
        .write_cache_bytes = write_cache_bytes,
        .budget_host = host,
        .paths = meta.paths,
        .meta_uri = m.meta_uri,
        .data_uri = "",
        .mountpoint = mountpoint,
        .opts = opts,
        .join = m.join,
        .reporter = reporter,
        .heartbeat = heartbeat,
        .stash = stash,
        .fabric = mount_fabric.Plan.init(p.gpa),
    };
    defer ctx.fabric.deinit();
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
    // The journal tails and the writer guards to the supervisor (decision
    // 0003 §BB.2–BB.3), before `ready`: from here a crash of this process
    // is replayed to the device under the dead claim. Without a supervisor
    // the posture is said once: the coalesced cadence's window applies.
    announceTails(be, env);
    // The daemon's own fabric associations go AFTER the plane closed its
    // device fds (this `defer` sits below the plane's) and before the
    // metadata set (which they never served).
    defer ctx.fabric.release();
    // Every volume's node cache is an R5 component (`kv_node_cache`: floor
    // 64 MiB, weight 1, shed = a checkpoint kick): rung 9 found the caches
    // unregistered — their RAM was the largest unaccounted part of Σ gauges
    // vs RSS under the suites (`docs/evidence/rung-09-memory-footprint.md`).
    // Through the backend: a joiner swaps its cache while it polls.
    for (be.volumes) |vol| mem_budget.authority.register(vol.budgetComponent());
    // A READER of a symmetric set (charter §5.5): every kernel TTL derives
    // to 0 — a cache may hold an entry exactly as long as the holder's
    // recall proves it; an explicit `-o *_timeout` refuses naming the law.
    const reader = be.isReader();
    if (reader) ctx.opts.ttls = ro_mount.tokenReaderTtls(&ctx.opts) catch return ctx.failed("{s}: a `-o ro` reader's TTLs derive to 0 (see the log)", .{ctx.meta_uri});

    // 1b. The set config: the one control record, the stamp's set, the key
    //     gate on an encrypted set.
    var cfg = loadSetConfig(ctx, be) orelse return cli.exit_failed;
    defer cfg.deinit(gpa);
    // The set's directory spread depth (charter §5.4; decision 0003 §BZ):
    // the record's word, before anything can mint.
    be.setDirSpreadDepth(cfg.dir_spread_depth);
    const data_uri = dataUriOf(gpa, &cfg) catch return ctx.failed("out of memory (data uri)", .{});
    defer gpa.free(data_uri);
    ctx.data_uri = data_uri;

    // 2. The handler tables, the data plane, the filesystem, the binding.
    var registry = watchdog.Registry.init(gpa, watchdog.derivedShards(facts.possible_cpus)) catch return ctx.failed("out of memory (watchdog registry)", .{});
    defer registry.deinit();
    var guards = op_guard.Table.init(gpa, op_guard.INODE_GUARD_STRIPES) catch return ctx.failed("out of memory (op guards)", .{});
    defer guards.deinit();
    var opens = open_table.OpenTable.init(gpa, open_table.derivedCapacity(facts.possible_cpus, fuse_geometry.Q_DEPTH_DESIRED)) catch return ctx.failed("out of memory (open table)", .{});
    defer opens.deinit();
    const plane = plane_mod.DataPlane.open(gpa, io, ctx.sqz, be, &cfg, .{ .env = env, .facts = facts, .budget_bytes = ctx.budget_bytes, .write_cache_bytes = ctx.write_cache_bytes, .member_identity = ctx.fabric.memberIdentity(), .derived_key = ctx.derived_key, .derived_next = ctx.derived_next, .guards = &guards, .mount = .{ .unsupervised = !journal_tail.hasSupervisor() } }) catch |e| return ctx.failed("the data plane could not open ({s}) — see the log", .{@errorName(e)});
    // The write cache's memfd and claim to the supervisor (charter §6.1
    // "Staged"): from here a crash of this process keeps every resident
    // for the member's next mount.
    announceWriteCache(plane, env);
    if (ctx.derived_key) |*k| k.zeroize();
    if (ctx.derived_next) |*k| k.zeroize();
    var plane_open = true;
    defer if (plane_open) plane.close();
    const flush_interval_ms = checkpoint.flushIntervalMsFromEnv(env);
    // The TTL law (decision 0003 §AB.2): a joiner and a reader start
    // (and stay) PEERED; a manager or a `--single-writer` set starts
    // ISOLATED until a member joins — an interception mount too (§CE): the
    // host's limiter invalidates every change its ring makes.
    var coherence: KernelCoherence = .{
        .io = io,
        .gpa = gpa,
        .law = mount_options.TtlLaw.init(ctx.opts.ttls, ctx.opts.explicit_ttls | mount_options.envExplicitTtls(env), mount_options.isolatedTtlNs(lease_clock.T_OWNER_MS), reader or be.volumes[0].joined != null),
        .opens = &opens,
    };
    coherence.init() catch return ctx.failed("out of memory (the kernel view's queue)", .{});
    defer coherence.deinit();
    handler.kv_fs.fuse_ttl_law_peered.set(@intFromBool(coherence.law.peered.load(.acquire)));
    for (be.volumes) |vol| if (vol.sym) |s| {
        s.manager.peer_gate = &coherence.gate;
    };
    // The interception host's ring table (decision 0003 §CE): the host's
    // limiter keeps its words there, a kernel size set through the
    // filesystem lowers their high-water; it outlives both.
    var ring: ?ring_inodes.Table = if (ctx.opts.interception and !reader) ring_inodes.Table.init(gpa, op_guard.INODE_GUARD_STRIPES) catch return ctx.failed("out of memory (the interception ring table)", .{}) else null;
    defer if (ring) |*r| r.deinit(gpa);
    var fs = BackendFs.mount(gpa, io, be, plane, .{
        .ttls = ctx.opts.ttls,
        .ttl_law = &coherence.law,
        .ring = if (ring) |*r| r else null,
        .page_size = facts.page_size,
        .dlm_stripes = dlm.resolveWidth(env, facts, fuse_geometry.Q_DEPTH_DESIRED),
        .uid = ctx.opts.uid,
        .gid = ctx.opts.gid,
        .times_park = flush_interval_ms != 0,
        .write_postlude_hold = knobs.boolKnob("SQUEEZEFS_TEST_WRITE_POSTLUDE_HOLD", env),
        .syncfs_keeps_custody = knobs.boolKnob("SQUEEZEFS_TEST_SYNCFS_KEEPS_CUSTODY", env),
        .foreign_claim_hold = claimHoldPath(env),
        .promote_kill_after_commit = knobs.boolKnob("SQUEEZEFS_TEST_PROMOTE_KILL_AFTER_COMMIT", env),
        .landing_hold = switch (knobs.intKnob("SQUEEZEFS_TEST_LANDING_HOLD_AT_WRITE", u64, env)) {
            0 => null,
            else => |n| .{ .at_write = n, .fire = &holdLandingFromWrite },
        },
    }) catch return ctx.failed("out of memory mounting the filesystem", .{});
    defer fs.unmount();
    // The corpse gather is the data plane's upstream supply (decision 0003
    // §CR): a full store kicks its destroy. Unbound before the filesystem
    // goes (the serve path unbinds it before the plane closes).
    const corpse_upstream = fs.corpseUpstream();
    plane.bindCorpseSupply(&corpse_upstream);
    defer if (plane_open) plane.bindCorpseSupply(null);
    // The `.stats`/`.config`/`.trace` renderer: every family's source the
    // daemon owns (the session joins once it is up); the op-trace ring
    // armed from its row with the derived geometry.
    var vs: VirtualSource = .{
        .sqz = ctx.sqz,
        .renderer = stats.Renderer.init(gpa, .{
            .build = .{ .train = ctx.p.build.train, .commit = ctx.p.build.commit, .profile = ctx.p.build.profile },
            .mount = .{ .uid = ctx.opts.uid, .gid = ctx.opts.gid, .pid = @intCast(linux.getpid()), .mountpoint = ctx.mountpoint, .sqmeta_uri = ctx.meta_uri, .sqdata_uri = ctx.data_uri, .read_only = ctx.opts.read_only, .writeback = ctx.opts.write_back, .direct_device_true = ctx.opts.direct_device_true, .interception = ctx.opts.interception },
            .env = env,
            .plane = plane,
            .sqz = ctx.sqz,
            .backend = be,
            .io = io,
        }),
        .cfg = &cfg,
    };
    defer vs.renderer.deinit();
    vs.renderer.sources.fabric = &ctx.fabric;
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
        // Directory striping (charter §5.5) over the plane, BEFORE the wire
        // serves: the census floor is the slot plane's live `N_floor`.
        _ = fs.bindStripes(xb.plane.stripe_k, stripeFloor(be)) catch return ctx.failed("out of memory (directory striping)", .{});
    }
    defer fs.unbindStripes();
    // The READER's plane (charter §5.5 "`-o ro` mounts are member-readers
    // + token clients … with NO live writer … the QUIESCENT reader"): the
    // divert over every slot as foreign, or the projection with the
    // fail-stop poll; the FUSE layer wraps the filesystem with the gate.
    const rplane: ?*sym_reader.ReaderPlane = if (reader) sym_reader.ReaderPlane.start(gpa, io, be, env, facts.cpus, ctx.budget_bytes, ctx.mountpoint) catch |e| return ctx.failed("the reader plane could not arm: {s}", .{sym_reader.startReason(e)}) else null;
    var rplane_live = true;
    defer if (rplane_live) if (rplane) |rp| rp.stop();
    if (rplane) |rp| {
        rp.divert.setKernelPin(io, coherence.law.peered_ttls.longestNs());
        fs.bindDivert(&rp.divert);
        plane.bindForeignLayout(rp.foreignLayout());
        // A recall purges the plane's tiers of the object's blocks before
        // it acks (the plane outlives the reader plane).
        rp.sink.bindPlane(plane);
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
        // A foreign reader's grant lands this mount's custody of the object
        // first: the write cache's residents promoted, every other write
        // custody landed and its size claim journaled (charter §6.1
        // coherence; `KvFs.landForGrant`).
        wp.holder.prepare = .{ .ctx = &fs, .run = &landForGrant, .pending = &grantPending, .size_epoch = &grantSizeEpoch };
        // ... and this mount's own extending write on an object a member
        // holds a grant on lands before its ack (register-then-check's
        // writer half, `KvFs.landIfWatched`).
        fs.bindForeignWatch(.{ .ctx = &wp.holder, .held = &holderHeld });
        // The read divert: a foreign slot's records come under a token or
        // answer the typed EAGAIN, never off this mount's cache; a token
        // whose records served the kernel stays this mount's at its holder
        // for the kernel's longest peered TTL (decision 0003 §AG.2) — and,
        // as the holder, this mount keeps a recalled reader registered as
        // long, so its next change reaches a kernel that still serves what
        // the token told it.
        wp.divert.setKernelPin(io, coherence.law.peered_ttls.longestNs());
        wp.holder.setKernelPin(io, coherence.law.peered_ttls.longestNs());
        fs.bindDivert(&wp.divert);
        // The cross-owner plane's step sessions: a foreign holder's steps
        // ship over the wire plane's dialled links.
        xv_bound.?.bindDialer(wp.peers.xvDialer());
        // A foreign file's layout record comes off its holder's token; a
        // foreign file's layout PUBLISH ships to its holder as ONE record
        // verb, and a shipped publish served here stages its refs through
        // the plane.
        plane.bindForeignLayout(wp.foreignLayout());
        plane.bindForeignPublish(wp.foreignPublish());
        wp.bindServedPublish(plane.servedPublishFace());
        wp.bindServedTruncate(fs.servedTruncateFace());
        // A recall purges the plane's tiers of the object's blocks before
        // it acks (the plane outlives the wire).
        wp.sink.bindPlane(plane);
        // The data planes' arms: the manager holds the allocation lease
        // and serves the grants; a joiner mints from pulled grants and
        // ships its terminal frees.
        wp.bindDataPlanes(plane) catch |e| return ctx.failed("the data planes could not arm over the wire ({s}) — see the log", .{@errorName(e)});
    }
    // THE JOB FABRIC (charter §8 "the coordinator is volume 0's manager"):
    // every writer runs a node (the local pool, the bodies over the plane);
    // volume 0's MANAGER is the coordinator — it serves the job block and
    // the admin lane on the ONE listener and alone submits and adopts (a
    // joiner's `job:` records are its projection of the manager's tree 0).
    // Admission opens once the allocation lease armed (the grant plane is
    // the mover's mint source); a reader runs no fabric.
    var node: ?*jobs.coordinator.Node = null;
    var data_arm: DataArmCtx = .{ .wire = wire };
    if (!reader) {
        seedJobIds(io);
        const local = proc.localIdentity();
        const identity: jobs.records.StableIdentity = if (wire) |wp| .{ .node_token = wp.identity.node_token, .mount_slot = wp.identity.mount_slot } else .{ .node_token = sym_join.nodeToken(local), .mount_slot = 0 };
        const n = jobs.coordinator.Node.create(gpa, io, jobs.coordinator.Options.fromEnv(env, facts), .{
            .backend = be.volumes[0],
            .plane = plane,
            .identity = identity,
            .pid = @intCast(linux.getpid()),
            .wero = null,
            .held = true,
            .random = &jobRandom,
            .data_arm = if (wire != null) data_arm.arm() else null,
            .meta_set = be,
            // A joined member is a fleet READ worker over its own manager link.
            // Through the LIVE incarnation's session (a re-join in place
            // re-binds it with the rest, decision 0003 §BS).
            .fleet_caller = if (be.volumes[0].joined != null) jobs.wire.Caller{ .ctx = wire.?, .call = &sym_wire.WirePlane.liveManagerCall } else null,
            .bitmaps = if (wire) |wp| fsck_runner.linux.BitmapFace{ .ctx = wp, .bitmapOf = &holderBitmapOf } else null,
        }) catch return ctx.failed("out of memory (job fabric)", .{});
        node = n;
        n.setManager(if (wire) |wp| wp.posture == .manager else true);
        n.bindFsck(be, &fs, env, ctx.budget_bytes) catch |e| return ctx.failed("the fsck engine could not bind over the live set ({s})", .{@errorName(e)});
        mem_budget.authority.register(n.budgetComponent());
        n.start(if (wire) |wp| &wp.svc else null) catch |e| return ctx.failed("the job fabric could not start ({s}) — see the log", .{@errorName(e)});
        // A remote worker's connection ending expires its shard leases.
        if (wire) |wp| wp.listener.bindDeparture(n.departure());
        n.openAdmission();
    }
    var node_live = true;
    defer if (node_live) if (node) |n| {
        if (wire) |wp| wp.listener.bindDeparture(null);
        n.stop(if (wire) |wp| &wp.svc else null);
        n.destroy();
    };
    // The corpse sweep (kv_fs module doc): what a crashed daemon's forgets
    // left at nlink 0 is destroyed before any request is served — a
    // writer's duty; a reader destroys nothing.
    if (!reader) _ = fs.sweepCorpsesAtMount() catch |e| return ctx.failed("the corpse sweep at mount failed ({t})", .{e});
    op_trace.arm(env);
    var reader_gate: handler.reader_gate.ReaderGate = .{ .inner = filesystem.Filesystem.of(BackendFs, &fs), .gate = undefined };
    if (rplane) |rp| reader_gate.gate = rp.gate();
    // THE MOVE GATE (decision 0003 §X.1) outermost over the filesystem the
    // FUSE session and the interception host call: every op an in-flight
    // slot move refuses is re-issued by the daemon — parked on the mount's
    // move epoch (every member's lease table bumps it), bounded by the
    // lattice's T_park_max — never answered EAGAIN. A `--single-writer`
    // mount has no wire and no move: the gate is not armed.
    // The set's tables keep the pointer to the end: this frame outlives
    // the set's close on every exit.
    var moves = sym_slots.moves.Moves.init(io);
    be.bindMoves(&moves);
    plane.bindMoves(&moves);
    const move_deadline_ms: ?u64 = if (wire) |wp| wp.moveDeadlineMs() else if (rplane) |rp| rp.moveDeadlineMs() else null;
    var move_gate: handler.move_gate.MoveGate = .{ .inner = if (rplane != null) reader_gate.filesystem() else filesystem.Filesystem.of(BackendFs, &fs), .moves = &moves, .deadline_ns = sym_slots.moves.deadlineNs(move_deadline_ms orelse 0) };
    const served_fs: filesystem.Filesystem = if (move_deadline_ms != null) move_gate.filesystem() else move_gate.inner;
    // Zero-message opens (decision 0003 §AF): the lifecycle answers OPEN
    // ENOSYS and FLUSH drains; the session drops ATOMIC_O_TRUNC and probes
    // every FLUSH on its worker; the reclaim cadence drains idle owed custody.
    const no_open = knobs.boolKnob("SQUEEZEFS_FUSE_NO_OPEN", env);
    // The write-back posture (decision 0003 §AJ): `timer` answers FLUSH
    // ENOSYS (the kernel's `no_flush`), a CREATE carries FOPEN_NOFLUSH and
    // the write-back timer below lands custody within T_wb.
    const writeback = open_table.Posture.ofKnobWord(knobs.enumKnob("SQUEEZEFS_WRITEBACK_POSTURE", env));
    var binding: handler.ring.Binding = .{};
    binding.bind(.{
        .fs = served_fs,
        .io = io,
        .registry = &registry,
        .guards = &guards,
        .opens = &opens,
        .probe = plane.probe(),
        .shared_enabled = op_guard.sharedEnabled(env),
        .no_open = no_open,
        // One FLUSH-drain per ino per reclaim tick; a burst's later closes
        // drain at the tick: the fork appender (2,000 children closing
        // copies of one fd, a cache-less set) uploaded a whole 4 MiB block
        // per close — 8.1 GB of device writes for 143 KB — draining each.
        .drain_coalesce_ns = RECLAIM_CADENCE_NS,
        .writeback = writeback,
    });
    binding.bindData(.{ .plane = plane });

    // 3. The FUSE fd (root: inside the session; else fusermount3) and the
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
    // The request timeout (decision 0003 §Z): the lattice's whole deadline
    // for one request — T_owner for the set to give a peer up, then the
    // park that follows (the move gate's T_park_max; a `--single-writer`
    // mount has no park and takes the same bound at the constant T_owner).
    const lattice_t_owner_ms: u64 = if (wire) |wp| wp.t_owner_ms else if (rplane) |rp| rp.tOwnerMs() else lease_clock.T_OWNER_MS;
    const lattice_park_max_ms: u64 = move_deadline_ms orelse sym_park.tParkMaxMs(extent_grant.failoverBoundMs(0, 0), sym_park.graceMs(lease_clock.T_OWNER_MS));
    var session_refusal: transport.session.Refusal = .none;
    const session = Session.mount(gpa, ctx.sqz, binding.filesystem(), ctx.mountpoint, env, facts, .{
        .opts = ctx.opts,
        .block_size = cfg.block_size,
        .mem_budget_bytes = ctx.budget_bytes,
        .budget = &mem_budget.authority,
        .fuse_fd = fuse_fd,
        .unmounter = unmounter,
        .on_shutdown = .{ .ctx = ctx, .call = onSessionShutdown },
        .refusal = &session_refusal,
        .request_timeout_s = fuse_geometry.requestTimeoutS(lattice_t_owner_ms, lattice_park_max_ms),
        .no_open = no_open,
    }) catch |e| {
        // `Session.mount` closed fusermount3's fd and detached its mount
        // (it owns both on every path). The typed refusal with an operator
        // remedy names it on the refusal line itself (the `--daemon`
        // invoker sees only this).
        if (e == error.BufpoolAbsent) return ctx.failed("the FUSE session could not arm at {s}: {f}", .{ ctx.mountpoint, session_refusal });
        return ctx.failed("the FUSE session could not arm at {s} ({s}) — see the log", .{ ctx.mountpoint, @errorName(e) });
    };
    var session_live = true;
    defer if (session_live) session.deinit();
    // THE STALL ANSWER (decision 0003 §BQ): a write the parked-buffer gate
    // holds behind refused flushes answers before the kernel's request
    // deadline would end this mount — the timeout the kernel applies (the
    // one asked for, seam included, when it applies none) less a check.
    plane.wp.setStallAnswerNs(fuse_geometry.stallAnswerNs(fuse_geometry.resolveRequestTimeoutS(fuse_geometry.requestTimeoutOverride(env), fuse_geometry.requestTimeoutS(lattice_t_owner_ms, lattice_park_max_ms)), session.requestTimeoutS()));
    // The wedge census names the drain workers' state while the session
    // stands, then every metadata member's conveyor, stage B and checkpoint.
    var census: WedgeCensus = .{ .session = session, .set = be };
    registry.setCensusHook(census.hook());
    defer registry.setCensusHook(null);
    // The sideband never `inode_lock`s a directory this daemon owes a reply
    // on (decision 0003 §AB.2): it judges every `INVAL_ENTRY`'s parent
    // against the registry (the probe outlives the session: a stack value
    // of this frame, cleared before the session goes).
    const in_flight_probe = registry.inFlightProbe();
    session.setInFlightProbe(&in_flight_probe);
    defer session.setInFlightProbe(null);
    // An early exit from here on ends every move park BEFORE the session's
    // teardown joins its handlers (the serve path closes it explicitly).
    defer moves.close();
    vs.renderer.sources.session = session;
    // The kernel's id of the mount just made: `squeezefs umount` tells
    // this daemon's own mount from a bind or a move of its superblock by it.
    vs.renderer.sources.mount.mount_id = session.mount_id orelse 0;
    // A recalled token's kernel view rides the session's notify seam; so
    // does a served record verb's (the holder's own kernel view).
    if (wire) |wp| {
        const seam = session.tokenNotifySeam();
        wp.sink.bindSession(seam);
        wp.record_svc.sink.kernel = .{ .ctx = seam.ctx, .invalInode = seam.invalInode, .prune = seam.prune };
    }
    if (rplane) |rp| rp.sink.bindSession(session.tokenNotifySeam());
    // The kernel view on every writer's conveyor (decision 0003 §AB.2):
    // disarmed and detached before the session goes (LIFO: this `defer`
    // runs before `session.deinit`).
    coherence.attach(session) catch return ctx.failed("the kernel view's notifier thread could not start", .{});
    if (!reader) be.armKernel(&coherence.view);
    var kernel_view_live = true;
    defer if (kernel_view_live) {
        if (!reader) be.armKernel(null);
        coherence.detach();
    };
    // THE RE-JOIN IN PLACE (decision 0003 §BS): a live member its home
    // shard evicts re-joins under this live session when nothing it holds
    // is un-landed — the plane's and the filesystem's words are the
    // mount's to judge; the kernel's view and the read cache drop at the
    // successor. Unbound before the wire stops (LIFO).
    var rejoin_host: RejoinHost = .{ .fs = &fs, .plane = plane, .set = be, .coherence = &coherence, .registry = &registry, .gpa = gpa, .io = io };
    defer rejoin_host.deinit();
    const rejoin_face = rejoin_host.face();
    if (wire) |wp| if (wp.joined != null) wp.bindRejoinHost(&rejoin_face);
    defer if (wire) |wp| wp.bindRejoinHost(null);
    // The zc legs: the held-slot direct store on a zero-copy session, the
    // direct device leg, the fd-source probe serve.
    var held_store: HeldStore = .{ .session = session };
    plane.armSession(if (session.heldStoreArmed()) held_store.face() else null, session.zeroCopyArmed(), session.readZcServe());
    // 3b. `-o interception`: the client bypass's session host (charter §7)
    //     — the listeners, the admission, the service threads on demand,
    //     the direct-drive lanes — then the `il0` xattr the shim/SDK read.
    //     The mount's device is resolved BEFORE the host arms (decision
    //     0002 row 309) through the kernel's own view of the root.
    var host: ?*ipc_host.IpcHost = null;
    if (ctx.opts.interception and !reader) {
        const mount_dev = mountDevice(ctx.mountpoint) orelse return ctx.failed("the mount's device could not be resolved for the interception host", .{});
        host = ipc_host.IpcHost.arm(.{
            .gpa = gpa,
            .io = io,
            .sqz = ctx.sqz,
            .env = env,
            .facts = facts,
            .budget_bytes = ctx.budget_bytes,
            .plane = plane,
            .fs = served_fs,
            .il0_bind = &bindIl0Thunk,
            .il0_ctx = &fs,
            .guards = &guards,
            .probe = plane.probe(),
            .shared_enabled = op_guard.sharedEnabled(env),
            .opens = &opens,
            .notify = .{ .ctx = session, .call = &invalThunk, .sync = &invalSyncThunk, .attrs = &invalAttrsThunk },
            .ring = &ring.?,
            .our_commit = ctx.p.build.commit,
            .mount_dev = mount_dev,
            .mount_uid = ctx.opts.uid,
            .block_size = cfg.block_size,
            .max_file_size = handler.lifecycle.MAX_FILE_SIZE,
            .topo = &session.topo,
            .direct_clients = ctx.opts.interception_direct,
        }) catch |e| return ctx.failed("the interception host could not arm ({s}) — see the log", .{@errorName(e)});
    } else if (ctx.opts.interception) {
        log.warn("mount: -o interception on a `-o ro` reader arms no session host (a reader serves no ring)", .{});
    }
    rejoin_host.bindIpc(host);
    var host_live = true;
    defer if (host_live) if (host) |h| {
        rejoin_host.bindIpc(null);
        h.disarm();
        h.deinit();
    };

    // 4. The budget sampler — AFTER the last mount-time registrant (the
    //    fiber stacks, the node caches, the plane's tiers and pools, the
    //    session's arena, the host's arenas) and so, this `defer` sitting
    //    below every registrant's, stopped and JOINED before any of them
    //    is freed on every exit of this function; the serve-path teardown
    //    below stops it first for the same reason. The thread reads every
    //    source without a lock: a source freed under it kills the daemon
    //    in `samplerMain → current` (a late bring-up refusal of the R5
    //    crash row died there with the plane already closed). The `.stats`
    //    renderer's census walks (/proc, /sys) ride the same thread
    //    (decision 0003 §CC.1); the renderer (`vs`) outlives the sampler
    //    for the same reason.
    if (!mem_budget.authority.addSampler(vs.renderer.sampler())) return ctx.failed("the memory-budget sampler has no slot for the stats census", .{});
    _ = mem_budget.authority.spawnSampler(ctx.budget_host, mem_budget.sampler_period_ns) catch |e| return ctx.failed("cannot start the memory-budget sampler ({s})", .{@errorName(e)});
    var sampler_live = true;
    defer if (sampler_live) mem_budget.authority.stopSampler(ctx.p.io);

    // 5. The watchdog loop and the reclaim cadence on the meta pool. The
    //    watchdog's tick beats the supervisor's heartbeat and runs until
    //    the end of the teardown (the supervisor judges a detached or
    //    unmounting daemon by it; a long dismount pass is not a wedge).
    var wd = watchdog.Watchdog.init(&registry);
    if (ctx.heartbeat) |h| wd.on_tick = .{ .ctx = h, .call = &beatHeartbeat };
    var wd_fut = ctx.sqz.spawnMeta(watchdogMain, .{ &wd, io }) catch return ctx.failed("cannot spawn the watchdog", .{});
    const owed_seam_ns: ?u64 = if (knobs.optIntKnob("SQUEEZEFS_TEST_FUSE_OWED_IDLE_MS", u64, env)) |ms| ms * std.time.ns_per_ms else null;
    const owed_idle_ns: u64 = owed_seam_ns orelse OWED_IDLE_NS;
    var reclaim: ReclaimCadence = .{ .fs = &fs, .io = io, .plane = plane, .node = node, .opens = &opens, .owed_idle_ns = owed_idle_ns };
    var reclaim_fut = ctx.sqz.spawnMeta(ReclaimCadence.run, .{&reclaim}) catch {
        wd.requestStop();
        wd_fut.cancel(io);
        return ctx.failed("cannot spawn the reclaim cadence", .{});
    };
    // The membership cadence on a fiber of its own (decision 0003 §AE.2):
    // a symmetric set's evictions, death-ledger poll and parked-scope sweep
    // never wait behind the reclaim cadence's guard-taking passes.
    var membership: ?MembershipCadence = if (wire) |wp| .{ .fs = &fs, .io = io, .wire = wp } else null;
    var membership_fut: ?@TypeOf(reclaim_fut) = if (membership) |*m| ctx.sqz.spawnMeta(MembershipCadence.run, .{m}) catch {
        wd.requestStop();
        wd_fut.cancel(io);
        reclaim.requestStop();
        reclaim_fut.cancel(io);
        return ctx.failed("cannot spawn the membership cadence", .{});
    } else null;
    // The times drain (decision 0003 §L.6): the parked write stamps and
    // SETATTR echoes journaled once per flush interval; strict mode parks
    // nothing and runs no cadence.
    var times_drain: TimesDrainCadence = .{ .fs = &fs, .io = io, .interval_ns = flush_interval_ms * std.time.ns_per_ms, .meta_uri = ctx.meta_uri };
    var times_drain_fut: ?@TypeOf(reclaim_fut) = if (flush_interval_ms == 0) null else ctx.sqz.spawnMeta(TimesDrainCadence.run, .{&times_drain}) catch {
        wd.requestStop();
        wd_fut.cancel(io);
        reclaim.requestStop();
        reclaim_fut.cancel(io);
        if (membership_fut) |*f| {
            membership.?.requestStop();
            f.cancel(io);
        }
        return ctx.failed("cannot spawn the times drain cadence", .{});
    };
    // The write-back timer (decision 0003 §AJ): one pass per flush cadence
    // lands every custody as old as the checkpoint's cadence ceiling —
    // T_wb = age + period + the pass's drain. The `close` posture drains at
    // close instead.
    const wb_period_ns = open_table.timerPeriodNs(flush_interval_ms);
    var wb_timer: WritebackTimer = .{ .fs = &fs, .io = io, .opens = &opens, .period_ns = wb_period_ns, .age_ns = owed_seam_ns orelse open_table.timerAgeNs(checkpoint.maxAgeMsFromEnv(env)) };
    open_table.writeback_bound_ns.set(if (writeback == .timer) open_table.boundStaticNs(wb_timer.age_ns, wb_period_ns) else 0);
    open_table.writeback_age_ns.set(if (writeback == .timer) wb_timer.age_ns else 0);
    var wb_timer_fut: ?@TypeOf(reclaim_fut) = if (writeback == .close) null else ctx.sqz.spawnMeta(WritebackTimer.run, .{&wb_timer}) catch {
        wd.requestStop();
        wd_fut.cancel(io);
        reclaim.requestStop();
        reclaim_fut.cancel(io);
        if (membership_fut) |*f| {
            membership.?.requestStop();
            f.cancel(io);
        }
        if (times_drain_fut) |*f| {
            times_drain.requestStop();
            f.cancel(io);
        }
        return ctx.failed("cannot spawn the write-back timer", .{});
    };
    if (writeback == .timer) {
        log.info("write-back timer: custody lands once {d} ms old, a pass every {d} ms — T_wb = {d} ms + the pass's drain; close is no durability point", .{ wb_timer.age_ns / std.time.ns_per_ms, wb_period_ns / std.time.ns_per_ms, open_table.writeback_bound_ns.load() / std.time.ns_per_ms });
    } else log.info("write-back: the close posture — custody drains at the close the kernel reports", .{});
    // The atime posture (charter §4 "Access time"): the kernel mount's
    // flags, read now and at every mount-table change.
    const atime_buf = gpa.alloc(u8, mountinfo.READ_BYTES) catch return ctx.failed("out of memory (the mount table's buffer)", .{});
    defer gpa.free(atime_buf);
    var atime_watch: AtimeWatch = .{ .fs = &fs, .io = io, .mount_id = session.mount_id, .mountpoint = ctx.mountpoint, .knob = knobs.enumKnob("SQUEEZEFS_ATIME", env), .buf = atime_buf };
    atime_watch.refresh();
    var atime_fut = ctx.sqz.spawnMeta(AtimeWatch.run, .{&atime_watch}) catch {
        wd.requestStop();
        wd_fut.cancel(io);
        reclaim.requestStop();
        reclaim_fut.cancel(io);
        if (membership_fut) |*f| {
            membership.?.requestStop();
            f.cancel(io);
        }
        if (times_drain_fut) |*f| {
            times_drain.requestStop();
            f.cancel(io);
        }
        if (wb_timer_fut) |*f| {
            wb_timer.requestStop();
            f.await(io);
        }
        return ctx.failed("cannot spawn the atime watch", .{});
    };

    // 6. Ready. The heartbeat's first stamp lands HERE, before the
    //    supervisor's first probe can read it (the watchdog's first tick is
    //    one interval away; a page still 0 reads as a missed probe).
    if (ctx.heartbeat) |h| h.beat();
    // The session's posture, not the knob's: a mainline zero-copy session
    // serves opens (decision 0003 §BE) and has nothing to latch.
    if (session.noOpen()) latchNoOpen(ctx.mountpoint);
    // The classes the law serves now, and its posture (decision 0003 §AB.2).
    const ttls_now = coherence.law.current();
    log.info("ready: {s} mounted at {s} (pid {d}; {d} queue(s) × depth {d}; budget {d} MiB; TTLs attr {d} ms entry {d} ms dir_entry {d} ms negative {d} ms, {s})", .{ ctx.meta_uri, ctx.mountpoint, linux.getpid(), session.plan.nqueues, session.plan.depth, ctx.budget_bytes >> 20, ttls_now.attr_ns / std.time.ns_per_ms, ttls_now.entry_ns / std.time.ns_per_ms, ttls_now.dir_entry_ns / std.time.ns_per_ms, ttls_now.negative_ns / std.time.ns_per_ms, if (coherence.law.peered.load(.acquire)) "peered" else "isolated" });
    if (ctx.reporter) |*r| {
        r.ready();
        r.close();
    }
    // The landing-hold seam (the replay rows): every window admitted from
    // here stays on the lane un-issued — acked, in the tail, never landed —
    // until the daemon dies.
    if (knobs.boolKnob("SQUEEZEFS_TEST_JOURNAL_HOLD_LANDING", env)) {
        journal_lane.test_seams.delay_issue.store(1, .release);
        log.warn("test seam: SQUEEZEFS_TEST_JOURNAL_HOLD_LANDING — journal writes are held un-issued from here; acked metadata lives in the journal tail alone", .{});
    }

    // 7. Serve until a stop latches or the session ends.
    serve(ctx, session, &vs);
    // Every op parked on a slot move answers now (EIO, counted): nothing
    // the teardown joins waits out the lattice's deadline.
    moves.close();

    // 8. Teardown in order: the budget sampler first (stopped and joined
    //    while every source it reads is still live — step 4's law), then
    //    the interception host (every session poisoned, its threads
    //    joined, the listeners closed, the xattr unarmed) while the session
    //    its handoffs ride is still live.
    sampler_live = false;
    mem_budget.authority.stopSampler(ctx.p.io);
    if (host) |h| {
        rejoin_host.bindIpc(null);
        h.disarm();
        h.deinit();
    }
    host_live = false;
    session.close();
    reclaim.requestStop();
    reclaim_fut.cancel(io);
    if (membership_fut) |*f| {
        membership.?.requestStop();
        f.cancel(io);
    }
    if (times_drain_fut) |*f| {
        times_drain.requestStop();
        f.cancel(io);
    }
    atime_watch.requestStop();
    atime_fut.cancel(io);
    // The write-back timer finishes its pass (never cancelled mid-drain);
    // the dismount pass below lands whatever it leaves.
    if (wb_timer_fut) |*f| {
        wb_timer.requestStop();
        f.await(io);
    }
    // The job fabric stops BEFORE the dismount pass and the wire: a mover
    // mid-step releases its claim at its next task boundary (the plan is
    // regenerated by the next mount's adoption — crash-resume's law).
    if (node) |n| {
        if (wire) |wp| wp.listener.bindDeparture(null);
        n.stop(if (wire) |wp| &wp.svc else null);
        n.destroy();
    }
    node_live = false;
    // The dismount pass (charter §8 `umount`: "teardown promotes staged
    // files first") with every arm it mints and frees through still LIVE:
    // the grant plane is the allocator's mint source (a holder's own
    // window, a joiner's pulled window) and the wire plane's teardown
    // DISARMS it — a promotion minting after that met the carved window's
    // SET bits at the frontier (the rung-8 dismount defect). No FUSE
    // request is in flight (the session is closed); the wire still serves.
    plane.dismount();
    // The write cache drained by the pass: the supervisor discards it
    // instead of holding an empty ring for a successor.
    if (plane.writeCacheDrained()) announceWriteCacheDrained();
    // The wire BEFORE the planes and the backend it serves over: every
    // connection severed, the token hook disarmed on every pass — and
    // every seam the planes hold INTO it unbound first (nothing may reach
    // a stopped plane).
    if (wire) |wp| {
        fs.bindForeignWatch(null);
        plane.bindForeignLayout(null);
        plane.bindForeignPublish(null);
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
    log.info("teardown: session closed; open_count_stranded={d} transport_requests_abandoned={d} tick_commit_lost_wakes={d} tick_cqe_lost_wakes={d} tick_late_commits={d} tick_late_cqes={d}", .{ stranded, abandoned, transport.worker.park_tick_commit_lost_wakes.load(), transport.worker.park_tick_cqe_lost_wakes.load(), transport.worker.park_tick_late_commits.load(), transport.worker.park_tick_late_cqes.load() });
    if (!reader) be.armKernel(null);
    coherence.detach();
    kernel_view_live = false;
    session_live = false;
    vs.renderer.sources.session = null;
    registry.setCensusHook(null);
    session.deinit();
    // The plane closes BEFORE the backend's final checkpoint (the dismount
    // pass's promotions were layout transactions); the `defer`s unwind the
    // filesystem and the tables after it.
    plane.bindCorpseSupply(null);
    plane_open = false;
    plane.close();
    be_open = false;
    const dismount_clean = be.closeJudged();
    // The watchdog last: its tick is the heartbeat the supervisor judged
    // the whole teardown by.
    wd.requestStop();
    wd_fut.cancel(io);
    if (failed) {
        log.err("a member of {s} latched FAILED during this mount (EIO until remount) — exiting 1", .{ctx.meta_uri});
        return cli.exit_failed;
    }
    // A dismount that never reached its durability point (the member's
    // UNCLEAN unmount line says which step failed) is no orderly exit.
    if (!dismount_clean) {
        log.err("the dismount of {s} did not land its final checkpoint — exiting 1 (the next mount replays its journal)", .{ctx.meta_uri});
        return cli.exit_failed;
    }
    if (ctx.connection_aborted) {
        log.err("the FUSE connection of {s} was aborted under its live mount (fail-stopped) — exiting 1", .{ctx.mountpoint});
        return cli.exit_failed;
    }
    return cli.exit_ok;
}

/// The fabric's job-id source: the process's secure random through the
/// executor's `Io` is not reachable from the fabric's hot lock, so the
/// ids ride a per-process xorshift seeded ONCE from `Io.randomSecure` at
/// the node's creation (ids are identities, never secrets; 0 is never a
/// job id — the fabric re-rolls it).
var job_id_seed: std.atomic.Value(u64) = .init(0);

/// The striping census' `N_floor`: the slot plane's live derivation on
/// volume 0 (the handover judge's word — "the striping trigger reads its
/// window"), read at every served insert.
/// Each writable volume's tail memfd and guard fd over the supervisor's
/// socket (`SCM_RIGHTS`; the kernel holds them from the send on). A send
/// that fails is said and the volume runs without the replay.
fn announceTails(be: *meta_set.MetaSet, env: *const std.process.Environ.Map) void {
    const sock = journal_tail.supervisor_sock.load(.acquire);
    const strict = checkpoint.flushIntervalMsFromEnv(env) == 0;
    const at_apply = !strict and std.mem.eql(u8, knobs.enumKnob("SQUEEZEFS_META_ACK_POINT", env), "apply");
    if (sock < 0) {
        if (at_apply) log.info("metadata acks precede the journal landing (the ext4 posture, charter §5.2); no supervisor holds the journal tail, so a crash of this process has the flush-window exposure ({d} ms) — `--daemon --supervise` replays it", .{checkpoint.flushIntervalMsFromEnv(env)});
        return;
    }
    var sent: usize = 0;
    for (be.volumes, 0..) |vol, i| {
        const t = vol.tail orelse continue;
        const index: u16 = @intCast(i);
        if (!announceTail(null, index, t.fd)) {
            log.err("volume {d}: cannot hand the journal tail to the supervisor — a crash has the flush-window exposure", .{index});
            continue;
        }
        const guard_frame = (cli.supervisor.TailFrame{ .kind = .guard, .index = index }).encode();
        ipc_rl.sendCtl(sock, &guard_frame, vol.guard_fd) catch |e| {
            log.err("volume {d}: cannot hand the writer guard to the supervisor ({s}) — a successor may open the volume before the replay", .{ index, @errorName(e) });
            continue;
        };
        // A tail the volume re-creates later (a JOINED ring's growth) rides
        // the same socket; the supervisor replays every tail it holds in
        // order, a retired one's slots all free.
        vol.tail_announce = .{ .ctx = null, .announce = &announceTail };
        sent += 1;
    }
    log.info("journal tails: {d} volume(s) handed to the supervisor (a crash of this process is replayed to the device before the writer claim is released); acks {s}", .{ sent, if (at_apply) "precede the landing (the ext4 posture)" else "follow the landing" });
}

/// One tail frame + its memfd over the supervisor's socket
/// (`KvBackend.tail_announce`).
fn announceTail(_: ?*anyopaque, index: u16, fd: std.os.linux.fd_t) bool {
    const sock = journal_tail.supervisor_sock.load(.acquire);
    if (sock < 0) return false;
    const frame = (cli.supervisor.TailFrame{ .kind = .tail, .index = index }).encode();
    ipc_rl.sendCtl(sock, &frame, fd) catch return false;
    return true;
}

/// The write cache's memfd and its claim over the supervisor's socket
/// (two frames, one fd each — the discipline). A mount without a
/// supervisor binds no write cache (decision 0003 §BR.6).
fn announceWriteCache(plane: *plane_mod.DataPlane, env: *const std.process.Environ.Map) void {
    const fds = plane.writeCacheFds() orelse return;
    const sock = journal_tail.supervisor_sock.load(.acquire);
    // A write cache is bound only under a supervisor (`run`).
    std.debug.assert(sock >= 0);
    if (knobs.parseStr(env.get("SQUEEZEFS_TEST_WRITE_CACHE_ANNOUNCE_HOLD"))) |path| if (!std.mem.eql(u8, path, "off")) holdBeforeAnnounce(path);
    const claim_frame = (cli.supervisor.TailFrame{ .kind = .write_cache_claim, .index = 0 }).encode();
    const ring_frame = (cli.supervisor.TailFrame{ .kind = .write_cache, .index = 0 }).encode();
    const handover_frame = (cli.supervisor.TailFrame{ .kind = .write_cache_handover, .index = 0 }).encode();
    if (fds.claim) |claim| ipc_rl.sendCtl(sock, &claim_frame, claim) catch |e| {
        log.err("write cache: cannot hand the claim to the supervisor ({s}) — a crash loses the ring's unsynced residents", .{@errorName(e)});
        return;
    };
    ipc_rl.sendCtl(sock, &ring_frame, fds.memfd) catch |e| {
        log.err("write cache: cannot hand the memfd to the supervisor ({s}) — a crash loses the ring's unsynced residents", .{@errorName(e)});
        return;
    };
    // After a handover: the previous supervisor keeps the ring until this
    // one acks holding it on the connection, then lets the claim go for it
    // (decision 0003 §CV).
    if (fds.handover) |conn| {
        ipc_rl.sendCtl(sock, &handover_frame, conn) catch |e| {
            log.err("write cache: cannot hand the claim connection to the supervisor ({s}) — the previous supervisor keeps the ring until its ack bound, then leaves it to this process alone", .{@errorName(e)});
            return;
        };
        plane.writeCacheAnnounced();
    }
    log.info("write cache: the memfd and its {s} handed to the supervisor (a crash of this process keeps every resident for this member's next mount)", .{if (fds.handover != null) "claim connection" else "claim"});
}

/// `SQUEEZEFS_TEST_WRITE_CACHE_ANNOUNCE_HOLD`: park until a writer opens
/// the FIFO, the ring bound and not yet the supervisor's.
fn holdBeforeAnnounce(path: []const u8) void {
    log.warn("write cache: SQUEEZEFS_TEST_WRITE_CACHE_ANNOUNCE_HOLD (pid {d}) — the ring is bound and not yet the supervisor's; holding until a writer opens {s}", .{ linux.getpid(), path });
    var pb: [linux.PATH_MAX]u8 = undefined;
    const p = std.fmt.bufPrintSentinel(&pb, "{s}", .{path}, 0) catch return;
    const rc = linux.open(p.ptr, .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) {
        log.warn("write cache: the announce hold could not open {s} ({t}); not holding", .{ path, linux.errno(rc) });
        return;
    }
    _ = linux.close(@intCast(rc));
}

fn announceWriteCacheDrained() void {
    const sock = journal_tail.supervisor_sock.load(.acquire);
    if (sock < 0) return;
    const frame = (cli.supervisor.TailFrame{ .kind = .write_cache_drained, .index = 0 }).encode();
    ipc_rl.sendCtl(sock, &frame, null) catch {};
}

/// `tokens.table.PrepareSeam` over the filesystem: the object is the ino.
fn landForGrant(ctx: *anyopaque, _: Io, object: u64) void {
    const fs: *BackendFs = @ptrCast(@alignCast(ctx));
    fs.data.recallResidents(object);
    fs.landForGrant(object);
}

/// `PrepareSeam.size_epoch`: the holder's size epoch of the object, biased
/// by one on the wire (0 = none: a fresh set's epochs start at 0).
fn grantSizeEpoch(ctx: *anyopaque, object: u64) u64 {
    const fs: *BackendFs = @ptrCast(@alignCast(ctx));
    return fs.data.size_epochs.current(object) + 1;
}

/// `SQUEEZEFS_TEST_FOREIGN_CLAIM_HOLD` (a FIFO path; unset or `off` = none).
fn claimHoldPath(env: *const std.process.Environ.Map) ?[]const u8 {
    const raw = knobs.parseStr(env.get("SQUEEZEFS_TEST_FOREIGN_CLAIM_HOLD")) orelse return null;
    return if (std.mem.eql(u8, raw, "off")) null else raw;
}

/// `PrepareSeam.pending`: a claim parked since the landing above.
fn grantPending(ctx: *anyopaque, object: u64) bool {
    const fs: *BackendFs = @ptrCast(@alignCast(ctx));
    return fs.claimParked(object);
}

/// `KvFs.ForeignWatch` over the wire's token holder.
fn holderHeld(ctx: *anyopaque, local: u64) bool {
    const h: *token_table.Holder = @ptrCast(@alignCast(ctx));
    return h.held(local);
}

fn stripeFloor(be: *meta_set.MetaSet) sym_xv.stripe.Floor {
    return .{ .live = .{ .ctx = be, .read = &stripeFloorRead } };
}

/// Volume 0's judge as `volumes` names it at every read (a re-join in
/// place publishes a successor; Acquire pairs with
/// `MetaSet.publishSuccessors`' Release store).
fn stripeFloorRead(ctx: *anyopaque) u32 {
    const set: *meta_set.MetaSet = @ptrCast(@alignCast(ctx));
    const be0 = @atomicLoad(*meta_set.KvBackend, &set.volumes[0], .acquire);
    return @intCast(@min(be0.slots.judge.nFloorNow(), std.math.maxInt(u32)));
}

fn seedJobIds(io: Io) void {
    var b: [8]u8 = undefined;
    io.randomSecure(&b) catch std.mem.writeInt(u64, &b, @bitCast(@import("../core/clock.zig").monotonicNs()), .little);
    job_id_seed.store(std.mem.readInt(u64, &b, .little) | 1, .monotonic);
}

fn jobRandom() u64 {
    var x = job_id_seed.load(.monotonic);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    job_id_seed.store(x, .monotonic);
    return x;
}

/// The online `volume add-data`'s grant-plane arm (the wire plane's).
/// The online fsck's bitmap face over the wire plane: the holder plane's
/// bitmap while this mount holds the allocation lease, null else.
fn holderBitmapOf(ctx: *anyopaque, vol_tag: u64) ?*data_bitmap.DataBitmap {
    const wp: *sym_wire.WirePlane = @ptrCast(@alignCast(ctx));
    const hp = wp.holder_plane.load(.acquire) orelse return null;
    return hp.bitmapOf(vol_tag);
}

const DataArmCtx = struct {
    wire: ?*sym_wire.WirePlane,

    fn arm(self: *DataArmCtx) jobs.coordinator.DataArm {
        return .{ .ctx = self, .arm = &call };
    }

    fn call(ctx: *anyopaque, plane: *plane_mod.DataPlane, ordinal: u16) error{Io}!void {
        const self: *DataArmCtx = @ptrCast(@alignCast(ctx));
        const wp = self.wire orelse return error.Io;
        wp.armDataVolume(plane, ordinal) catch return error.Io;
    }
};

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
    // This daemon's word of the set config's generation: the manager's
    // grants carry it, a joiner compares its own against the grant's.
    lifecycle_stats.volume_config_generation.set(cfg.config_generation);
    {
        // The fabric plane BEFORE discovery: a data volume with a
        // `fabric_endpoint` record is connected (or its predecessor's
        // association reused) under this mount's identity and its uri
        // becomes the live node; the §5.2 ladder judges every other volume
        // under an explicit pair. Discovery's superblock read right after
        // is the identity law for what the connect brought up.
        var fdiag: mount_fabric.Refusal = undefined;
        ctx.fabric.bind(ctx.p.env, &cfg, ctx.paths, ctx.mountpoint, &fdiag) catch {
            var buf: [1024]u8 = undefined;
            var w: Io.Writer = .fixed(&buf);
            fdiag.write(&w) catch {};
            _ = ctx.failed("fabric: {s}", .{w.buffered()});
            cfg.deinit(gpa);
            return null;
        };
    }
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
    if (cfg.encryption == .none and ctx.stash != null) {
        // The format's pairing law, mirrored: a key for a plaintext set
        // encrypts nothing.
        _ = ctx.failed("--encrypt-key was given but {s} is a PLAINTEXT set (encryption none) — nothing would be encrypted; re-run without the flag", .{ctx.meta_uri});
        cfg.deinit(gpa);
        return null;
    }
    if (cfg.encryption != .none) {
        var path_buf: [128]u8 = undefined;
        var served: []const u8 = "";
        const derived = if (ctx.stash) |st| blk: {
            // Rung 1 (`--encrypt-key`): the pre-fork stash, derived against
            // the set's salt and verified; the stash is zeroized here —
            // the derived copy below is the only key-bearing word left.
            defer st.zeroize();
            served = "--encrypt-key";
            break :blk keys.linux_verbs.deriveFromStash(st, &cfg.kdf_salt, cfg.key_id) catch |e| {
                var buf: [512]u8 = undefined;
                var w: Io.Writer = .fixed(&buf);
                transform.writeKeyFileRefusal(&w, e, "--encrypt-key") catch {};
                _ = ctx.failed("encrypted set: {s}", .{w.buffered()});
                cfg.deinit(gpa);
                return null;
            };
        } else transform.linux_keys.resolveVolumeKey(ctx.p.env, null, &cfg.kdf_salt, cfg.key_id, &path_buf, &served) catch |e| {
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
        // An open rotation window: the NEXT key from the key dir (the
        // sweep resumes under it; every block is readable under one of
        // the two) — missing or wrong refuses the mount by name.
        if (cfg.next_key) |nk| {
            var nb: [128]u8 = undefined;
            const next = keys.linux_verbs.resolveNextKey(&nk.kdf_salt, nk.key_id) catch |e| {
                var buf: [512]u8 = undefined;
                var w: Io.Writer = .fixed(&buf);
                transform.writeKeyFileRefusal(&w, e, keys.keyPath(&nb, nk.key_id)) catch {};
                _ = ctx.failed("encrypted set with a key-rotation window open (next key id {x}): {s} — install it with `squeezefs key install`, or finish the rotation where it started", .{ &nk.key_id, w.buffered() });
                if (ctx.derived_key) |*k| k.zeroize();
                cfg.deinit(gpa);
                return null;
            };
            ctx.derived_next = next;
            log.info("mount: key-rotation window OPEN — next key id {x} verified from {s}; the sweep resumes on this mount's fabric", .{ &nk.key_id, keys.keyPath(&nb, nk.key_id) });
        }
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
        error.LegacyFormat => "a pre-v5 squeezefs format, which this binary does not mount — reformat required",
        error.UnsupportedFormat => "unknown or later-rung capability bits — reformat or a later binary",
        error.Corrupt => "corruption outside the replay window (see the log)",
        error.DeviceRefused => "the device refused registration (O_DIRECT posture) or does not exist",
        error.Io => "device I/O failed",
        error.OutOfMemory => "out of memory",
        error.LaneSpawnFailed, error.ThreadSpawnFailed => "cannot spawn the journal lane or the checkpoint thread",
        // A bring-up step's own class, carried as itself (decision 0003 §CS).
        else => "a bring-up step failed (see the log: the line names its class)",
    };
}

fn watchdogMain(wd: *watchdog.Watchdog, io: Io) void {
    wd.run(io);
}

/// The watchdog's tick hook: stamp the supervisor's page.
fn beatHeartbeat(ctx: *anyopaque) void {
    const h: *cli.supervisor.Heartbeat = @ptrCast(@alignCast(ctx));
    h.beat();
}

/// The periodic drain of the corpse gather (module doc), the cross-owner
/// roll-forward and stripe migration — passes that may wait on a DLM guard.
const ReclaimCadence = struct {
    fs: *BackendFs,
    io: Io,
    /// The plane's fill census + the fabric's write-rate sample ride the
    /// same tick (the "health-worker cadence" of the router).
    plane: *plane_mod.DataPlane,
    node: ?*jobs.coordinator.Node,
    /// The no-open owed set (decision 0003 §AF): custody no close covered —
    /// a held-open writer, a mapping written back after its fd closed, an
    /// async write landing after its close — drained here once idle.
    opens: *open_table.OpenTable,
    /// `OWED_IDLE_NS` (the seam `SQUEEZEFS_TEST_FUSE_OWED_IDLE_MS` wins).
    owed_idle_ns: u64,
    stop: std.atomic.Value(bool) = .init(false),

    fn requestStop(self: *ReclaimCadence) void {
        self.stop.store(true, .release);
        self.fs.reclaim_wake.notifyOne(self.io);
    }

    /// The owed inos one tick drains at most: the harvest's stack room (a
    /// longer tail waits one tick; a full harvest loops within the tick).
    const OWED_HARVEST = 64;

    /// Every owed ino idle for `owed_idle_ns` drained through the
    /// filesystem's FLUSH drain (the RELEASE drain), on this meta fiber —
    /// the corpse destroys of the same tick settle custody the same way.
    fn drainIdleOwed(self: *ReclaimCadence, c: *filesystem.Ctx) void {
        if (!self.opens.no_open or self.opens.posture == .timer) return;
        var buf: [OWED_HARVEST]u64 = undefined;
        while (true) {
            const inos = self.opens.takeIdleOwed(clock.monotonicNs(), self.owed_idle_ns, &buf);
            var failed = false;
            for (inos) |ino| {
                self.fs.flush(c, .{ .ino = ino, .fh = 0, .lock_owner = 0, .drain = true }) catch |e| {
                    // Never dropped: noted again (idle at once); the next
                    // tick retries — this one stops harvesting.
                    failed = true;
                    log.err("reclaim cadence: the idle drain of owed ino {d} failed ({t}); noted owed again for the next tick", .{ ino, e });
                    self.opens.noteWrite(ino, 0) catch log.err("reclaim cadence: owed ino {d} could not be noted again — its custody waits for a close, an fsync, a syncfs or the unmount", .{ino});
                    continue;
                };
                open_table.fuse_owed_idle_drains.inc();
            }
            if (failed or inos.len < buf.len) return;
        }
    }

    /// Parked on the filesystem's reclaim wake with the next tick as the
    /// deadline (register-then-check, the notify law): a full corpse batch
    /// wakes it for the full batches alone (`kv_fs.gatherCorpse` — the
    /// forgetting thread destroys nothing); the tick drains the tail and
    /// runs the rest.
    fn run(self: *ReclaimCadence) void {
        var next_tick_ns = clock.monotonicNs() + RECLAIM_CADENCE_NS;
        while (true) {
            var wake = self.fs.reclaim_wake.notified();
            _ = wake.enable(self.io);
            if (self.stop.load(.acquire)) {
                wake.cancel(self.io);
                return;
            }
            _ = wake.waitUntilUncancelable(self.io, next_tick_ns);
            if (self.stop.load(.acquire)) return;
            var trace = op_trace.OpTrace.begin(0, .other);
            defer trace.finish();
            var ctx: filesystem.Ctx = .{ .unique = 0, .uid = 0, .gid = 0, .pid = 0, .io = self.io, .trace = &trace, .wd_slot = null, .window = &.{} };
            if (clock.monotonicNs() < next_tick_ns) {
                // A full store kicked the gather (decision 0003 §CR): the
                // tail too, now; else full batches alone.
                if (self.fs.takeCorpseKick()) {
                    self.fs.reclaimDrainKicked(&ctx) catch |e| log.err("reclaim cadence: drain failed: {t}", .{e});
                } else self.fs.reclaimDrainFull(&ctx) catch |e| log.err("reclaim cadence: drain failed: {t}", .{e});
                continue;
            }
            _ = self.fs.takeCorpseKick();
            self.fs.reclaimDrain(&ctx) catch |e| log.err("reclaim cadence: drain failed: {t}", .{e});
            next_tick_ns = clock.monotonicNs() + RECLAIM_CADENCE_NS;
            self.drainIdleOwed(&ctx);
            // The cross-owner cadence rides the same meta fiber: the
            // roll-forward of abandoned intents, the stripe migration.
            self.fs.xvCadenceTick();
            self.plane.refreshCensus() catch |e| log.err("reclaim cadence: the fill census could not refresh: {t}", .{e});
            if (self.node) |n| n.sampleWriteRate(write_admit.stats.ledgerSum());
        }
    }
};

/// The membership cadence (decision 0003 §AE.2): the cluster wire's tick —
/// evictions at `T_owner`, the grace reap, the death ledger's poll with the
/// recoveries it acts and the open intents it hands to the cross-owner
/// cadence — then the parked guard scopes' expiry sweep. Nothing it runs
/// waits on a DLM guard: it is what dooms a dead member's lease and frees
/// the scopes that member parked, so a roll-forward or a migration waiting
/// on one (the reclaim cadence) resumes within a tick of the eviction —
/// on one fiber they waited on each other for ever (the shared hot
/// directory row: C killed mid-storm, never evicted).
const MembershipCadence = struct {
    fs: *BackendFs,
    io: Io,
    wire: *sym_wire.WirePlane,
    stop: std.atomic.Value(bool) = .init(false),

    fn requestStop(self: *MembershipCadence) void {
        self.stop.store(true, .release);
    }

    fn run(self: *MembershipCadence) void {
        while (!self.stop.load(.acquire)) {
            self.io.sleep(.fromNanoseconds(RECLAIM_CADENCE_NS), .awake) catch return;
            if (self.stop.load(.acquire)) return;
            self.wire.tick();
            self.fs.xvSweepTick();
        }
    }
};

/// The times drain cadence (decision 0003 §L.6; the reference's
/// `times_drain_task`): a meta fiber parked on the filesystem's drain wake
/// with the flush interval as its deadline — the cap crossing wakes it
/// early, the tick drains whatever is parked — so a parked write stamp is
/// journaled within one interval of its write, the same bound the
/// coalesced barrier already gives an acked entry. Register-then-check on
/// every iteration (the notify law); a drain that fails keeps its
/// refinements parked and the next wake retries — unless the store is
/// FAILED (the fail-stop latch): every commit refuses until remount, so
/// the cadence names it ONCE and from then on drains nothing and counts
/// its ticks (`fuse_write_times_drain_failed_ticks`), never a line per
/// tick (fstests generic/475 under dm-error: 20 lines a second until the
/// unmount). Generic over the filesystem so the tick runs over the RAM
/// store in tests; the daemon's is `TimesDrainCadence`.
fn TimesDrainCadenceOf(comptime Fs: type) type {
    return struct {
        const Self = @This();

        fs: *Fs,
        io: Io,
        interval_ns: u64,
        /// The set the FAILED line names.
        meta_uri: []const u8,
        stop: std.atomic.Value(bool) = .init(false),
        /// The FAILED line was logged (the cadence fiber's own word).
        failed_named: bool = false,

        fn requestStop(self: *Self) void {
            self.stop.store(true, .release);
            self.fs.times_drain_wake.notifyOne(self.io);
        }

        fn run(self: *Self) void {
            while (true) {
                var n = self.fs.times_drain_wake.notified();
                _ = n.enable(self.io);
                if (self.stop.load(.acquire)) {
                    n.cancel(self.io);
                    return;
                }
                _ = n.waitUntilUncancelable(self.io, clock.monotonicNs() + self.interval_ns);
                if (self.stop.load(.acquire)) return;
                self.tick();
            }
        }

        /// One tick: the drain of whatever is parked.
        fn tick(self: *Self) void {
            if (self.fs.pending.len() == 0) return;
            if (self.fs.store.isFailed()) return self.failedTick();
            var trace = op_trace.OpTrace.begin(0, .other);
            defer trace.finish();
            // `kernel`: the stamps are the ones this mount's kernel already
            // holds through the fold (nothing to invalidate back into it),
            // so the drain is acked at its apply like the syncfs drain of
            // the same stamps — a commit acked at the landing held its
            // batch's exclusive inode guards across the device, and every
            // op on those inodes waited for it (for good under a held
            // landing: THE REPLAY PROOF's 115 s create).
            var ctx: filesystem.Ctx = .{ .unique = 0, .uid = 0, .gid = 0, .pid = 0, .io = self.io, .trace = &trace, .wd_slot = null, .window = &.{}, .kernel = true };
            _ = self.fs.drainPendingTimes(&ctx) catch |e| {
                // The latch can trip under the drain (the lane's thread).
                if (self.fs.store.isFailed()) return self.failedTick();
                log.warn("times drain cadence: drain failed ({t}); {d} refinement(s) stay parked for the next tick", .{ e, self.fs.pending.len() });
            };
        }

        fn failedTick(self: *Self) void {
            handler.kv_fs.fuse_write_times_drain_failed_ticks.inc();
            if (self.failed_named) return;
            self.failed_named = true;
            log.err("times drain cadence: a member of {s} is FAILED — the drain stops until remount; {d} parked refinement(s) stay fold-visible for the rest of this mount and are never journaled (every commit answers EIO)", .{ self.meta_uri, self.fs.pending.len() });
        }
    };
}

const TimesDrainCadence = TimesDrainCadenceOf(BackendFs);

/// The write-back timer (decision 0003 §AJ): a meta fiber parked on its own
/// wake with the period (the flush cadence) as its deadline; each wake runs
/// ONE `open_table.timerPass` — every owed ino whose custody is `age_ns` old
/// drains through the filesystem's `drainAged`, what it keeps is re-owed.
/// The stop wakes it and the teardown AWAITS the pass in flight: a drain is
/// never cancelled mid-DMA (the dismount pass lands what is left).
const WritebackTimer = struct {
    fs: *BackendFs,
    io: Io,
    opens: *open_table.OpenTable,
    period_ns: u64,
    age_ns: u64,
    wake: sync.Notify = .init,
    stop: std.atomic.Value(bool) = .init(false),

    fn requestStop(self: *WritebackTimer) void {
        self.stop.store(true, .release);
        self.wake.notifyOne(self.io);
    }

    fn drain(ctx: *anyopaque, ino: u64, cutoff_ns: u64) open_table.Drained {
        const self: *WritebackTimer = @ptrCast(@alignCast(ctx));
        return self.fs.drainAged(ino, cutoff_ns);
    }

    fn run(self: *WritebackTimer) void {
        while (true) {
            var n = self.wake.notified();
            _ = n.enable(self.io);
            if (self.stop.load(.acquire)) {
                n.cancel(self.io);
                return;
            }
            _ = n.waitUntilUncancelable(self.io, clock.monotonicNs() + self.period_ns);
            if (self.stop.load(.acquire)) return;
            self.opens.timerPass(clock.monotonicNs(), self.age_ns, .{ .ctx = self, .call = &drain });
            self.fs.afterTimerPass();
        }
    }
};

/// THE ATIME WATCH (charter §4 "Access time"; `fuse/atime.zig`): the
/// mount's atime law is the KERNEL MOUNT's flags, read from the daemon's
/// own entry in `/proc/self/mountinfo` (by the mount id `mount(2)` gave,
/// else the path) once before ready and again at every change of the
/// mount table — a `POLL_ADD(POLLPRI)` on an open fd of mountinfo
/// (`exec/fdwatch.exceptional`) parks this fiber until the kernel's
/// `touch_mnt_namespace` wakes it, which `-o remount,strictatime`
/// (`do_reconfigure_mnt` → `set_mount_attributes`) does; the kernel sends
/// a FUSE daemon nothing else on a flag remount. Nothing per op, nothing
/// per tick: one table read per mount-table change on the host.
/// `SQUEEZEFS_ATIME` overrides the posture word. An entry that is gone
/// (the unmount in flight) leaves the standing law alone.
const AtimeWatch = struct {
    fs: *BackendFs,
    io: Io,
    mount_id: ?u64,
    mountpoint: []const u8,
    /// The knob's word (`auto` = the mount's).
    knob: []const u8,
    buf: []u8,
    stop: std.atomic.Value(bool) = .init(false),
    /// The law was said once (the mount's line; a change says it again).
    said: bool = false,

    fn requestStop(self: *AtimeWatch) void {
        self.stop.store(true, .release);
    }

    /// Read the law once (the mount's bring-up calls it before ready).
    fn refresh(self: *AtimeWatch) void {
        const text = mountinfo.read(self.io, self.buf);
        const e = (if (self.mount_id) |id| mountinfo.entryOf(text, id) else null) orelse mountinfo.entryAt(text, self.mountpoint) orelse return;
        const law = atime.override(atime.lawOf(e.options, e.super_options), self.knob);
        const changed = self.fs.atime_law.set(law);
        if (changed or !self.said) log.info("atime: {s}{s}{s} ({s})", .{ law.posture.name(), if (law.nodiratime) ",nodiratime" else "", if (law.read_only) ",ro" else "", if (std.mem.eql(u8, self.knob, "auto")) "the kernel mount's flags" else "SQUEEZEFS_ATIME over the kernel mount's flags" });
        self.said = true;
    }

    fn run(self: *AtimeWatch) void {
        const fd_rc = linux.open(mountinfo.path, .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
        if (linux.errno(fd_rc) != .SUCCESS) {
            log.warn("atime: {s} cannot be opened ({t}); the posture stays as read at mount — a flag remount is not seen", .{ mountinfo.path, linux.errno(fd_rc) });
            return;
        }
        const fd: linux.fd_t = @intCast(fd_rc);
        defer _ = linux.close(fd);
        while (!self.stop.load(.acquire)) {
            _ = exec.fdwatch.exceptional(fd) catch |e| {
                if (e != error.Canceled) log.warn("atime: the mount-table watch ended ({t}); the posture stays as it stands — a flag remount is not seen", .{e});
                return;
            };
            if (self.stop.load(.acquire)) return;
            self.refresh();
        }
    }
};

/// The landing-hold seam's trigger (`SQUEEZEFS_TEST_LANDING_HOLD_AT_WRITE`):
/// from this write on, every window stays on the lane un-issued until the
/// daemon dies — what landed before it stays landed.
fn holdLandingFromWrite(at_write: u64) void {
    journal_lane.test_seams.delay_issue.store(1, .release);
    log.warn("test seam: SQUEEZEFS_TEST_LANDING_HOLD_AT_WRITE — journal writes are held un-issued from write {d} on; acked metadata from here lives in the journal tail alone", .{at_write});
}

/// The `il0` face binder the host calls at arm and unarm.
fn bindIl0Thunk(ctx: *anyopaque, face: ?handler.kv_fs.Il0Face) void {
    const fs: *BackendFs = @ptrCast(@alignCast(ctx));
    fs.bindIl0(face);
}

/// The KD-11 invalidator's hook: `FUSE_NOTIFY_INVAL_INODE` on the sideband.
fn invalThunk(ctx: ?*anyopaque, ino: u64, off: i64, len: i64) error{QueueFull}!void {
    const session: *Session = @ptrCast(@alignCast(ctx.?));
    return session.notifyInvalInode(ino, off, len);
}

/// The invalidator's awaited size announcement (a grow, before its ack).
fn invalSyncThunk(ctx: ?*anyopaque, ino: u64) void {
    const session: *Session = @ptrCast(@alignCast(ctx.?));
    session.notifyAttrsAwaited(ino);
}

/// The invalidator's never-dropped attrs-only notify (a reserved cell).
fn invalAttrsThunk(ctx: ?*anyopaque, ino: u64) void {
    const session: *Session = @ptrCast(@alignCast(ctx.?));
    session.notifyAttrs(ino);
}

/// The mounted root's `st_dev` (the fd screen's mount authority): an
/// `O_PATH` open of the mountpoint served by our own lanes.
/// Zero-message opens (decision 0003 §AF): the kernel latches `no_open` at
/// the connection's first OPEN answered ENOSYS, and until then a file a
/// CREATE opened still sends a RELEASE at its last close. The daemon opens
/// its own `.stats` once before it reports ready (the request is the
/// workers', the opener this task — as `mountDevice`'s GETATTR), so the
/// posture holds from the first application request. Its close is the
/// write-back timer's latch too (decision 0003 §AJ): a zero-message open
/// carries the kernel's own `FOPEN_KEEP_CACHE` — no reply could add
/// `FOPEN_NOFLUSH` — so its FLUSH answered ENOSYS sets `fc->no_flush` and
/// every later close(2) on the connection is local.
fn latchNoOpen(mountpoint: [:0]const u8) void {
    var buf: [linux.PATH_MAX:0]u8 = undefined;
    const path = std.fmt.bufPrintSentinel(&buf, "{s}/{s}", .{ mountpoint, handler.virtual_inodes.STATS_NAME }, 0) catch {
        log.warn("mount: the no-open latch's path does not fit PATH_MAX under {s}; the first application open latches it", .{mountpoint});
        return;
    };
    const rc = linux.open(path, .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) {
        log.warn("mount: {s} could not be opened to latch no-open ({t}); the first application open latches it", .{ path, linux.errno(rc) });
        return;
    }
    _ = linux.close(@intCast(rc));
}

fn mountDevice(mountpoint: [:0]const u8) ?u64 {
    const rc = linux.openat(linux.AT.FDCWD, mountpoint.ptr, .{ .ACCMODE = .RDONLY, .PATH = true, .DIRECTORY = true, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) return null;
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    return ipc_rl.deviceOf(fd);
}

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

/// THE KERNEL'S COHERENCE (decision 0003 §AB.2): the mount's TTL law, the
/// conveyors' kernel view (a change this kernel did not ask for, named to
/// it) and the managers' peer gate (the first join flips the law to PEERED
/// and invalidates the kernel's whole view). A producer — the journal lane
/// at a window's terminal outcome — NEVER waits: it queues the names and
/// the objects before the member's ack and the NOTIFIER thread writes them
/// (the sideband thread itself commits — a FORGET's destroy — so a lane
/// waiting on a sideband cell is a deadlock; measured: the first cut wedged
/// an rmdir for 75 s). A full queue owes the kernel a whole-view flush
/// (`INC_EPOCH` + attrs-only `INVAL_INODE` of every ino it holds): lossless.
const KernelCoherence = struct {
    io: Io,
    gpa: std.mem.Allocator,
    law: mount_options.TtlLaw,
    opens: *open_table.OpenTable,
    queue: mpmc.BoundedMpmc(Item) = undefined,
    /// Bumped at every push; the notifier parks on it.
    kick: futex.Word = .init(0),
    flush_owed: std.atomic.Value(bool) = .init(false),
    /// Whole-view flushes written (a `enterPeered` waits for the next).
    flushes: futex.Word = .init(0),
    stop: std.atomic.Value(bool) = .init(false),
    session: ?*Session = null,
    thread: ?std.Thread = null,
    name_buf: thread_name.CommBuf = undefined,
    view: conveyor.KernelView = undefined,
    gate: sym_manager.PeerGate = undefined,

    /// The queue: a full terminal outcome's worth of names and objects for
    /// every lane at once — 1,024 items (64 members × 16), 272 KiB.
    const QUEUE_CAP: usize = 1024;

    const Item = struct {
        /// A global ino: the parent of `name`, or the object whose
        /// attributes go (`name_len` 0).
        ino: u64,
        name_len: u8,
        name: [record_mod.NAME_MAX]u8,
    };

    fn init(self: *KernelCoherence) std.mem.Allocator.Error!void {
        self.queue = try mpmc.BoundedMpmc(Item).init(self.gpa, QUEUE_CAP);
        self.view = .{ .ctx = self, .invalidate = &invalidate };
        self.gate = .{ .ctx = self, .enter = &enterPeered };
    }

    fn deinit(self: *KernelCoherence) void {
        std.debug.assert(self.thread == null);
        self.queue.deinit(self.gpa);
    }

    /// The session is up: the notifier starts writing into it.
    fn attach(self: *KernelCoherence, session: *Session) std.Thread.SpawnError!void {
        self.session = session;
        self.thread = try std.Thread.spawn(.{}, notifierMain, .{self});
    }

    /// Before the session goes (every conveyor disarmed first): the
    /// notifier stops; what it had not written dies with the session.
    fn detach(self: *KernelCoherence) void {
        const t = self.thread orelse return;
        self.stop.store(true, .release);
        _ = self.kick.fetchAdd(1, .release);
        futex.wakeAll(&self.kick);
        futex.wakeAll(&self.flushes);
        t.join();
        self.thread = null;
        self.session = null;
    }

    fn push(self: *KernelCoherence, ino: u64, name: []const u8) void {
        var it: Item = .{ .ino = ino, .name_len = @intCast(name.len), .name = undefined };
        @memcpy(it.name[0..name.len], name);
        if (!self.queue.tryPush(it)) self.flush_owed.store(true, .release);
    }

    /// A whole-view flush owed, without waiting for it (a re-join in
    /// place: the evicted incarnation's view may be stale).
    fn flushWholeView(self: *KernelCoherence) void {
        if (self.thread == null) return;
        self.flush_owed.store(true, .release);
        self.wakeNotifier();
    }

    fn wakeNotifier(self: *KernelCoherence) void {
        // release pairs with the notifier's acquire of `kick`: the pushes
        // (and an owed flush) are visible when it wakes.
        _ = self.kick.fetchAdd(1, .release);
        futex.wakeOne(&self.kick);
    }

    fn invalidate(ctx: *anyopaque, names: []const tx.NameNote, unnamed: bool, objects: []const u64) void {
        const self: *KernelCoherence = @ptrCast(@alignCast(ctx));
        for (names) |n| if (route_table.kernelGlobalOf(n.parent)) |g| self.push(g, n.name);
        for (objects) |o| if (route_table.kernelGlobalOf(o)) |g| self.push(g, "");
        if (unnamed) if (self.session) |s| s.notifyIncEpoch();
        self.wakeNotifier();
    }

    /// The first join: replies take the peered classes from here on, then
    /// the kernel's whole view goes stale — no entry cached under the
    /// isolated posture outlives the first peer writer. The join is
    /// answered after the flush is written.
    fn enterPeered(ctx: *anyopaque) void {
        const self: *KernelCoherence = @ptrCast(@alignCast(ctx));
        if (!self.law.enterPeered()) return;
        handler.kv_fs.fuse_ttl_law_peered.set(1);
        log.info("mount: a member joined — the kernel's TTLs take the peered classes (decision 0003 §AB.2); its whole view is invalidated", .{});
        if (self.thread == null) return;
        const f0 = self.flushes.load(.acquire);
        self.flush_owed.store(true, .release);
        self.wakeNotifier();
        while (self.flushes.load(.acquire) == f0 and !self.stop.load(.acquire)) {
            _ = futex.waitTimeout(&self.flushes, f0, tick_law.tick_ns);
        }
    }

    fn notifierMain(self: *KernelCoherence) void {
        _ = thread_name.nameCurrentThread(&self.name_buf, "sqz-kview");
        const s = self.session.?;
        while (!self.stop.load(.acquire)) {
            const k = self.kick.load(.acquire);
            if (self.flush_owed.swap(false, .acq_rel)) self.flushAll(s);
            while (self.queue.tryPop()) |it| {
                if (it.name_len != 0) s.notifyInvalEntryOwed(it.ino, it.name[0..it.name_len]) else s.notifyAttrs(it.ino);
            }
            if (self.flush_owed.load(.acquire)) continue;
            _ = futex.waitTimeout(&self.kick, k, tick_law.tick_ns);
        }
    }

    /// Every dentry (one epoch) and every attribute the kernel holds.
    fn flushAll(self: *KernelCoherence, s: *Session) void {
        s.notifyIncEpoch();
        if (self.opens.trackedInos(self.gpa)) |inos| {
            defer self.gpa.free(inos);
            for (inos) |ino| s.notifyAttrs(ino);
        } else |_| {
            // No room to list them: owed again (the dentries went with the
            // epoch); the next round retries.
            log.err("mount: out of memory listing the kernel's inodes for a whole-view flush — retried", .{});
            self.flush_owed.store(true, .release);
        }
        _ = self.flushes.fetchAdd(1, .release);
        futex.wakeAll(&self.flushes);
    }
};

/// The mount's half of a re-join in place (`sym_wire.RejoinHost`; charter
/// §5.5 "A live member evicted …"; decision 0003 §BS): what the data plane
/// and the filesystem hold un-landed, and the mount's own re-bindings for
/// the successor incarnation.
const RejoinHost = struct {
    fs: *BackendFs,
    plane: *plane_mod.DataPlane,
    set: *meta_set.MetaSet,
    coherence: *KernelCoherence,
    registry: *watchdog.Registry,
    gpa: Allocator,
    io: Io,
    /// Guards the drain's words: the beat marks and polls, the teardown
    /// unbinds the interception host (before it disarms) and frees them.
    lock: Io.Mutex = .init,
    /// The interception host while it serves (its ops read the
    /// incarnations exactly as FUSE ops do).
    ipc: ?*ipc_host.IpcHost = null,
    /// The FUSE ops in flight at the last mark (registry slot, request
    /// unique) and the interception sessions with ops in flight then.
    marked: std.ArrayList(Marked) = .empty,
    ipc_marked: std.ArrayList(ipc_host.IpcHost.InflightMark) = .empty,
    /// A mark ran out of memory: the next poll marks again (every op in
    /// flight at the publication and still in flight is in flight then).
    remark: bool = false,
    closed: bool = false,

    const Marked = struct { slot: u32, unique: u64 };

    fn face(self: *RejoinHost) sym_wire.RejoinHost {
        return .{ .ctx = self, .unlanded = &unlanded, .rebound = &rebound, .opsMark = &opsMark, .opsDrained = &opsDrained };
    }

    fn bindIpc(self: *RejoinHost, h: ?*ipc_host.IpcHost) void {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        self.ipc = h;
        self.ipc_marked.clearRetainingCapacity();
    }

    fn deinit(self: *RejoinHost) void {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        self.closed = true;
        self.ipc = null;
        self.marked.deinit(self.gpa);
        self.ipc_marked.deinit(self.gpa);
    }

    fn opsMark(ctx: *anyopaque) void {
        const self: *RejoinHost = @ptrCast(@alignCast(ctx));
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        if (!self.closed) self.markLocked();
    }

    /// Every FUSE op the registry holds now (acquire pairs with a claim's
    /// acq_rel CAS: the slot's unique is the claimer's) and every
    /// interception session with an op in flight.
    fn markLocked(self: *RejoinHost) void {
        self.marked.clearRetainingCapacity();
        self.ipc_marked.clearRetainingCapacity();
        self.remark = false;
        for (self.registry.slots, 0..) |*s, i| {
            if (s.state.load(.acquire) == 0) continue;
            self.marked.append(self.gpa, .{ .slot = @intCast(i), .unique = s.unique }) catch {
                self.remark = true;
                return;
            };
        }
        if (self.ipc) |h| h.markInflight(self.gpa, &self.ipc_marked) catch {
            self.remark = true;
        };
    }

    /// Every marked op answered: its slot freed or claimed by another
    /// request (another unique), every marked session idle since.
    fn opsDrained(ctx: *anyopaque) bool {
        const self: *RejoinHost = @ptrCast(@alignCast(ctx));
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        if (self.closed) return true;
        if (self.remark) {
            self.markLocked();
            return false;
        }
        var i: usize = 0;
        while (i < self.marked.items.len) {
            const m = self.marked.items[i];
            const s = &self.registry.slots[m.slot];
            if (s.state.load(.acquire) != 0 and s.unique == m.unique) i += 1 else _ = self.marked.swapRemove(i);
        }
        const ipc_drained = if (self.ipc) |h| h.inflightDrained(&self.ipc_marked) else true;
        return self.marked.items.len == 0 and ipc_drained;
    }

    /// The data plane's words, then the filesystem's parked refinements
    /// (journaled within one flush interval; parked past the eviction they
    /// were never journaled into the recovered region).
    fn unlanded(ctx: *anyopaque, buf: []u8) ?[]const u8 {
        const self: *RejoinHost = @ptrCast(@alignCast(ctx));
        if (self.plane.unlandedWord(buf)) |w| return w;
        const parked = self.fs.pending.len();
        if (parked != 0) return std.fmt.bufPrint(buf, "{d} parked time refinement(s) never journaled", .{parked}) catch "parked time refinements";
        return null;
    }

    /// The successor is published: its node caches join the budget, the
    /// read cache and the kernel's whole view drop what the evicted
    /// incarnation served.
    fn rebound(ctx: *anyopaque) void {
        const self: *RejoinHost = @ptrCast(@alignCast(ctx));
        for (self.set.volumes) |vol| mem_budget.authority.register(vol.budgetComponent());
        self.plane.dropIncarnationViews();
        self.coherence.flushWholeView();
    }
};

/// The serve loop: parks on the daemon's eventfd; returns when the
/// teardown may begin. A stop (SIGTERM/SIGINT, what `squeezefs umount`
/// sends) first runs the holder census (decision 0003 §S): a busy mount
/// REFUSES the stop — logged with its holders, counted
/// (`transport_umount_busy_refusals`), the daemon serving on — because an
/// orderly unmount of a busy mount is EBUSY on every Linux filesystem and
/// the teardown's drains cannot be undone once begun. The lazy stop
/// (SIGUSR2, `umount --lazy`) detaches an unheld mount orderly (the same
/// drain-first teardown) and a held one NOW, serving its holders until the
/// kernel ends the connection (the sideband's disconnect fires the
/// `on_shutdown` hook, and the teardown runs then).
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
        if (bits & (daemon.SIG_STOP | daemon.SIG_STOP_LAZY) != 0) {
            const lazy = bits & daemon.SIG_STOP_LAZY != 0;
            const what: []const u8 = if (lazy) "lazy stop requested (SIGUSR2)" else "stop requested (SIGTERM/SIGINT)";
            // The census over everyone but this process (its own fds are
            // the devices and the staging segments, never the mount); the
            // stop's own sender reading `.stats`/`.config`/`.trace` holds
            // nothing (`squeezefs umount` polls `.stats` for this verdict).
            const exempt: holders.Exempt = .{ .pid = daemon.Signals.stopSender(), .fd_ino = &handler.virtual_inodes.isVirtualIno };
            // The census' cost is said on every verdict: `umount --lazy`
            // waits for it, and a walk the host's process table makes slow
            // is this line's to show.
            const census_start = clock.monotonicNs();
            const c = holders.censusAt(ctx.p.gpa, ctx.p.io, ctx.mountpoint, &.{linux.getpid()}, exempt);
            const census_ms = (clock.monotonicNs() - census_start) / std.time.ns_per_ms;
            if (c != null and c.?.busy()) {
                if (!lazy) {
                    log.warn("{s} but {s} is busy ({f}; census {d} ms) — not unmounting; still serving (`squeezefs umount` retries when they are gone; `--lazy` detaches now and serves them to the end)", .{ what, ctx.mountpoint, c.?, census_ms });
                    transport.session.umount_busy_refusals.inc();
                    continue;
                }
                if (session.detachLazily()) {
                    log.warn("{s}: {s} detached lazily with {f} (census {d} ms) — serving them until the kernel ends the connection, then tearing down", .{ what, ctx.mountpoint, c.?, census_ms });
                } else {
                    log.warn("{s}: {s} is no longer attached — the connection ends when its last holder leaves ({f}; census {d} ms)", .{ what, ctx.mountpoint, c.?, census_ms });
                }
                continue;
            }
            log.info("{s}: unmounting {s} (census {d} ms, {s})", .{ what, ctx.mountpoint, census_ms, if (c == null) "no entry for the mountpoint in the table" else "no holder" });
            return;
        }
        // acquire pairs with the hook's release.
        if (ctx.session_ended.load(.acquire) or !session.isReady()) {
            if (session.endedUnderLiveMount()) {
                ctx.connection_aborted = true;
                log.err("the kernel ended the FUSE connection under the live mount at {s} — a request outlived the {d} s request timeout (this daemon was stopped or wedged past the fail-stop lattice's deadline, decision 0003 §Z) or the connection was aborted through fusectl: the mount is fail-stopped (ENOTCONN) until it is unmounted and mounted again; tearing down", .{ ctx.mountpoint, session.requestTimeoutS() });
                return;
            }
            log.info("the FUSE session ended ({s}): tearing down", .{if (session.isMounted()) "shutdown" else "unmounted"});
            return;
        }
    }
}

/// SIGUSR1: every `.stats` family through the ONE aggregator, one log
/// line per family (`stats <name>: {…}`; the daemon line last).
fn dumpStats(vs: *VirtualSource) void {
    stats.logFamilies(&vs.renderer);
}

/// The `.stats`/`.config`/`.trace` renderer bound into the filesystem
/// (`kv_fs.VirtualFace`): ONE `stats.Renderer` over the daemon's sources,
/// the set config for `.config`, the op-trace drain for `.trace`. A
/// lookup's render runs on the BLOCKING POOL (decision 0003 §CC.1): the
/// handler is a fiber on the drain worker's own driven lane, so a render
/// there held the worker — the ring not pumped, its queue's requests
/// waiting — for the render's length (27 ms idle, hundreds under load,
/// serialized across every reader); on the pool the fiber parks and the
/// worker pumps on. A refused job (the pool's queue full) renders inline.
const VirtualSource = struct {
    renderer: stats.Renderer,
    cfg: *const set_config.Config,
    sqz: *SqzIo,

    fn build(ctx: *anyopaque, class: handler.virtual_inodes.Class, gpa: Allocator) Allocator.Error!handler.virtual_inodes.Owned {
        const self: *VirtualSource = @ptrCast(@alignCast(ctx));
        return self.sqz.runBlocking(buildInline, .{ self, class, gpa }) catch |e| switch (e) {
            error.ConcurrencyUnavailable => buildInline(self, class, gpa),
        };
    }

    fn buildInline(self: *VirtualSource, class: handler.virtual_inodes.Class, gpa: Allocator) Allocator.Error!handler.virtual_inodes.Owned {
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

test "the reclaim cadence is one watchdog tick; the supervisor's probe interval is that tick (its heartbeat is the watchdog's)" {
    try testing.expectEqual(watchdog.TICK_NS, RECLAIM_CADENCE_NS);
    try testing.expectEqual(watchdog.TICK_NS, cli.supervisor.PROBE_INTERVAL_NS);
}

test "the times drain cadence on a FAILED store (fstests generic/475 under dm-error: a line per 50 ms tick until the unmount): the first tick names the set ONCE, every tick on the latch drains nothing and counts, a tick with nothing parked says and counts nothing; a live store's tick drains as before" {
    const kv_fs = handler.kv_fs;
    var store = try kv_fs.MemStore.init(testing.allocator);
    defer store.deinit();
    var nodata: kv_fs.NoData = .{};
    var fs = try kv_fs.MemFs.mount(testing.allocator, testing.io, &store, &nodata, .{ .dlm_stripes = 64 });
    defer fs.unmount();
    var cadence: TimesDrainCadenceOf(kv_fs.MemFs) = .{ .fs = &fs, .io = testing.io, .interval_ns = 0, .meta_uri = "sqmeta:///dev/mapper/failed-meta" };
    const failed0 = kv_fs.fuse_write_times_drain_failed_ticks.load();
    // A stamp parked on the root (stored mtime 1): a Δ for the drain.
    const root = try kv_fs.MemFs.localOf(1);
    const stamp: u64 = @bitCast(clock.realtimeCoarseNs());
    _ = try fs.pending.park(root, stamp, stamp);
    // Live: the tick drains it (one commit), nothing is counted.
    cadence.tick();
    try testing.expectEqual(@as(u64, 1), store.commits);
    try testing.expectEqual(@as(u64, 0), fs.pending.len());
    try testing.expectEqual(failed0, kv_fs.fuse_write_times_drain_failed_ticks.load());
    try testing.expect(!cadence.failed_named);
    // FAILED with a refinement parked: one line across 100 ticks, 100
    // ticks counted, no commit attempted, the refinement still parked.
    _ = try fs.pending.park(root, stamp + 1, stamp + 1);
    store.failed = true;
    log.testing.expectErrorLogs(1);
    for (0..100) |_| cadence.tick();
    try log.testing.expectAllFired();
    try testing.expect(cadence.failed_named);
    try testing.expectEqual(failed0 + 100, kv_fs.fuse_write_times_drain_failed_ticks.load());
    try testing.expectEqual(@as(u64, 1), store.commits);
    try testing.expectEqual(@as(u64, 1), fs.pending.len());
    // Nothing parked: nothing to drain, nothing counted.
    _ = fs.pending.retire(root);
    try testing.expectEqual(@as(u64, 0), fs.pending.len());
    cadence.tick();
    try testing.expectEqual(failed0 + 100, kv_fs.fuse_write_times_drain_failed_ticks.load());
}

// THE REPLAY PROOF under load: the cadence's drain of a directory's parked
// stamp held the directory's exclusive inode guard until its window
// LANDED (a tool's commit is acked at the landing); under the landing-hold
// seam that is never, and the next create in the directory parked 115 s on
// the guard. The stamps are the ones this mount's kernel already holds
// through the fold — the drain the fsync/syncfs paths run over the same
// stamps is a kernel-origin commit, acked at its apply.
test "the times drain cadence commits as this mount's kernel's own op — acked at its apply like the syncfs drain of the same stamps, never its exclusive inode guards held across a journal landing (red: a tool's commit, acked at the landing — under the landing-hold seam a create into the drained directory parked on its guard for good)" {
    const kv_fs = handler.kv_fs;
    var store = try kv_fs.MemStore.init(testing.allocator);
    defer store.deinit();
    var nodata: kv_fs.NoData = .{};
    var fs = try kv_fs.MemFs.mount(testing.allocator, testing.io, &store, &nodata, .{ .dlm_stripes = 64 });
    defer fs.unmount();
    var cadence: TimesDrainCadenceOf(kv_fs.MemFs) = .{ .fs = &fs, .io = testing.io, .interval_ns = 0, .meta_uri = "sqmeta:///dev/null" };
    const root = try kv_fs.MemFs.localOf(1);
    const stamp: u64 = @bitCast(clock.realtimeCoarseNs());
    _ = try fs.pending.park(root, stamp, stamp);
    const commits0 = store.commits;
    cadence.tick();
    try testing.expectEqual(commits0 + 1, store.commits);
    try testing.expectEqual(@as(u64, 0), fs.pending.len());
    try testing.expect(store.last_kernel_origin);
}
