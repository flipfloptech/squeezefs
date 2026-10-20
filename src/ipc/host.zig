//! The daemon side of the client bypass (charter §7; spec `docs/porting/
//! il_daemon.md`): `IpcHost` — armed by `-o interception` — owns the
//! `il0` rendezvous (the abstract + path `SOCK_SEQPACKET` listeners, the
//! blob the mount's GETXATTR answers, the nonce), the session ADMISSION
//! (the HELLO ladder as `rendezvous.admitHello` over live budget facts,
//! the balance-first owner pick with the reservation under the SAME lock
//! — the retired registry-scan count raced its own insert and convoyed 32
//! simultaneous HELLOs onto the low indices — the sealed memfd, the arena's
//! node bind, SESSION_OK), the service threads spawned ON ADMISSION lowest
//! owner index first up to the ceiling (`host/service.zig`), the direct-
//! drive lanes built at arm (`host/direct.zig`), the ctl poller (ONE
//! thread `poll(2)`-ing every live session's socket: BIND/UNBIND, EOF =
//! the client died, the idle reap tick — decision 0002 row 310: a bounded
//! CONNECTION cap, never a thread per connection), the severance pools,
//! the KD-11 invalidator, the R5 components, poison, and the teardown in
//! order: every session poisoned → the threads joined → the listeners
//! closed → the xattr unarmed.
//!
//! Lock discipline: `reg_lock` (a `std.Io.Mutex` — control plane, held
//! for table edits only, never across a wait) guards the session table,
//! the budget ledger, the per-uid counts and the owner loads; `epoch` is
//! the service threads' re-collect signal (they touch the lock only when it
//! moved). The nonce has its own lock (rotation at blob synthesis and
//! HELLO). Teardown runs exactly once per session (`torn_down`), joins the
//! handoff group, then drops the registry's mapping share; the dd lane's
//! in-flight entries and a stale service pass hold their own shares.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const Atomic = std.atomic.Value;
const Allocator = std.mem.Allocator;

const clock = @import("../core/clock.zig");
const log = @import("../core/log.zig");
const knobs = @import("../core/knobs.zig");
const derive = @import("../core/derive.zig");
const thread_name = @import("../core/thread_name.zig");
const mem_budget = @import("../mem/budget.zig");
const mem_sizing = @import("../mem/sizing.zig");
const placement = @import("../exec/placement.zig");
const sqz_io = @import("../exec/sqz_io.zig");
const filesystem = @import("../fuse/handler/filesystem.zig");
const op_guard = @import("../fuse/handler/op_guard.zig");
const open_table = @import("../fuse/handler/open_table.zig");
const kv_fs = @import("../fuse/handler/kv_fs.zig");
const route_table = @import("../meta/routing/route_table.zig");
const plane_mod = @import("../data/plane.zig");
const router = @import("../data/router.zig");
const layout = @import("layout.zig");
const wire = @import("wire.zig");
const sizing = @import("sizing.zig");
const rendezvous = @import("rendezvous.zig");
const rl = @import("rendezvous_linux.zig");
const shm = @import("shm.zig");
const doorbell = @import("doorbell.zig");
const ipc = @import("ipc.zig");

pub const stats = @import("host/stats.zig");
pub const severed_pool = @import("host/severed_pool.zig");
pub const placed = @import("host/placed.zig");
pub const placed_claims_model = @import("host/placed_claims_model.zig");
pub const inval = @import("host/inval.zig");
pub const session = @import("host/session.zig");
pub const service = @import("host/service.zig");
pub const direct = @import("host/direct.zig");
pub const direct_lease = @import("host/direct_lease.zig");
pub const e2e_linux = @import("host/e2e_linux.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("ipc/host is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

const hs = stats.stats;
const Session = session.Session;

fn sleepNs(ns: u64) void {
    const ts: linux.timespec = .{ .sec = @intCast(ns / std.time.ns_per_s), .nsec = @intCast(ns % std.time.ns_per_s) };
    _ = linux.nanosleep(&ts, null);
}

/// The idle reap tick: four ticks per idle window, railed 100 ms..5 s.
pub fn reapTickNs(idle_ns: u64) u64 {
    return derive.clamp(idle_ns / 4, 100 * std.time.ns_per_ms, 5 * std.time.ns_per_s);
}

/// `SQUEEZEFS_IPC_DD_SHARDS` verbatim else the ONE width function.
pub fn resolveDdShards(explicit: ?u32, cpus: u32) u32 {
    return explicit orelse sizing.ilDrainLanesDefault(cpus);
}

pub fn ddShardsFromEnv(env: *const std.process.Environ.Map, cpus: u32) u32 {
    return resolveDdShards(knobs.optIntKnob("SQUEEZEFS_IPC_DD_SHARDS", u32, env), cpus);
}

/// The host's uid for the kill-priv class: an unprivileged peer (`uid !=
/// 0`) stands in for the kernel's per-write `!capable(CAP_FSETID)`.
pub fn killPrivClass(peer_uid: u32) bool {
    return peer_uid != 0;
}

pub const Deps = struct {
    gpa: Allocator,
    io: Io,
    sqz: *sqz_io.SqzIo,
    env: *const std.process.Environ.Map,
    facts: derive.Facts,
    budget_bytes: u64,
    plane: *plane_mod.DataPlane,
    fs: filesystem.Filesystem,
    /// The `il0` face's binder (the mounted `KvFs`).
    il0_bind: *const fn (ctx: *anyopaque, face: ?kv_fs.Il0Face) void,
    il0_ctx: *anyopaque,
    guards: *op_guard.Table,
    probe: op_guard.Probe,
    shared_enabled: bool,
    opens: *open_table.OpenTable,
    /// `Session.notifyInvalInode` (the KD-11 invalidator's hook).
    notify: inval.Hook,
    our_commit: []const u8,
    /// The mount root's device (`rendezvous_linux.deviceOf`, resolved
    /// BEFORE the host arms — decision 0002 row 309).
    mount_dev: u64,
    mount_uid: u32,
    block_size: u64,
    max_file_size: u64,
    /// `-o direct_device_true`.
    topo: *const placement.Topology,
    /// `-o interception=direct`: the trusted-client direct arm (charter §7
    /// "The direct arm").
    direct_clients: bool = false,
};

pub const ArmError = error{ OutOfMemory, ListenerFailed, SpawnFailed, EngineFailed, Eventfd, DirectArm };

pub const IpcHost = struct {
    gpa: Allocator,
    io: Io,
    d: Deps,
    // ---- policy ----
    arena_cap_bytes: u64,
    default_geometry: layout.Geometry,
    per_uid_cap: u64,
    allow_dirty: bool,
    thp: bool,
    latch: bool,
    shed_target: Atomic(u64) = .init(0),
    // ---- rendezvous ----
    nonce_lock: std.Io.Mutex = .init,
    nonces: rendezvous.NonceState,
    name_buf: [rendezvous.SOCKET_NAME_MAX]u8 = undefined,
    name_len: usize = 0,
    abstract_fd: linux.fd_t = -1,
    path: ?rl.PathListener = null,
    accept_threads: [2]?std.Thread = .{ null, null },
    handshakes: Atomic(u32) = .init(0),
    ctl_cap: u32,
    // ---- registry ----
    reg_lock: std.Io.Mutex = .init,
    sessions: []?*Session,
    live: u32 = 0,
    in_use_bytes: u64 = 0,
    uid_counts: std.AutoHashMapUnmanaged(u32, u32) = .empty,
    epoch: Atomic(u64) = .init(1),
    owners: []*service.Owner,
    owner_load: []u32,
    owner_node: []?u16,
    spawned: u16 = 0,
    svc_stop: Atomic(bool) = .init(false),
    // ---- the ctl poller ----
    poll_efd: linux.fd_t,
    poll_thread: ?std.Thread = null,
    // ---- the sink ----
    pool: severed_pool.SeveredPool,
    placed: placed.Registry,
    inval: inval.Invalidator,
    dd: ?*direct.Engine,
    sink: *service.DataPlaneSink,
    /// The direct arm's host half (`-o interception=direct`).
    direct_host: ?direct_lease.DirectHost,
    stop: Atomic(bool) = .init(false),
    armed_at_ns: u64,

    /// Arm (module doc): the pools, the lanes, the listeners, the poller,
    /// the `il0` face. Service threads spawn on admission.
    pub fn arm(d: Deps) ArmError!*IpcHost {
        const gpa = d.gpa;
        const self = try gpa.create(IpcHost);
        errdefer gpa.destroy(self);
        const cpus = d.facts.cpus;
        const arena_cap = mem_sizing.ipcArenaCapFromEnv(d.env, d.budget_bytes);
        const arena_bytes = sizing.ipcArenaBytesFromEnv(d.env, arena_cap, cpus);
        const max_op = sizing.maxOpBytesFromEnv(d.env, d.block_size);
        var geometry = layout.Geometry.default;
        geometry.arena_bytes = arena_bytes;
        geometry.slots = sizing.ipcSlotsForArena(arena_bytes);
        geometry.max_op_bytes = @intCast(@min(max_op, arena_bytes));
        const footprint = (layout.SessionLayout.compute(geometry) catch return error.OutOfMemory).total_bytes;
        const ctl_cap = sizing.ctlConnCap(arena_cap, footprint);
        const ceiling: u16 = @intCast(sizing.drainLanesFromEnv(d.env, cpus));
        const dd_shards: u16 = @intCast(ddShardsFromEnv(d.env, cpus));
        self.* = .{
            .gpa = gpa,
            .io = d.io,
            .d = d,
            .arena_cap_bytes = arena_cap,
            .default_geometry = geometry,
            .per_uid_cap = mem_sizing.ipcPerUidSessionCap(arena_cap, arena_bytes),
            .allow_dirty = rendezvous.allowDirtyFromEnv(d.env),
            .thp = shm.thpFromEnv(d.env),
            .latch = doorbell.latchFromEnv(d.env),
            .nonces = rendezvous.NonceState.init(&rl.fillNonce, clock.monotonicNs()),
            .ctl_cap = ctl_cap,
            .sessions = undefined,
            .owners = undefined,
            .owner_load = undefined,
            .owner_node = undefined,
            .poll_efd = -1,
            .pool = undefined,
            .placed = undefined,
            .inval = undefined,
            .dd = null,
            .sink = undefined,
            .direct_host = null,
            .armed_at_ns = clock.monotonicNs(),
        };
        if (d.direct_clients) {
            self.direct_host = direct_lease.DirectHost.init(d.plane, d.block_size) catch |e| {
                log.err("ipc: the direct arm could not arm ({s}): the lease page or a data device node", .{@errorName(e)});
                return error.DirectArm;
            };
        }
        errdefer if (self.direct_host) |*dh| dh.deinit();
        self.sessions = try gpa.alloc(?*Session, ctl_cap);
        errdefer gpa.free(self.sessions);
        @memset(self.sessions, null);
        try self.uid_counts.ensureTotalCapacity(gpa, ctl_cap);
        errdefer self.uid_counts.deinit(gpa);
        // The owner partition over the topology (memory follows thread).
        self.owner_node = try gpa.alloc(?u16, ceiling);
        errdefer gpa.free(self.owner_node);
        self.owner_load = try gpa.alloc(u32, ceiling);
        errdefer gpa.free(self.owner_load);
        @memset(self.owner_load, 0);
        self.partition(ceiling);
        // The pools and the invalidator.
        self.pool = try severed_pool.SeveredPool.init(gpa, @intCast(geometry.max_op_bytes), severed_pool.poolSlots(arena_cap, geometry.max_op_bytes));
        errdefer self.pool.deinit();
        self.placed = try placed.Registry.init(gpa, &d.plane.wp.table.pool, @intCast(d.block_size), placed.assemblyCapBytes(d.budget_bytes));
        errdefer self.placed.deinit();
        self.inval = try inval.Invalidator.init(gpa, op_guard.INODE_GUARD_STRIPES, d.notify, inval.Invalidator.windowMsFromEnv(d.env));
        errdefer self.inval.deinit(gpa);
        mem_budget.authority.register(mem_budget.Component.init("ipc_session_arenas", 0, 1, .{ .gauge = &hs.ipc_arena_bytes }, .{ .ctx = self, .call = &shedFn }));
        mem_budget.authority.register(self.pool.component());
        mem_budget.authority.register(self.placed.component());
        // The direct-drive lanes (at arm, never on the data path).
        var fds: [router.MAX_VOLUMES]linux.fd_t = undefined;
        var n_fds: usize = 0;
        for (d.plane.devs[0..d.plane.vols.n()]) |maybe| {
            fds[n_fds] = if (maybe) |dev| dev.directFd() else -1;
            n_fds += 1;
        }
        const direct_write = knobs.boolKnob("SQUEEZEFS_IL_DIRECT_WRITE", d.env);
        const direct_read = knobs.boolKnob("SQUEEZEFS_IL_DIRECT_READ", d.env);
        if (n_fds != 0) {
            const owned_fds = try gpa.dupe(linux.fd_t, fds[0..n_fds]);
            errdefer gpa.free(owned_fds);
            self.dd = direct.Engine.init(gpa, .{ .io = d.io, .wp = d.plane.wp, .vols = &d.plane.vols, .vol_fds = owned_fds, .pool = &self.pool, .inval = &self.inval, .opens = d.opens, .cfg = direct.Config.fromEnv(d.env, dd_shards), .latch = self.latch }) catch |e| {
                log.err("ipc: the direct-drive lanes could not build ({s})", .{@errorName(e)});
                return error.EngineFailed;
            };
        }
        errdefer if (self.dd) |dd| {
            dd.deinit();
            gpa.free(dd.vol_fds);
        };
        self.sink = try service.DataPlaneSink.init(gpa, .{
            .io = d.io,
            .sqz = d.sqz,
            .plane = d.plane,
            .fs = d.fs,
            .guards = d.guards,
            .probe = d.probe,
            .shared_enabled = d.shared_enabled,
            .opens = d.opens,
            .pool = &self.pool,
            .placed = &self.placed,
            .inval = &self.inval,
            .dd = self.dd,
            .direct_write = direct_write,
            .direct_read = direct_read,
            .max_file_size = d.max_file_size,
            .owners = ceiling,
            .drain_budget = geometry.slots,
        });
        errdefer self.sink.deinit();
        if (self.dd) |dd| dd.read_fallback = self.sink.readFallback();
        // The owners (threads spawn on admission).
        self.owners = try gpa.alloc(*service.Owner, ceiling);
        errdefer gpa.free(self.owners);
        const spin = doorbell.SpinGovernor.Config.fromEnv(d.env);
        var made: usize = 0;
        errdefer for (self.owners[0..made]) |o| o.deinit();
        for (self.owners, 0..) |*o, i| {
            const node = self.owner_node[i];
            const cpus_set: ?placement.CpuSet = if (node) |n| d.topo.nodeCpus(n, d.topo.mask) else null;
            o.* = try service.Owner.init(gpa, .{ .index = @intCast(i), .node = node, .cpus = cpus_set, .lanes = ceiling, .cores = @max(cpus, 1), .spin = spin, .max_sessions = ctl_cap }, self.registry(), self.sink.sink(), &self.svc_stop);
            o.*.latch = self.latch;
            made += 1;
        }
        // The listeners.
        var prng = std.Random.DefaultPrng.init(clock.monotonicNs());
        const name = rendezvous.socketName(&self.name_buf, @intCast(linux.getpid()), prng.random().int(u32));
        self.name_len = name.len;
        const backlog = sizing.listenBacklog(ctl_cap);
        self.abstract_fd = rl.listenAbstract(name, backlog) catch |e| {
            log.err("ipc: the abstract listener failed ({s})", .{@errorName(e)});
            return error.ListenerFailed;
        };
        errdefer _ = linux.close(self.abstract_fd);
        var dir_buf: rendezvous.DirBuf = undefined;
        if (rendezvous.socketDirFromEnv(&dir_buf, d.env, linux.geteuid())) |dir| {
            self.path = rl.listenPath(dir, name, backlog) catch |e| blk: {
                log.warn("ipc: the path rendezvous in {s} is refused ({s}); the mount degrades to the abstract rung alone", .{ dir, @errorName(e) });
                break :blk null;
            };
        }
        errdefer if (self.path) |*p| p.close();
        // The poller's eventfd and the threads.
        const efd = linux.eventfd(0, linux.EFD.CLOEXEC | linux.EFD.NONBLOCK);
        if (linux.errno(efd) != .SUCCESS) return error.Eventfd;
        self.poll_efd = @intCast(efd);
        errdefer _ = linux.close(self.poll_efd);
        if (self.dd) |dd| dd.start() catch return error.SpawnFailed;
        self.poll_thread = std.Thread.spawn(.{}, pollLoop, .{self}) catch return error.SpawnFailed;
        self.accept_threads[0] = std.Thread.spawn(.{}, acceptLoop, .{ self, self.abstract_fd, false }) catch return error.SpawnFailed;
        if (self.path) |p| self.accept_threads[1] = std.Thread.spawn(.{}, acceptLoop, .{ self, p.fd, true }) catch return error.SpawnFailed;
        // The face: GETXATTR of `il0` answers from now on.
        d.il0_bind(d.il0_ctx, .{ .ctx = self, .synthesize = &synthesizeFn });
        const population = sizing.ipcAdmittedPopulation(arena_cap, footprint);
        const population_floor = sizing.ipcSessionPopulationFloor(cpus);
        log.info("ipc: interception armed — socket {s}{s}{s}; {d} service lane(s), {d} dd shard(s), arena {d} MiB ({d} slots × {d} KiB) × cap {d} MiB = {d} session(s) (population floor {d}), ctl cap {d}", .{ name, if (self.path != null) " + " else "", if (self.path) |p| p.path() else "", ceiling, dd_shards, arena_bytes >> 20, geometry.slots, geometry.slotSlab() >> 10, arena_cap >> 20, population, population_floor, ctl_cap });
        // The arena already yielded to the floor down to its 4 MiB grain:
        // only the budget can admit more sessions.
        if (population < population_floor) log.warn("ipc: the admission cap {d} MiB holds {d} session(s) of {d} MiB — under the population floor {d} (2 × {d} cpus): client processes past it are kernel-served; raise SQUEEZEFS_MEM_BUDGET_MB or SQUEEZEFS_IPC_MEM_PCT", .{ arena_cap >> 20, population, arena_bytes >> 20, population_floor, cpus });
        return self;
    }

    /// Owner `i` → its partition node (a largest-remainder round-robin by
    /// node CPU share); one node ⇒ unpinned.
    fn partition(self: *IpcHost, ceiling: u16) void {
        const topo = self.d.topo;
        if (topo.node_count <= 1) {
            @memset(self.owner_node, null);
            return;
        }
        var shares: [placement.max_cpus]u32 = undefined;
        const nodes: usize = @min(topo.node_count, 256);
        @memset(shares[0..nodes], 0);
        var it = topo.mask.iterator(.{});
        while (it.next()) |cpu| {
            const n = topo.node_of[cpu];
            if (n != placement.no_node and n < nodes) shares[n] += 1;
        }
        var out: [256]u16 = undefined;
        layout.nodePartition(@min(ceiling, out.len), shares[0..nodes], &out);
        for (self.owner_node, 0..) |*slot, i| slot.* = if (i < out.len) out[i] else null;
    }

    /// Teardown in order (module doc).
    pub fn disarm(self: *IpcHost) void {
        self.stop.store(true, .release);
        // 1. Every live session poisoned (the shutdown broadcast — silent)
        //    and torn down (handoffs joined).
        while (self.takeAny()) |s| {
            s.poison(.host_shutdown);
            self.teardown(s, "host shutdown");
            _ = s.map.releaseRef();
        }
        // 2. The service threads.
        self.svc_stop.store(true, .release);
        for (self.owners) |o| o.kick();
        for (self.owners) |o| o.join();
        // 3. The dd reapers (their in-flight CQEs drain first).
        if (self.dd) |dd| dd.shutdown();
        // 4. The listeners: `shutdown(2)` unblocks a blocking accept (a
        //    close does not), then the threads join and the fds close.
        _ = linux.shutdown(self.abstract_fd, linux.SHUT.RDWR);
        if (self.path) |p| _ = linux.shutdown(p.fd, linux.SHUT.RDWR);
        for (&self.accept_threads) |*t| if (t.*) |th| {
            th.join();
            t.* = null;
        };
        // Handshakes in flight end on their own socket timeout; a bounded
        // wait keeps the teardown prompt without a wedge.
        const deadline = clock.monotonicNs() + rendezvous.CTL_HANDSHAKE_TIMEOUT_NS;
        while (self.handshakes.load(.acquire) != 0 and clock.monotonicNs() < deadline) sleepNs(std.time.ns_per_ms);
        _ = linux.close(self.abstract_fd);
        if (self.path) |*p| p.close();
        // 5. The poller.
        self.wakePoller();
        if (self.poll_thread) |t| {
            t.join();
            self.poll_thread = null;
        }
        _ = linux.close(self.poll_efd);
        // 6. The xattr.
        self.d.il0_bind(self.d.il0_ctx, null);
        hs.ipc_service_threads.set(0);
        hs.ipc_session_owners.set(0);
        // 7. The lease page (every direct session is gone: unshare, unmap).
        if (self.direct_host) |*dh| {
            dh.deinit();
            self.direct_host = null;
        }
    }

    pub fn deinit(self: *IpcHost) void {
        for (self.owners) |o| o.deinit();
        self.gpa.free(self.owners);
        self.sink.deinit();
        if (self.dd) |dd| {
            const fds = dd.vol_fds;
            dd.deinit();
            self.gpa.free(fds);
        }
        self.inval.deinit(self.gpa);
        self.placed.deinit();
        self.pool.deinit();
        self.gpa.free(self.owner_load);
        self.gpa.free(self.owner_node);
        self.uid_counts.deinit(self.gpa);
        self.gpa.free(self.sessions);
        self.gpa.destroy(self);
    }

    // ---- the il0 face ----

    fn synthesizeFn(ctx: *anyopaque, out: *[wire.BLOB_LEN]u8) void {
        const self: *IpcHost = @ptrCast(@alignCast(ctx));
        self.nonce_lock.lockUncancelable(self.io);
        defer self.nonce_lock.unlock(self.io);
        _ = self.nonces.rotateIfStale(&rl.fillNonce, clock.monotonicNs());
        const b = rendezvous.buildBlob(.{ .build_commit = self.d.our_commit, .socket_name = self.name_buf[0..self.name_len], .socket_path = if (self.path) |p| p.path() else "" }, &self.nonces.current) catch unreachable;
        out.* = b.bytes().*;
    }

    /// The blob as a client reads it (tests; the `il0` xattr is this).
    pub fn blob(self: *IpcHost) wire.Il0Blob {
        var raw: [wire.BLOB_LEN]u8 = undefined;
        synthesizeFn(self, &raw);
        return wire.Il0Blob.decode(&raw) catch unreachable;
    }

    // ---- the listeners and the HELLO ----

    fn acceptLoop(self: *IpcHost, listener: linux.fd_t, path_rung: bool) void {
        var name_buf: thread_name.CommBuf = undefined;
        _ = thread_name.nameCurrentThread(&name_buf, if (path_rung) "sqz-ipc-accp" else "sqz-ipc-acc");
        while (!self.stop.load(.acquire)) {
            const sock = rl.accept(listener) catch {
                if (self.stop.load(.acquire)) return;
                // EMFILE/ENOBUFS-class: a bounded breath, never a spin.
                sleepNs(std.time.ns_per_ms);
                continue;
            };
            if (self.handshakes.fetchAdd(1, .acq_rel) >= self.ctl_cap) {
                _ = self.handshakes.fetchSub(1, .acq_rel);
                _ = linux.close(sock);
                continue;
            }
            const t = std.Thread.spawn(.{}, helloThread, .{ self, sock }) catch {
                _ = self.handshakes.fetchSub(1, .acq_rel);
                _ = linux.close(sock);
                continue;
            };
            t.detach();
        }
    }

    /// One HELLO (module doc): recv → the ladder + the reservation under
    /// `reg_lock` → the memfd → SESSION_OK → register → the owner's thread.
    fn helloThread(self: *IpcHost, sock: linux.fd_t) void {
        defer _ = self.handshakes.fetchSub(1, .acq_rel);
        var name_buf: thread_name.CommBuf = undefined;
        _ = thread_name.nameCurrentThread(&name_buf, "sqz-ipc-hello");
        const t0 = clock.monotonicNs();
        rl.setRecvTimeout(sock, rendezvous.CTL_HANDSHAKE_TIMEOUT_NS);
        var buf: [wire.CTL_MSG_MAX]u8 = undefined;
        const r = rl.recvCtl(sock, &buf) catch {
            _ = linux.close(sock);
            return;
        };
        defer if (r.fd) |f| {
            _ = linux.close(f);
        };
        const h = wire.decodeFixed(wire.Hello, buf[0..r.len]) catch {
            _ = linux.close(sock);
            return;
        };
        const peer = rl.peerCred(sock);
        const facts: ?rendezvous.FdFacts = if (r.fd) |f| rl.fdFacts(f) else null;
        const nonces = blk: {
            self.nonce_lock.lockUncancelable(self.io);
            defer self.nonce_lock.unlock(self.io);
            _ = self.nonces.rotateIfStale(&rl.fillNonce, clock.monotonicNs());
            break :blk self.nonces;
        };
        // The ladder over LIVE facts and the reservation, under one hold.
        var owner: u16 = 0;
        const admission: rendezvous.Admission = blk: {
            self.reg_lock.lockUncancelable(self.io);
            defer self.reg_lock.unlock(self.io);
            const uid_sessions: u64 = if (peer) |p| (self.uid_counts.get(p.uid) orelse 0) else 0;
            const shed = self.shed_target.load(.monotonic);
            const policy: rendezvous.Policy = .{
                .armed = !self.stop.load(.acquire),
                .our_commit = self.d.our_commit,
                .allow_dirty = self.allow_dirty,
                .default_geometry = self.default_geometry,
                .arena_cap_bytes = self.arena_cap_bytes,
                .arena_in_use_bytes = self.in_use_bytes,
                .shed_target_bytes = if (shed != 0) shed else null,
                .per_uid_session_cap = self.per_uid_cap,
                .uid_sessions = uid_sessions,
            };
            switch (rendezvous.admitHello(policy, &h, facts, self.d.mount_dev, peer, &nonces)) {
                .refuse => |class| {
                    if (class == .budget and shed != 0 and self.in_use_bytes > shed) hs.ipc_shed_refusals.inc();
                    self.refuse(sock, class);
                    return;
                },
                .admit => |adm| {
                    if (self.live >= self.sessions.len) {
                        self.refuse(sock, .budget);
                        return;
                    }
                    owner = self.pickOwnerLocked();
                    self.owner_load[owner] += 1;
                    self.in_use_bytes += adm.footprint_bytes;
                    const gop = self.uid_counts.getOrPutAssumeCapacity(peer.?.uid);
                    if (!gop.found_existing) gop.value_ptr.* = 0;
                    gop.value_ptr.* += 1;
                    break :blk adm;
                },
            }
        };
        if (admission.dev_override) ipc.stats.ipc_binds_dev_override.inc();
        const l = layout.SessionLayout.compute(admission.geometry) catch unreachable;
        const created = shm.createSealed(l, self.thp) catch |e| {
            log.err("ipc: session memfd creation failed ({s}); refusing Internal", .{@errorName(e)});
            self.unreserve(owner, admission.footprint_bytes, peer.?.uid);
            self.refuse(sock, .internal);
            return;
        };
        const node = self.owner_node[owner];
        shm.bindArenaToNode(created.mapped, node);
        const s = Session.create(self.gpa, rl.Accepted{ .admission = admission, .peer = peer.?, .mapped = created.mapped, .layout = l }, sock, owner, node, t0) catch {
            _ = linux.close(created.fd);
            shm.unmap(created.mapped);
            self.unreserve(owner, admission.footprint_bytes, peer.?.uid);
            self.refuse(sock, .internal);
            return;
        };
        // The direct arm's screen: the peer could open every data device
        // node (`deviceReadable`), else the session is a ring session and
        // the refusal is counted and said once per peer.
        var features: u64 = 0;
        if (self.direct_host) |*dh| {
            if (dh.grants(peer.?)) {
                features |= wire.FEATURE_DIRECT;
                s.direct_granted = true;
                hs.ipc_direct_sessions.inc();
            } else {
                hs.ipc_direct_refused_access.inc();
                log.warn("ipc: the direct arm is refused to pid {d} (uid {d}, gid {d}): it cannot open every data device node; the ring serves it", .{ peer.?.pid, peer.?.uid, peer.?.gid });
            }
        }
        const ok: wire.SessionOk = .{ .features = features };
        const sent = rl.sendCtl(sock, wire.frameBytes(&ok), created.fd);
        _ = linux.close(created.fd);
        sent catch {
            self.unreserve(owner, admission.footprint_bytes, peer.?.uid);
            _ = linux.close(sock);
            _ = s.map.releaseRef();
            return;
        };
        // Register: the table, the epoch, the threads up to the owner.
        {
            self.reg_lock.lockUncancelable(self.io);
            defer self.reg_lock.unlock(self.io);
            for (self.sessions) |*slot| if (slot.* == null) {
                slot.* = s;
                break;
            };
            self.live += 1;
            _ = self.epoch.fetchAdd(1, .release);
            hs.ipc_sessions_active.set(self.live);
            hs.ipc_arena_bytes.set(self.in_use_bytes);
            hs.ipc_session_owners.set(self.ownersWithSessionsLocked());
            self.ensureThreadsLocked(owner);
        }
        ipc.stats.ipc_sessions_total.inc();
        ipc.stats.ipc_session_admission_ns.record(clock.monotonicNs() - t0);
        self.owners[owner].kick();
        self.wakePoller();
    }

    fn refuse(self: *IpcHost, sock: linux.fd_t, class: wire.RefuseClass) void {
        _ = self;
        ipc.countRefusal(class);
        const ref: wire.Refuse = .{ .class = @backingInt(class), .sdk_abi_min = if (class == .version) wire.sdk_abi_accepted[wire.sdk_abi_accepted.len - 1] else 0, .sdk_abi_max = if (class == .version) wire.sdk_abi_accepted[0] else 0 };
        rl.sendCtl(sock, wire.frameBytes(&ref), null) catch {};
        _ = linux.close(sock);
    }

    fn unreserve(self: *IpcHost, owner: u16, footprint: u64, uid: u32) void {
        self.reg_lock.lockUncancelable(self.io);
        defer self.reg_lock.unlock(self.io);
        self.owner_load[owner] -= 1;
        self.in_use_bytes -= footprint;
        if (self.uid_counts.getPtr(uid)) |c| c.* -= 1;
    }

    /// Balance-first `(load, distance, index)`; distance is 0 on every
    /// owner here (the peer's node is not inferred in this wave).
    fn pickOwnerLocked(self: *IpcHost) u16 {
        var cands: [64]layout.OwnerCandidate = undefined;
        for (self.owner_load, 0..) |l, i| cands[i] = .{ .load = l, .distance = 0 };
        return @intCast(layout.pickOwner(cands[0..self.owner_load.len]).?);
    }

    fn ownersWithSessionsLocked(self: *IpcHost) u64 {
        var n: u64 = 0;
        for (self.owner_load) |l| n += @intFromBool(l != 0);
        return n;
    }

    /// Spawn-on-admission, lowest owner index first, up to `owner`.
    fn ensureThreadsLocked(self: *IpcHost, owner: u16) void {
        while (self.spawned <= owner) {
            self.owners[self.spawned].spawn() catch |e| {
                log.err("ipc: cannot spawn sqz-ipc-svc{d} ({s})", .{ self.spawned, @errorName(e) });
                return;
            };
            self.spawned += 1;
            hs.ipc_service_threads.set(self.spawned);
        }
    }

    // ---- the registry face the service threads use ----

    fn registry(self: *IpcHost) service.Registry {
        return .{ .ctx = self, .epoch = &epochFn, .collect = &collectFn, .poison = &poisonFn };
    }

    fn epochFn(ctx: *anyopaque) u64 {
        const self: *IpcHost = @ptrCast(@alignCast(ctx));
        return self.epoch.load(.acquire);
    }

    fn collectFn(ctx: *anyopaque, owner: u16, out: []*Session) usize {
        const self: *IpcHost = @ptrCast(@alignCast(ctx));
        self.reg_lock.lockUncancelable(self.io);
        defer self.reg_lock.unlock(self.io);
        var n: usize = 0;
        for (self.sessions) |maybe| if (maybe) |s| {
            if (s.owner != owner or n == out.len) continue;
            s.map.retain();
            out[n] = s;
            n += 1;
        };
        return n;
    }

    /// A protocol violation on the service thread: the generation bump
    /// now (every client op falls through), the teardown on the poller.
    fn poisonFn(ctx: *anyopaque, s: *Session, reason: session.PoisonReason) void {
        const self: *IpcHost = @ptrCast(@alignCast(ctx));
        s.poison(reason);
        log.err("ipc: session (pid {d}, owner {d}) poisoned: {s}", .{ s.peer.pid, s.owner, @tagName(reason) });
        self.wakePoller();
    }

    // ---- the ctl poller ----

    fn wakePoller(self: *IpcHost) void {
        const one: u64 = 1;
        _ = linux.write(self.poll_efd, std.mem.asBytes(&one), 8);
    }

    fn pollLoop(self: *IpcHost) void {
        var name_buf: thread_name.CommBuf = undefined;
        _ = thread_name.nameCurrentThread(&name_buf, "sqz-ipc-ctl");
        const fds = self.gpa.alloc(linux.pollfd, self.sessions.len + 1) catch return;
        defer self.gpa.free(fds);
        const owners = self.gpa.alloc(*Session, self.sessions.len) catch return;
        defer self.gpa.free(owners);
        const tick_ms: i32 = @intCast(reapTickNs(rendezvous.IDLE_REAP_NS) / std.time.ns_per_ms);
        while (!self.stop.load(.acquire)) {
            fds[0] = .{ .fd = self.poll_efd, .events = linux.POLL.IN, .revents = 0 };
            var n: usize = 1;
            {
                self.reg_lock.lockUncancelable(self.io);
                defer self.reg_lock.unlock(self.io);
                for (self.sessions) |maybe| if (maybe) |s| {
                    if (s.torn_down.load(.acquire)) continue;
                    s.map.retain();
                    owners[n - 1] = s;
                    fds[n] = .{ .fd = s.ctl, .events = linux.POLL.IN, .revents = 0 };
                    n += 1;
                };
            }
            const rc = linux.poll(fds.ptr, @intCast(n), tick_ms);
            const ready: usize = if (linux.errno(rc) == .SUCCESS) rc else 0;
            if (fds[0].revents != 0) {
                var drain: u64 = 0;
                _ = linux.read(self.poll_efd, std.mem.asBytes(&drain), 8);
            }
            var i: usize = 1;
            while (i < n) : (i += 1) {
                const s = owners[i - 1];
                if (fds[i].revents != 0 and !s.torn_down.load(.acquire)) self.handleCtl(s, fds[i].revents);
                if (s.poisoned.load(.acquire)) self.teardown(s, "poisoned");
                _ = s.map.releaseRef();
            }
            if (ready == 0) self.reapIdle();
        }
    }

    /// One readable ctl socket: BIND, UNBIND, EOF (the client died), or a
    /// malformed frame (poison).
    fn handleCtl(self: *IpcHost, s: *Session, revents: i16) void {
        var buf: [wire.CTL_MSG_MAX]u8 = undefined;
        const r = rl.recvCtl(s.ctl, &buf) catch |e| {
            switch (e) {
                error.Eof => {
                    hs.ipc_sessions_closed.inc();
                    self.teardown(s, "ctl socket EOF");
                },
                else => {
                    s.poison(.ctl_malformed);
                    self.teardown(s, "malformed ctl frame");
                },
            }
            return;
        };
        if (revents & (linux.POLL.HUP | linux.POLL.ERR) != 0 and r.len == 0) {
            if (r.fd) |f| _ = linux.close(f);
            hs.ipc_sessions_closed.inc();
            self.teardown(s, "ctl socket hangup");
            return;
        }
        defer if (r.fd) |f| {
            _ = linux.close(f);
        };
        const frame = buf[0..r.len];
        switch (wire.tagOf(frame) catch return self.malformed(s)) {
            .bind => {
                const req = wire.decodeFixed(wire.Bind, frame) catch return self.malformed(s);
                const f = r.fd orelse return self.malformed(s);
                switch (rendezvous.screenFd(rl.fdFacts(f), self.d.mount_dev, false)) {
                    .ok => |ok| {
                        const durability: wire.Durability = if (req.durability == @backingInt(wire.Durability.sync)) .sync else .mount;
                        const id = s.bind(ok, killPrivClass(s.peer.uid), durability) orelse {
                            ipc.countRefusal(.budget);
                            const reply: wire.BindRefused = .{ .class = @backingInt(wire.RefuseClass.budget) };
                            rl.sendCtl(s.ctl, wire.frameBytes(&reply), null) catch {};
                            return;
                        };
                        s.touch(clock.monotonicNs());
                        const reply: wire.BindOk = .{ .rights = ok.rights, .binding_id = id, .ino = ok.ino, .durability_granted = @backingInt(durability) };
                        rl.sendCtl(s.ctl, wire.frameBytes(&reply), null) catch {};
                        self.inval.onBind(ok.ino, self.sizeOf(ok.ino), clock.realtimeCoarseS());
                    },
                    .refuse => |class| {
                        ipc.countRefusal(class);
                        const reply: wire.BindRefused = .{ .class = @backingInt(class) };
                        rl.sendCtl(s.ctl, wire.frameBytes(&reply), null) catch {};
                    },
                }
            },
            .unbind => {
                const req = wire.decodeFixed(wire.Unbind, frame) catch return self.malformed(s);
                if (s.unbind(req.binding_id)) |ino| {
                    if (!self.inoBoundAnywhere(ino)) self.inval.onLastUnbind(ino, clock.realtimeCoarseS());
                }
            },
            .direct_hello => {
                _ = wire.decodeFixed(wire.DirectHello, frame) catch return self.malformed(s);
                const dh = if (s.direct_granted) self.direct_host else null;
                if (dh) |h| {
                    const reply = h.directOk();
                    rl.sendCtl(s.ctl, wire.frameBytes(&reply), h.memfd) catch {};
                } else {
                    direct_lease.countRefusal(.disabled);
                    const reply: wire.LeaseRefused = .{ .class = @backingInt(wire.LeaseRefuseClass.disabled) };
                    rl.sendCtl(s.ctl, wire.frameBytes(&reply), null) catch {};
                }
            },
            .lease_req => {
                const req = wire.decodeFixed(wire.LeaseReq, frame) catch return self.malformed(s);
                var out: [wire.CTL_MSG_MAX]u8 = undefined;
                const verdict: direct_lease.LeaseVerdict = blk: {
                    if (!s.direct_granted or self.direct_host == null) break :blk .{ .refused = .disabled };
                    const b = s.binding(req.binding_id) orelse break :blk .{ .refused = .binding };
                    if (!b.rights.read) break :blk .{ .refused = .binding };
                    break :blk direct_lease.buildLease(self.d.plane, b.ino, &out);
                };
                switch (verdict) {
                    .ok => |n| rl.sendCtl(s.ctl, out[0..n], null) catch {},
                    .refused => |class| {
                        direct_lease.countRefusal(class);
                        const reply: wire.LeaseRefused = .{ .class = @backingInt(class) };
                        rl.sendCtl(s.ctl, wire.frameBytes(&reply), null) catch {};
                    },
                }
                s.touch(clock.monotonicNs());
            },
            else => self.malformed(s),
        }
    }

    fn malformed(self: *IpcHost, s: *Session) void {
        s.poison(.ctl_malformed);
        self.teardown(s, "unexpected ctl frame");
    }

    /// The announced size of a bound nodeid (the plane's RAM view); 0 when
    /// unknown — the first write then reads as growth (attrs-only).
    fn sizeOf(self: *IpcHost, nodeid: u64) u64 {
        if (!route_table.isRoutable(nodeid)) return 0;
        const rp = self.d.plane.rp;
        const info = rp.meta.file(rp.meta.ctx, route_table.effectiveIno(nodeid)) orelse return 0;
        return info.size;
    }

    fn inoBoundAnywhere(self: *IpcHost, ino: u64) bool {
        self.reg_lock.lockUncancelable(self.io);
        defer self.reg_lock.unlock(self.io);
        for (self.sessions) |maybe| if (maybe) |s| {
            if (!s.torn_down.load(.acquire) and s.bindsIno(ino)) return true;
        };
        return false;
    }

    /// Sessions idle ≥ the window: a silent generation bump, the teardown
    /// (counted `ipc_sessions_reaped`, never the poison tripwire).
    fn reapIdle(self: *IpcHost) void {
        const now = clock.monotonicNs();
        while (self.takeIdle(now, rendezvous.IDLE_REAP_NS)) |s| {
            s.poison(.idle_reap);
            hs.ipc_sessions_reaped.inc();
            self.teardown(s, "idle");
            _ = s.map.releaseRef();
        }
    }

    fn takeIdle(self: *IpcHost, now: u64, idle_ns: u64) ?*Session {
        self.reg_lock.lockUncancelable(self.io);
        defer self.reg_lock.unlock(self.io);
        for (self.sessions) |maybe| if (maybe) |s| {
            if (!s.torn_down.load(.acquire) and now -| s.last_active_ns.load(.monotonic) >= idle_ns) {
                s.map.retain();
                return s;
            }
        };
        return null;
    }

    fn takeAny(self: *IpcHost) ?*Session {
        self.reg_lock.lockUncancelable(self.io);
        defer self.reg_lock.unlock(self.io);
        for (self.sessions) |maybe| if (maybe) |s| {
            if (!s.torn_down.load(.acquire)) {
                s.map.retain();
                return s;
            }
        };
        return null;
    }

    /// The R5 shed hook: refuse NEW sessions while the gauge is above the
    /// target (one-shot) and tear down sessions with NO live binding —
    /// a bound session is never torn by a shed (its arena is in use).
    fn shedFn(ctx: *anyopaque, target: u64) void {
        const self: *IpcHost = @ptrCast(@alignCast(ctx));
        self.shed_target.store(@max(target, 1), .monotonic);
        while (self.takeUnbound()) |s| {
            s.poison(.shed);
            hs.ipc_sessions_reaped.inc();
            self.teardown(s, "shed (unbound)");
            _ = s.map.releaseRef();
            if (hs.ipc_arena_bytes.load() <= target) break;
        }
    }

    fn takeUnbound(self: *IpcHost) ?*Session {
        self.reg_lock.lockUncancelable(self.io);
        defer self.reg_lock.unlock(self.io);
        for (self.sessions) |maybe| if (maybe) |s| {
            if (s.torn_down.load(.acquire)) continue;
            const n: usize = @intCast(@min(s.next_binding.load(.monotonic) - 1, s.bindings.len));
            var bound = false;
            for (s.bindings[0..n]) |*b| if (b.live.load(.acquire)) {
                bound = true;
            };
            if (!bound) {
                s.map.retain();
                return s;
            }
        };
        return null;
    }

    /// Exactly once (module doc). The caller holds a share of `s` it
    /// releases itself; the registry's share drops here after the join.
    fn teardown(self: *IpcHost, s: *Session, reason: []const u8) void {
        if (s.torn_down.swap(true, .acq_rel)) return;
        {
            self.reg_lock.lockUncancelable(self.io);
            defer self.reg_lock.unlock(self.io);
            for (self.sessions) |*slot| if (slot.* == s) {
                slot.* = null;
            };
            self.live -= 1;
            self.in_use_bytes -= s.footprint_bytes;
            if (self.uid_counts.getPtr(s.peer.uid)) |c| c.* -|= 1;
            self.owner_load[s.owner] -|= 1;
            _ = self.epoch.fetchAdd(1, .release);
            hs.ipc_sessions_active.set(self.live);
            hs.ipc_arena_bytes.set(self.in_use_bytes);
            hs.ipc_session_owners.set(self.ownersWithSessionsLocked());
            // The client's display-only page folds into the host's ledger.
            const page = s.m.statsPage();
            hs.ipc_lane_gate_kernel_routes.add(page.lane_gate_kernel_routes.load(.monotonic));
            hs.ipc_lane_gate_kernel_bytes.add(page.lane_gate_kernel_bytes.load(.monotonic));
            hs.ipc_client_aio_unknown_ctx_ops.add(page.preload_passthrough_ops[layout.PRELOAD_REASON_AIO_UNKNOWN_CTX_OP].load(.monotonic));
            hs.ipc_direct_ops.add(page.il_direct_ops.load(.monotonic));
            hs.ipc_direct_bytes.add(page.il_direct_bytes.load(.monotonic));
            hs.ipc_direct_rechecks_failed.add(page.il_direct_rechecks_failed.load(.monotonic));
        }
        self.owners[s.owner].kick();
        _ = linux.shutdown(s.ctl, linux.SHUT.RDWR);
        _ = linux.close(s.ctl);
        // The handoff fibers end on their own; the mapping outlives them by
        // their shares. Join, then drop the registry's share.
        s.group.await(self.io) catch {};
        log.info("ipc: session (pid {d}, owner {d}) torn down: {s}", .{ s.peer.pid, s.owner, reason });
        _ = s.map.releaseRef();
    }

    pub fn liveSessions(self: *IpcHost) u32 {
        self.reg_lock.lockUncancelable(self.io);
        defer self.reg_lock.unlock(self.io);
        return self.live;
    }
};

test {
    _ = stats;
    _ = severed_pool;
    _ = placed;
    _ = placed_claims_model;
    _ = inval;
    _ = session;
    _ = service;
    _ = direct;
    _ = e2e_linux;
}

const testing = std.testing;

test "the reap tick is a quarter of the idle window railed 100 ms..5 s; the dd shard width is the ONE width function; the kill-priv class is the unprivileged uid" {
    try testing.expectEqual(@as(u64, 5 * std.time.ns_per_s), reapTickNs(rendezvous.IDLE_REAP_NS));
    try testing.expectEqual(@as(u64, 100 * std.time.ns_per_ms), reapTickNs(10 * std.time.ns_per_ms));
    try testing.expectEqual(@as(u64, 500 * std.time.ns_per_ms), reapTickNs(2 * std.time.ns_per_s));
    var c: u32 = 1;
    while (c <= 4096) : (c += 1) try testing.expectEqual(sizing.ilDrainLanesDefault(c), resolveDdShards(null, c));
    try testing.expectEqual(@as(u32, 3), resolveDdShards(3, 256));
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    try testing.expectEqual(@as(u32, 12), ddShardsFromEnv(&env, 32));
    try env.put("SQUEEZEFS_IPC_DD_SHARDS", "4");
    try testing.expectEqual(@as(u32, 4), ddShardsFromEnv(&env, 32));
    try testing.expect(killPrivClass(1000));
    try testing.expect(!killPrivClass(0));
}
