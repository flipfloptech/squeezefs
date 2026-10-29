//! The metadata SET facade (Linux; charter §5.4; `docs/porting/
//! dynamic_routing.md` §2, §8 "set_backend"): N `KvBackend`s — one per
//! member volume, each with its own D0 gate, journal lane and checkpoint
//! task — behind ONE store face the filesystem (`fuse/handler/kv_fs.zig`)
//! and the data plane bind to. Discovery (`routing/discovery.zig`) orders
//! the members canonically and builds the slot → volume table; every op
//! routes by the ONE key law (`route_table.slotOfKey`: the key's slot →
//! its host → the host's tree for the slot), mints spread over the healthy
//! volumes' rotors (`router.pickMintVolume`, `MintRotor`) and, on an armed
//! volume, the slot plane's mint decision (parent affinity under the
//! ceiling, else the rotor tree with the most headroom); the commit DOOR
//! (`sym/slots/door.zig`, the host's conveyor step 0) parks a commit whose
//! slot is mid-flip or mid-handover and refuses one another appender leases.
//!
//! A transaction's records live on ONE member: an op whose objects span
//! two members (or two holders) is a cross-owner INTENT (charter §5.5;
//! `sym/xv/ops.zig`) — step 0 + the intent record in the initiator's ring,
//! one single-member entry per step, the retirement — never a split commit
//! here (the rung-5 two-entry split is retired: decision 0003 §E). A tx
//! that reaches the facade spanning two members is a committer defect:
//! refused `EIO`, counted on the tripwire, nothing written.
//!
//! The one-member set is the one-volume mount: the same facade, one
//! backend, slot 0 native and 63 rotor slots as guest trees on it.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;
const Io = std.Io;

const counters = @import("../core/counters.zig");
const errors = @import("../core/errors.zig");
const log = @import("../core/log.zig");
const derive = @import("../core/derive.zig");
const clock = @import("../core/clock.zig");
const knobs = @import("../core/knobs.zig");
const uring_fs = @import("../io/uring_fs.zig");
const key = @import("key.zig");
const tree = @import("tree.zig");
const node_cache = @import("node_cache.zig");
const merged_walk = @import("merged_walk.zig");
const tx = @import("tx.zig");
const ledger = @import("ledger.zig");
const journal = @import("journal.zig");
const inos = @import("inos.zig");
const conveyor = @import("conveyor.zig");
const checkpoint_cycle = @import("checkpoint_cycle.zig");
const backend = @import("backend.zig");
const journal_lane = @import("journal_lane.zig");
const fsync_marks = @import("fsync_marks.zig");
const guard = @import("guard.zig");
const routing = @import("routing.zig");
const routing_slots = @import("sym.zig").slots;
const SlotPlane = @import("sym/slots/plane.zig").Plane;
const xv = @import("sym.zig").xv;
const dlm = @import("dlm.zig");
const router = @import("../data/router.zig");

const route_table = routing.route_table;
const discovery = routing.discovery;
const dir_place = routing.dir_place;
const rstats = routing.stats.stats;

comptime {
    if (builtin.os.tag != .linux) @compileError("meta_set is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const KvBackend = backend.KvBackend;
pub const KvTx = tx.KvTx;
pub const Error = backend.Error;
pub const Outcome = conveyor.Outcome;

/// `backend.OpenError` plus the set's own refusal (discovery, logged by
/// name before this is returned).
pub const OpenError = backend.OpenError || error{SetRefused};

pub fn openCoreError(e: OpenError) errors.Error {
    return switch (e) {
        error.SetRefused => error.UnsupportedVolume,
        else => |be_err| backend.openCoreError(be_err),
    };
}

pub fn openErrno(e: OpenError) linux.E {
    return errors.toErrno(openCoreError(e));
}

pub const OpenOptions = struct {
    env: *const std.process.Environ.Map,
    /// The process's io pool; one is created (and owned) when null.
    pool: ?*uring_fs.Pool = null,
    /// The node-cache budget PER VOLUME (charter §5.3).
    cache_budget_bytes: u64 = @import("node_cache.zig").DEFAULT_CACHE_BUDGET_BYTES,
    cpus: u32 = 0,
    test_error_logs: if (builtin.is_test) usize else void = if (builtin.is_test) 0 else {},
    guard: guard.gate.Options = .{},
    /// A `-o ro` open only RESOLVES dual claims (never converges them).
    read_only: bool = false,
    /// The join door's inputs (the mount point, an explicit manager
    /// endpoint); null = a D0 loser is refused `Busy` (the in-process
    /// harnesses, which never join).
    join: ?backend.JoinOptions = null,
};

/// A member's mints by kind (`MetaSet.mints`).
pub const VolumeMints = struct {
    files: counters.ShardedCounter = .{},
    dirs: counters.ShardedCounter = .{},
};

/// `SQUEEZEFS_META_FSYNC_SCOPE` (decision 0003 §CH): `ino` — an fsync of
/// one inode asks the members its un-durable entries lie on, in parallel;
/// `set` — every member, one after another (the control arm).
pub const FsyncScope = enum { ino, set };

/// A member's share of the set's durability points (`.stats` `fsync`
/// under each member; `fsync_marks.stats` the set's sums).
pub const MemberFsync = struct {
    requests: counters.ShardedCounter = .{},
    requests_own: counters.ShardedCounter = .{},
    requests_chain: counters.ShardedCounter = .{},
    requests_none: counters.ShardedCounter = .{},
    skips: counters.ShardedCounter = .{},
};

/// Members asked at once by one durability point: their waiters live on
/// the caller's frame (a set wider than this asks in rounds).
const FSYNC_PARALLEL = 8;

pub const MetaSet = struct {
    gpa: Allocator,
    io: Io,
    env: *const std.process.Environ.Map,
    /// Canonical order (member position ascending).
    volumes: []*KvBackend,
    pool: *uring_fs.Pool,
    pool_owned: bool,
    route: route_table.Published,
    /// Per volume: the mint set (its first ≤ 64 hosted slots) and rotor.
    rotors: []router.MintRotor,
    /// Per volume: the mints by kind (`.stats` `mint_files` / `mint_dirs`
    /// under each member — a phase's share of the namespace per member).
    mints: []VolumeMints,
    mint_rr: std.atomic.Value(u64) = .init(0),
    /// Per volume: the durability requests (`MemberFsync`).
    fsync_counts: []MemberFsync,
    fsync_scope: FsyncScope = .ino,
    /// The set's directory spread depth `D` (`routing/dir_place.zig`;
    /// `set_config.dir_spread_depth`, read by the mount after the record
    /// and stored here before the FUSE session arms; 0 until then and on
    /// every harness that sets none = §AA.1's placement).
    dir_spread_depth: std.atomic.Value(u8) = .init(0),
    /// The slot decision's tie-break cursor (apart from the member pick's).
    slot_rr: std.atomic.Value(u64) = .init(0),
    set_uuid: [16]u8,
    hash_seed: u64,
    /// `hash_seed` expanded once at mount: the set's name key.
    name_key: key.NameKey,
    node_size: u32,
    /// One slot migration at a time per set (the coordinator's
    /// one-flip-per-slot law, made structural).
    migration_lock: Io.Mutex = .init,
    /// The delta tee (created at the first migration, reused after: a
    /// window that loaded the hook pointer may still be noting keys after
    /// a disarm, so the tee lives as long as the set).
    tee: ?*routing.migration_tee.MigrationTee = null,
    tee_hook: conveyor.CommitTee = undefined,
    /// Every member's posture (one per set: the manager of them all, or
    /// joined to their manager).
    posture: backend.Posture = .manager,
    closed: bool = false,
    /// The data path's failure lines (`lineSeam`): one per class once any
    /// member is FAILED.
    line_marks: journal.FailureLatch.LineMarks = .{},
    /// THE RE-JOIN IN PLACE's inputs (charter §5.5 "A live member evicted
    /// …"; decision 0003 §BS): the canonical member paths, the backend
    /// options and the join door this open used — a successor incarnation
    /// opens through the same door. Empty on every posture but a joiner.
    rejoin_paths: [][]u8 = &.{},
    rejoin_opts: backend.OpenOptions = undefined,
    rejoin_join: backend.JoinOptions = .{ .mountpoint = "" },
    /// Incarnations a re-join in place retired: closed after the live
    /// members (a reader that loaded one before the swap reads it still).
    retired: std.ArrayList(*KvBackend) = .empty,
    /// Re-joins in place this mount made (`.stats` `meta_rejoins_in_place`).
    rejoins: u64 = 0,
    /// `retired[0..retired_released]` have released their node caches.
    retired_released: usize = 0,
    /// The bindings every member's planes carry, re-applied to a
    /// successor incarnation: the move epoch, the parent memo, the times
    /// drain, the kernel view and the token recall on its conveyor, the
    /// supervisor's tail hand-off.
    bound_moves: ?*routing_slots.moves.Moves = null,
    bound_memo: ?routing_slots.handover.ParentMemo = null,
    bound_times_drain: ?routing_slots.handover.TimesDrain = null,
    bound_kernel: ?*const conveyor.KernelView = null,
    bound_tokens: ?*const conveyor.TokenHook = null,

    // ---- open / close ----

    /// Discover the set named by `paths` (any order), open every member
    /// in canonical order (each through its D0 ladder; a failure releases
    /// every prior guard), converge the dual claims a torn flip left,
    /// publish the table.
    pub fn open(gpa: Allocator, io: Io, paths: []const []const u8, opts: OpenOptions) OpenError!*MetaSet {
        if (paths.len == 0 or paths.len > route_table.MAX_VOLUMES) return error.SetRefused;
        const pool = opts.pool orelse (uring_fs.Pool.init(gpa, uring_fs.Config.resolve(opts.env, derive.Facts.probe())) catch return error.DeviceRefused);
        errdefer if (opts.pool == null) pool.deinit();

        // Discovery, no mount: every member's superblock + newest record.
        const owned = try gpa.alloc(discovery.linux_dev.Owned, paths.len);
        defer gpa.free(owned);
        var observed: usize = 0;
        defer for (owned[0..observed]) |*o| o.deinit(gpa);
        const obs = try gpa.alloc(discovery.Observation, paths.len);
        defer gpa.free(obs);
        for (paths, 0..) |p, i| {
            owned[i] = discovery.linux_dev.observe(gpa, opts.env, pool, p) catch |e| return switch (e) {
                error.NotFormatted => {
                    log.err("Metadata volume {s} is not formatted (zeroed superblock) — run `squeezefs format` first", .{p});
                    return error.NotFormatted;
                },
                error.LegacyFormat => {
                    log.err("Metadata volume {s} is a pre-v5 squeezefs format — reformat required", .{p});
                    return error.LegacyFormat;
                },
                error.DeviceRefused => error.DeviceRefused,
                error.Corrupt => error.Corrupt,
                error.Io => error.Io,
                error.OutOfMemory => error.OutOfMemory,
            };
            observed += 1;
            obs[i] = owned[i].obs;
        }
        var ddiag: discovery.Refusal = undefined;
        var disc = discovery.discover(gpa, obs, &ddiag) catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            error.Refused => {
                var buf: [2048]u8 = undefined;
                var w: Io.Writer = .fixed(&buf);
                ddiag.write(&w) catch {};
                log.err("metadata set discovery refused: {s}", .{w.buffered()});
                return error.SetRefused;
            },
        };
        defer disc.deinit();

        const self = try gpa.create(MetaSet);
        errdefer gpa.destroy(self);
        const volumes = try gpa.alloc(*KvBackend, paths.len);
        errdefer gpa.free(volumes);
        var opened: usize = 0;
        errdefer for (volumes[0..opened]) |be| be.close();
        // The posture (charter §5.7): volume 0's D0 ladder decides. Its
        // winner is the manager of every volume it opens; a loser whose
        // volume carries the symmetric plane JOINS the live manager — of
        // every volume, through the same door (a set is never half-joined).
        // A `-o ro` mount of a symmetric set is the READER of every volume
        // (charter §5.5: a member-reader + token client, or the quiescent
        // reader of the projection — never a writer claim); of a
        // `--single-writer` set it is the writer door with the FUSE layer
        // read-only (the single-writer class has no token plane).
        var posture: backend.Posture = if (opts.read_only and owned[disc.order[0]].symmetric) .reader else .manager;
        for (disc.order, 0..) |oi, vi| {
            // The join door stands at volume 0 of a symmetric set opened
            // with a join posture: its held flock is a normal join, not a
            // refusal, and the open logs it so.
            const join_door = vi == 0 and !opts.read_only and owned[oi].symmetric and opts.join != null;
            const be_opts: backend.OpenOptions = .{
                .env = opts.env,
                .pool = pool,
                .index = @intCast(vi),
                .cache_budget_bytes = opts.cache_budget_bytes,
                .cpus = opts.cpus,
                .test_error_logs = opts.test_error_logs,
                .guard = opts.guard,
                .join_door = join_door,
            };
            volumes[vi] = switch (posture) {
                .manager => KvBackend.open(gpa, io, paths[oi], be_opts) catch |e| blk: {
                    if (e != error.Busy or !join_door) return e;
                    // The D0 loser of a symmetric set: the join door.
                    posture = .joined;
                    rstats.routing_joined_opens.inc();
                    log.info("metadata set: volume 0's writer lock is held by a live manager — joining the set as an equal writer over the cluster wire", .{});
                    break :blk try KvBackend.openJoined(gpa, io, paths[oi], be_opts, opts.join.?);
                },
                .joined => jblk: {
                    // A member past volume 0 joins with the set's secret.
                    var j = opts.join.?;
                    j.secret = volumes[0].joined.?.secret;
                    break :jblk try KvBackend.openJoined(gpa, io, paths[oi], be_opts, j);
                },
                .reader => try KvBackend.openReader(gpa, io, paths[oi], be_opts),
            };
            opened += 1;
        }
        const rotors = try gpa.alloc(router.MintRotor, paths.len);
        errdefer gpa.free(rotors);
        const mints = try gpa.alloc(VolumeMints, paths.len);
        errdefer gpa.free(mints);
        @memset(mints, .{});
        const fsync_counts = try gpa.alloc(MemberFsync, paths.len);
        errdefer gpa.free(fsync_counts);
        @memset(fsync_counts, .{});
        const initial = disc.table();
        for (rotors, 0..) |*r, v| {
            var buf: [router.MINT_SPREAD]u16 = undefined;
            r.* = router.MintRotor.init(initial.hostedAscending(@intCast(v), &buf));
        }
        self.* = .{
            .gpa = gpa,
            .io = io,
            .env = opts.env,
            .volumes = volumes,
            .pool = pool,
            .pool_owned = opts.pool == null,
            .route = try route_table.Published.init(gpa, initial),
            .rotors = rotors,
            .mints = mints,
            .fsync_counts = fsync_counts,
            .fsync_scope = std.meta.stringToEnum(FsyncScope, knobs.enumKnob("SQUEEZEFS_META_FSYNC_SCOPE", opts.env)).?,
            .set_uuid = disc.set_uuid,
            .hash_seed = disc.hash_seed,
            .name_key = .init(disc.hash_seed),
            .node_size = disc.node_size,
            .posture = posture,
        };
        errdefer self.route.deinit();
        // A joiner converges nothing: the stamps are the manager's.
        if (!opts.read_only and posture == .manager) try self.convergeDualClaims(disc.dual_claims);
        if (posture == .joined) try self.keepRejoinInputs(paths, disc.order, opts);
        log.info("metadata set {x:0>2}{x:0>2}{x:0>2}{x:0>2}…: {d} member(s) mounted in canonical order as {s}, epoch {d}, {d} dual claim(s) converged, native keyspace on member {d}", .{ disc.set_uuid[0], disc.set_uuid[1], disc.set_uuid[2], disc.set_uuid[3], paths.len, @tagName(posture), disc.set_epoch, if (posture == .manager) disc.dual_claims.len else 0, disc.native_volume });
        return self;
    }

    /// §7 Q1: a slot two stamps claim (a flip crashed after write 1) is
    /// hosted by the higher epoch; the loser's stale claim is released NOW
    /// — the idempotent redo of flip write 2 at the winner's epoch (stamp
    /// `−s`, cursor dropped, a checkpoint) and the loser's tree dropped —
    /// so no later, unrelated flip can resurrect it.
    fn convergeDualClaims(self: *MetaSet, duals: []const discovery.DualClaim) OpenError!void {
        for (duals) |d| {
            const loser = self.volumes[d.loser];
            loser.stamp.releaseSlot(d.slot) catch return error.OutOfMemory;
            loser.stamp.removeCursor(d.slot);
            loser.stamp.setEpoch(@max(loser.stamp.set_epoch, d.winner_epoch));
            loser.dropTree(d.slot) catch |e| return mapOpenFailure(e);
            loser.checkpointNow() catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.VolumeFailed, error.Io => error.Io,
                error.Corrupt => error.Corrupt,
            };
            rstats.routing_dual_claims_converged.inc();
            log.warn("metadata set: slot {d} was claimed by member {d} (epoch {d}) and member {d} (epoch {d}) — a flip torn after its claim write; the stale claim on member {d} is released and its copy dropped (routing_dual_claims_converged)", .{ d.slot, d.winner, d.winner_epoch, d.loser, d.loser_epoch, d.loser });
        }
    }

    fn mapOpenFailure(e: Error) OpenError {
        return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.VolumeFailed, error.Io, error.JournalWriteFailed => error.Io,
            else => error.Corrupt,
        };
    }

    /// Close every member in canonical order (each a clean dismount).
    pub fn close(self: *MetaSet) void {
        _ = self.closeJudged();
    }

    /// `close`, answering whether every member's dismount was clean
    /// (`KvBackend.closeJudged`).
    pub fn closeJudged(self: *MetaSet) bool {
        const gpa = self.gpa;
        var clean = true;
        if (!self.closed) {
            self.closed = true;
            for (self.volumes) |be| clean = be.closeJudged() and clean;
            for (self.retired.items) |be| be.close();
        }
        self.retired.deinit(gpa);
        for (self.rejoin_paths) |p| gpa.free(p);
        if (self.rejoin_paths.len != 0) {
            gpa.free(self.rejoin_paths);
            gpa.free(self.rejoin_join.mountpoint);
        }
        if (self.tee) |t| t.deinit();
        self.route.deinit();
        gpa.free(self.rotors);
        gpa.free(self.mints);
        gpa.free(self.fsync_counts);
        gpa.free(self.volumes);
        if (self.pool_owned) self.pool.deinit();
        gpa.destroy(self);
        return clean;
    }

    pub fn volumeCount(self: *const MetaSet) usize {
        return self.volumes.len;
    }

    /// A `-o ro` reader of a symmetric set: nothing here is ever written.
    pub fn isReader(self: *const MetaSet) bool {
        return self.posture == .reader;
    }

    // ---- routing ----

    /// The key ino of a global ino (`route_table.effectiveIno`: a pure law).
    pub fn effectiveIno(_: *const MetaSet, global: u64) u64 {
        return route_table.effectiveIno(global);
    }

    pub fn globalIno(_: *const MetaSet, eff: u64) u64 {
        return route_table.globalOfEffective(eff);
    }

    /// The member hosting routing slot `slot` now.
    pub fn volumeOfSlot(self: *MetaSet, slot: u16) *KvBackend {
        return self.volumes[self.route.volumeOf(slot)];
    }

    pub fn volumeIndexOfSlot(self: *MetaSet, slot: u16) route_table.VolumeIndex {
        return self.route.volumeOf(slot);
    }

    /// The member and routing slot a slot-tree key lives on.
    fn homeOfKey(self: *MetaSet, kind: u8, k: []const u8) Error!struct { be: *KvBackend, slot: u16 } {
        if (kind == key.TREE_CONTROL) return .{ .be = self.volumes[0], .slot = 0 };
        const slot = route_table.slotOfKey(k) catch return error.Corrupt;
        return .{ .be = self.volumeOfSlot(slot), .slot = slot };
    }

    /// The tree a key lives in, null when its slot has no tree yet (a
    /// virgin slot: every read of it is absent).
    fn treeOfKey(self: *MetaSet, kind: u8, k: []const u8) Error!?*tree.KvTree {
        const h = try self.homeOfKey(kind, k);
        if (kind == key.TREE_CONTROL) return h.be.treeOf(.control) orelse error.Corrupt;
        return h.be.slotTree(h.slot);
    }

    // ---- the store face ----

    pub const StoreError = Error;

    pub fn coreError(e: Error) errors.Error {
        return backend.KvBackend.coreError(e);
    }

    pub fn begin(self: *MetaSet, site: tx.Site) Allocator.Error!*KvTx {
        const t = try self.gpa.create(KvTx);
        t.* = KvTx.init(self.gpa, site);
        return t;
    }

    /// `begin` with the arena carved from the committer's slab-first
    /// allocator (`KvTx.initBacked`): the slab returns through `backing`
    /// at the tx's terminal outcome.
    pub fn beginBacked(self: *MetaSet, site: tx.Site, child: Allocator, backing: tx.ArenaBacking) Allocator.Error!*KvTx {
        const t = try self.gpa.create(KvTx);
        t.* = KvTx.initBacked(child, site, backing);
        return t;
    }

    pub fn abandon(self: *MetaSet, t: *KvTx) void {
        t.deinit();
        self.gpa.destroy(t);
    }

    /// Point lookup through the key's home tree (latch-free); an owned copy.
    pub fn lookup(self: *MetaSet, gpa: Allocator, kind: u8, k: []const u8) Error!?[]u8 {
        var cost: tree.WalkCost = .{};
        return self.lookupCounted(gpa, kind, k, &cost);
    }

    /// `lookup`, adding its descent to `cost` (fsck's walk accounting).
    pub fn lookupCounted(self: *MetaSet, gpa: Allocator, kind: u8, k: []const u8, cost: *tree.WalkCost) Error!?[]u8 {
        const t = (try self.treeOfKey(kind, k)) orelse return null;
        cost.sources += 1;
        return t.lookupCounted(gpa, k, cost) catch |e| KvBackend.readError(e);
    }

    /// `lookup` over the borrowed value (`KvTree.lookupView`): no copy.
    pub fn lookupView(self: *MetaSet, gpa: Allocator, kind: u8, k: []const u8, comptime T: type, ctx: anytype, comptime visit: fn (@TypeOf(ctx), []const u8) T) Error!?T {
        const t = (try self.treeOfKey(kind, k)) orelse return null;
        return t.lookupView(gpa, k, T, ctx, visit) catch |e| KvBackend.readError(e);
    }

    /// Range scan over `[start, end]` of one kind. Bounds of an ino-major
    /// kind inside one slot read its tree; bounds spanning slots (the
    /// corpse sweep, the layout census) and EVERY refs range (a ref key
    /// leads with the volume and the block — its owner, the routing
    /// component, varies freely inside any block or volume range, so the
    /// bounds' slots say nothing about the keys between them) merge the
    /// slot trees of every member in key order, `max` smallest first — so a
    /// caller resuming at `successor(last)` pages through the whole set.
    /// The merge (`merged_walk`) fetches a tree only once its front reaches
    /// the least key the routing law lets the tree hold, in chunks: a page
    /// reads O(max + the trees it consults), never `max` from every tree.
    pub fn range(self: *MetaSet, gpa: Allocator, kind: u8, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue)) Error!void {
        var cost: tree.WalkCost = .{};
        return self.rangeCounted(gpa, kind, start, end, max, out, &cost);
    }

    /// `range`, adding what the trees read to `cost` (fsck's walk accounting).
    pub fn rangeCounted(self: *MetaSet, gpa: Allocator, kind: u8, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue), cost: *tree.WalkCost) Error!void {
        if (max == 0 or key.lessThan(end, start)) return;
        if (kind == key.TREE_CONTROL) {
            const t = self.volumes[0].treeOf(.control) orelse return error.Corrupt;
            cost.sources += 1;
            return rangeIn(t, gpa, start, end, max, out, cost);
        }
        if (kind != key.TREE_BLOCK_REFS) {
            const s0 = route_table.slotOfKey(start) catch null;
            const s1 = route_table.slotOfKey(end) catch null;
            if (s0 != null and s1 != null and s0.? == s1.?) {
                const t = self.volumeOfSlot(s0.?).slotTree(s0.?) orelse return;
                cost.sources += 1;
                return rangeIn(t, gpa, start, end, max, out, cost);
            }
        }
        return self.mergedRange(gpa, start, end, max, out, cost, true);
    }

    /// The law-free merged walk (fsck C1's raw view): every slot tree's
    /// records in `[start, end]` with each tree's bound the start itself,
    /// so a record a tree holds outside its slot's window is visited too —
    /// at the cost of consulting every tree per call.
    pub fn rangeRawCounted(self: *MetaSet, gpa: Allocator, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue), cost: *tree.WalkCost) Error!void {
        if (max == 0 or key.lessThan(end, start)) return;
        return self.mergedRange(gpa, start, end, max, out, cost, false);
    }

    /// Every member's slot trees as the merge's sources, each bounded by
    /// the routing law (`law`) or by the start.
    fn mergedRange(self: *MetaSet, gpa: Allocator, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue), cost: *tree.WalkCost, law: bool) Error!void {
        var sources: std.ArrayList(TreeSource) = .empty;
        defer sources.deinit(gpa);
        const native_volume = self.route.volumeOf(0);
        for (self.volumes, 0..) |be, v| {
            for (be.trees.load()) |t| {
                const slot = t.forest_slot orelse continue; // the control tree
                // A member's forest-slot-0 tree holds the SET's native
                // keyspace only on slot 0's host; elsewhere it is the
                // volume's own control home (its writer claim at local
                // ino 1), never set content.
                if (slot == key.NATIVE_FOREST_SLOT and v != native_volume) continue;
                const lb = if (law) key.slotTreeLowerBound(slot, start) else key.KeyBuf.fromSlice(start);
                if (key.lessThan(end, lb.slice())) continue;
                try sources.append(gpa, .{ .t = t, .lb = lb });
            }
        }
        try merged_walk.merge(TreeSource, gpa, sources.items, end, max, out, cost);
    }

    /// One slot tree as a merged walk's source.
    const TreeSource = struct {
        t: *tree.KvTree,
        lb: key.KeyBuf,

        pub fn fetch(self: *TreeSource, gpa: Allocator, from: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue), cost: *tree.WalkCost) Error!void {
            return rangeIn(self.t, gpa, from, end, max, out, cost);
        }
    };

    fn rangeIn(t: *tree.KvTree, gpa: Allocator, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue), cost: *tree.WalkCost) Error!void {
        t.rangeCounted(gpa, start, end, max, out, cost) catch |e| return KvBackend.readError(e);
    }

    /// Commit `t`: the one member every record lives on; a tx spanning
    /// two members is a committer defect (module doc) — refused, nothing
    /// written, the tripwire fired.
    pub fn commit(self: *MetaSet, t: *KvTx) Error!void {
        var home: ?route_table.VolumeIndex = null;
        for (t.staged.items) |s| {
            const h = self.homeOfKey(s.tree_id, s.key) catch |e| {
                self.abandon(t);
                return e;
            };
            // A record of a slot with no tree yet (a Delete of a never-
            // minted slot; a Put by a caller that minted elsewhere): the
            // tree is minted here so the conveyor's routing finds it — only
            // for a slot this mount may write (0003 §X.5). A slot another
            // appender leases is the door's to refuse, and a tree minted
            // for it would shadow the root a later grant transfers; a
            // joiner's unleased slot is minted by its first touch's install.
            if (s.tree_id != key.TREE_CONTROL and h.be.slotTree(h.slot) == null and h.be.slots.mayMintAtCommit(h.slot)) _ = h.be.mintGuestTree(h.slot) catch {
                self.abandon(t);
                return error.Io;
            };
            const v = self.route.volumeOf(h.slot);
            if (home) |hv| {
                if (hv != v) {
                    var why: [256]u8 = undefined;
                    rstats.routing_split_refused.fire(std.fmt.bufPrint(&why, "a transaction spanning two members (committer {s}:{d}) reached the facade: the cross-owner intents are the law", .{ t.site.fn_name, t.site.line }) catch "a transaction spanning two members reached the facade: the cross-owner intents are the law");
                    self.abandon(t);
                    return error.Corrupt;
                }
            } else home = v;
        }
        const hv = home orelse {
            // An empty tx: nothing to route; the conveyor's own refusal.
            return self.volumes[0].commit(t);
        };
        return self.volumes[hv].commit(t);
    }

    /// Every acked entry of every member on stable media at the return
    /// (fsyncdir, syncfs, the cross-owner intents' barriers, the freed-
    /// block hold's drain): each member asked at once for everything it
    /// reserved, a member whose durable floor is already there asked
    /// nothing (decision 0003 §CH).
    pub fn fsync(self: *MetaSet) Error!void {
        if (self.posture == .reader) return;
        fsync_marks.stats.meta_fsync_set_calls.inc();
        const t0 = clock.monotonicNs();
        defer fsync_marks.stats.meta_fsync_set_ns.record(clock.monotonicNs() -| t0);
        if (self.fsync_scope == .set) return self.fsyncEveryMember(null);
        return self.barrierMembers(null);
    }

    /// fsync / fdatasync of key inode `ino` (charter §5.2, §6.4; decision
    /// 0003 §CH): on stable media at the return is every acked entry its
    /// durability depends on — its records, the dentries naming it and the
    /// dentry chain above it (`fsync_marks`) — with every entry below them
    /// on the same members; a member holding none of them is asked
    /// nothing. The members asked are asked at once.
    pub fn fsyncIno(self: *MetaSet, ino: u64) Error!void {
        if (self.posture == .reader) return;
        fsync_marks.stats.meta_fsync_ino_calls.inc();
        const t0 = clock.monotonicNs();
        defer fsync_marks.stats.meta_fsync_ino_ns.record(clock.monotonicNs() -| t0);
        if (self.fsync_scope == .set) return self.fsyncEveryMember(ino);
        return self.barrierMembers(ino);
    }

    /// The control arm (`SQUEEZEFS_META_FSYNC_SCOPE=set`): every member's
    /// trailing barrier, one after another — each request counted by what
    /// the member held of `ino`'s.
    fn fsyncEveryMember(self: *MetaSet, ino: ?u64) Error!void {
        for (self.volumes, self.fsync_counts) |be, *c| {
            if (ino) |i| classify(c, be.cv.fsyncNeed(i), be.durableFloor());
            c.requests.inc();
            fsync_marks.stats.meta_fsync_member_requests.inc();
            try be.fsync();
        }
    }

    fn classify(c: *MemberFsync, need: fsync_marks.Marks.Need, floor: u64) void {
        if (need.own > floor) {
            c.requests_own.inc();
            fsync_marks.stats.meta_fsync_requests_own.inc();
        } else if (need.all > floor) {
            c.requests_chain.inc();
            fsync_marks.stats.meta_fsync_requests_chain.inc();
        } else {
            c.requests_none.inc();
            fsync_marks.stats.meta_fsync_requests_none.inc();
        }
    }

    /// Ask each member for what it owes (`ino`'s need, or with `ino` null
    /// everything it reserved) at once, then wait for every one asked. A
    /// FAILED member fails every durability point first (ext4's journal
    /// abort: no fsync of the set is acked over a member that latched).
    fn barrierMembers(self: *MetaSet, ino: ?u64) Error!void {
        for (self.volumes) |be| if (be.isFailed()) return error.VolumeFailed;
        var waiters: [FSYNC_PARALLEL]journal_lane.BarrierWaiter = undefined;
        var first_err: ?Error = null;
        var base: usize = 0;
        while (base < self.volumes.len) : (base += FSYNC_PARALLEL) {
            const end = @min(base + FSYNC_PARALLEL, self.volumes.len);
            var asked: [FSYNC_PARALLEL]bool = @splat(false);
            for (self.volumes[base..end], self.fsync_counts[base..end], 0..) |be, *c, k| {
                const floor = be.durableFloor();
                const cover = if (ino) |i| blk: {
                    const need = be.cv.fsyncNeed(i);
                    if (need.all > floor) classify(c, need, floor);
                    break :blk need.all;
                } else be.cv.reservedHwm();
                if (cover <= floor) {
                    c.skips.inc();
                    fsync_marks.stats.meta_fsync_member_skips.inc();
                    continue;
                }
                be.requestDurable(&waiters[k], cover) catch |e| {
                    first_err = e;
                    break;
                };
                asked[k] = true;
                c.requests.inc();
                fsync_marks.stats.meta_fsync_member_requests.inc();
            }
            // Every node asked is answered before the frame goes.
            for (asked[0 .. end - base], waiters[0 .. end - base]) |a, *w| {
                if (a) KvBackend.awaitDurable(w) catch |e| if (first_err == null) {
                    first_err = e;
                };
            }
            if (first_err) |e| return e;
        }
    }

    /// The freed-block hold's seal (charter §6.2): per member, the
    /// reservation high-water mark NOW — every commit answered before this
    /// call (its window reserved before its apply, its apply before its
    /// answer) lies below it. `out.len == volumes.len`.
    pub fn durabilityMarks(self: *MetaSet, out: []u64) void {
        std.debug.assert(out.len == self.volumes.len);
        if (self.posture == .reader) return @memset(out, 0);
        for (self.volumes, out) |be, *m| m.* = be.cv.reservedHwm();
    }

    /// Every member's last completed barrier covers its mark: the records
    /// below `marks` are on stable media. A reader writes nothing.
    pub fn marksDurable(self: *MetaSet, marks: []const u64) bool {
        if (self.posture == .reader) return true;
        for (self.volumes, marks) |be, m| if (be.lane.durableFloor() < m) return false;
        return true;
    }

    /// One full cycle with an immediate barrier on every member.
    pub fn checkpointNow(self: *MetaSet) checkpoint_cycle.CycleError!void {
        if (self.posture == .reader) return;
        for (self.volumes) |be| try be.checkpointNow();
    }

    pub fn nameKey(self: *const MetaSet) *const key.NameKey {
        return &self.name_key;
    }

    pub fn nodeSize(self: *const MetaSet) usize {
        return self.node_size;
    }

    pub fn setUuid(self: *const MetaSet) [16]u8 {
        return self.set_uuid;
    }

    /// `max` over the members (the staging generation's epoch).
    pub fn setEpoch(self: *const MetaSet) u64 {
        var e: u64 = 0;
        for (self.volumes) |be| e = @max(e, be.setEpoch());
        return e;
    }

    pub const Space = backend.KvBackend.Space;

    /// Σ over the members' heaps.
    pub fn space(self: *const MetaSet) Space {
        var s: Space = .{ .total_bytes = 0, .free_bytes = 0 };
        for (self.volumes) |be| {
            const v = be.space();
            s.total_bytes += v.total_bytes;
            s.free_bytes += v.free_bytes;
        }
        return s;
    }

    pub fn isFailed(self: *const MetaSet) bool {
        for (self.volumes) |be| if (be.isFailed()) return true;
        return false;
    }

    /// The one-line law over the SET (a member FAILED fails every mutation
    /// of the mount): the data plane's failure lines.
    pub fn admitsLine(self: *MetaSet, class: journal.FailureLatch.LogClass) bool {
        return self.line_marks.admits(class, self);
    }

    pub fn lineSeam(self: *MetaSet) journal.FailureLatch.LineSeam {
        return .{ .ctx = self, .admits = admitsLineFn };
    }

    fn admitsLineFn(ctx: *anyopaque, class: journal.FailureLatch.LogClass) bool {
        const self: *MetaSet = @ptrCast(@alignCast(ctx));
        return self.admitsLine(class);
    }

    pub fn stampBlockMapTree(self: *MetaSet) Error!void {
        for (self.volumes) |be| try be.stampBlockMapTree();
    }

    /// @anchor:ops_jobs_lifecycle_bit — bit 3 on EVERY member, barriered.
    pub fn stampVolumeLifecycle(self: *MetaSet) Error!void {
        for (self.volumes) |be| try be.stampVolumeLifecycle();
    }

    /// The native watermark of slot 0's host (the statfs face of a
    /// one-member set; `allocatedInos` is the set-wide figure).
    pub fn nextIno(self: *const MetaSet) u64 {
        return self.volumes[0].nextIno();
    }

    /// Σ over every allocation cursor's progression (`inos.liveInodes`
    /// before the destroys): the live-population input of `statfs`.
    pub fn allocatedInos(self: *MetaSet) u64 {
        var n: u64 = 0;
        for (self.volumes) |be| {
            const guests = be.stamp.cursorSnapshots(self.gpa) catch &.{};
            defer if (guests.len != 0) self.gpa.free(guests);
            n +|= inos.liveInodes(be.nextIno(), guests, 0);
        }
        return n;
    }

    // ---- mints ----

    /// The set's directory spread depth (`set_config.dir_spread_depth`):
    /// the mount stores it once the record is read, before the FUSE
    /// session arms; `.stats` `routing_dir_spread_depth` publishes it.
    pub fn setDirSpreadDepth(self: *MetaSet, depth: u8) void {
        std.debug.assert(dir_place.depthValid(depth));
        self.dir_spread_depth.store(depth, .monotonic);
        rstats.routing_dir_spread_depth.set(depth);
    }

    pub fn dirSpreadDepth(self: *const MetaSet) u8 {
        return self.dir_spread_depth.load(.monotonic);
    }

    /// Can a directory's depth change its member on this set: a spread
    /// depth ≥ 1 and more than one healthy member? The mkdir path asks
    /// this BEFORE it reads a parent's depth, so a one-member set, a set at
    /// `D = 0` and a set down to one healthy member do no work for the
    /// rule — two atomic words and each member's FAILED latch.
    pub fn dirsMaySpread(self: *const MetaSet) bool {
        if (self.volumes.len < 2 or self.dirSpreadDepth() == 0) return false;
        var healthy: usize = 0;
        for (self.volumes) |be| healthy += @intFromBool(!be.isFailed());
        return healthy > 1;
    }

    /// The health vector the mint pick reads: `min(1000, free/total ×
    /// 1000)` from each member's allocator (a RAM word, never a probe);
    /// a FAILED member is disabled.
    fn healths(self: *MetaSet, out: []?u32) []?u32 {
        for (self.volumes, 0..) |be, i| {
            const sp = be.space();
            const ns = be.nodeSize();
            out[i] = if (be.isFailed()) null else router.volumeHealth(sp.free_bytes / ns, sp.total_bytes / ns, true);
        }
        return out[0..self.volumes.len];
    }

    /// Mint a child of `parent_eff`. The member first (charter §5.4 "Mint
    /// spread", decision 0003 §AA.1 and §BZ — `dir_place.homesOnParent`):
    /// a child of a directory this mount leases outside the native slot
    /// stays on its parent's member (`Plane.homesChildren`) UNLESS it is a
    /// directory at depth ≤ the set's spread depth, which — like every
    /// child of `/` and of a foreign directory — takes the health-weighted
    /// round-robin over the members (`router.pickMintVolume`). Then the
    /// slot on that member: on an armed plane its decision — `gather`, the
    /// parent's slot under the affinity ceiling, else the rotor tree with
    /// the most headroom, an overflow rotor past the cap — else the
    /// member's rotor. `prefer_parent` keeps the mint on the parent's
    /// member (a rename's whiteout: the op stays on two members at most).
    /// `child` says a file or a directory at the depth it will record
    /// (`dir_place.Child`, `dir_place.childDepth`). The child's slot takes
    /// its door token at the commit (the record names it). Returns the
    /// child's EFFECTIVE (key) ino; `globalIno` inverts it. `caller` is the
    /// minting stream (the FUSE request's pid; 0 = none: the calling
    /// thread) whose mint window a rotor mint rides (decision 0003 §BV).
    pub fn allocateIno(self: *MetaSet, parent_eff: u64, prefer_parent: bool, gather: bool, caller: u32, child: dir_place.Child) Error!u64 {
        if (self.posture == .reader) return error.PartitionRefused;
        const parent_v = self.route.volumeOfEffective(parent_eff);
        const parent_slot = route_table.slotOfEffective(parent_eff);
        const is_dir = child == .dir;
        const home = prefer_parent or self.volumes.len == 1 or (self.volumes[parent_v].slots.homesChildren(parent_slot) and !dir_place.spreads(child.depth(), self.dirSpreadDepth()));
        var hb: [route_table.MAX_VOLUMES]?u32 = undefined;
        // A directory walks the band by its ordinal under its parent
        // (`dir_place.pickOrdinal`); a file takes the shared round-robin.
        const v: route_table.VolumeIndex = if (home) parent_v else switch (child) {
            .file => @intCast(router.pickMintVolume(parent_v, self.healths(hb[0..self.volumes.len]), &self.mint_rr)),
            .dir => |d| @intCast(router.pickMintVolumeAt(parent_v, self.healths(hb[0..self.volumes.len]), dir_place.pickOrdinal(parent_v, d.ordinal))),
        };
        if (v != parent_v) rstats.routing_mint_volume_picks.inc();
        if (is_dir) {
            if (!home) rstats.routing_dir_place_rotor.inc();
            if (v != parent_v) rstats.routing_dir_place_remote.inc() else rstats.routing_dir_place_local.inc();
            self.mints[v].dirs.inc();
        } else self.mints[v].files.inc();
        const plane = self.volumes[v].slots;
        const minter = routing_slots.mint_window.minterToken(caller);
        var burns: u64 = 0;
        while (true) {
            const rr = self.slot_rr.fetchAdd(1, .monotonic);
            const m = plane.mintSlot(parent_slot, v == parent_v, gather, rr, minter) orelse SlotPlane.Mint{ .slot = self.rotors[v].pickSlot() };
            plane.holdMintForTest(m.slot);
            const be = self.volumeOfSlot(m.slot);
            const local = be.allocateInoInSlot(m.slot) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                else => error.Io,
            };
            // The handover law (decision 0003 §BV): the ino is kept only
            // if its slot still stands at the decision's generation.
            if (m.g) |g| if (!plane.mintStands(m.slot, g, minter)) {
                routing_slots.acquire.stats.mint_burns.inc();
                burns += 1;
                // Each burn met a lease that moved under its mint; the
                // census then skips the releasing slot or reads its new
                // generation, so a run of them is a lease storm: the op
                // goes back to the move gate (re-issued, every home
                // re-resolved) rather than spin here.
                if (burns >= routing_slots.acquire.rotorCap(routing_slots.acquire.MINT_SPREAD)) return error.Deferred;
                continue;
            };
            return route_table.effectiveIno(inos.makeGlobalIno(local, m.slot, inos.ROUTING_WIDTH));
        }
    }

    /// The filesystem's directory-parent memo for the handover's subtree
    /// credit (charter §5.5 "`ops_h` counts the holder's namespace ops on
    /// the slot's SUBTREE"): bound on every member's plane.
    pub fn bindParentMemo(self: *MetaSet, memo: routing_slots.handover.ParentMemo) void {
        if (self.posture == .reader) return;
        self.bound_memo = memo;
        for (self.volumes) |be| be.slots.bindMemo(memo);
    }

    /// The mount's slot-move epoch into every member's lease table
    /// (decision 0003 §X.1). A `-o ro` reader has no slot plane: nothing
    /// it holds moves, its move gate re-polls.
    pub fn bindMoves(self: *MetaSet, m: *routing_slots.moves.Moves) void {
        if (self.posture == .reader) return;
        self.bound_moves = m;
        for (self.volumes) |be| be.slots.table.bindMoves(m);
    }

    /// The fail-stop lattice's terminal verdict on a joined mount (its
    /// POISON: `T_park_max` passed, the home shard's `UnknownLease`): every
    /// volume latched FAILED (mutations EIO until remount) AND the mount's
    /// move epoch closed — an op parked on a move-class refusal (the root's
    /// token serve answering MembershipPending to a member the shard
    /// evicted) answers EIO now, not at the lattice's `T_park_max`, and no
    /// later one waits: "the daemon answers, never holds" (a storm child
    /// parked in such a request outlived SIGKILL and hung the shard).
    pub fn poison(self: *MetaSet) void {
        for (self.volumes) |be| _ = be.lane.latch.failStop();
        if (self.posture == .reader) return;
        for (self.volumes) |be| be.slots.table.closeMoves();
    }

    /// The handover's step-0 drain over every member's plane (the
    /// filesystem's parked stamps of a slot, committed before its door
    /// closes).
    pub fn bindTimesDrain(self: *MetaSet, drain: routing_slots.handover.TimesDrain) void {
        // A `-o ro` reader has no slot plane (nothing it holds moves).
        if (self.posture == .reader) return;
        self.bound_times_drain = drain;
        for (self.volumes) |be| be.slots.bindTimesDrain(drain);
    }

    /// The kernel view on every writer's conveyor (decision 0003 §AB.2),
    /// kept for a successor incarnation; null disarms.
    pub fn armKernel(self: *MetaSet, view: ?*const conveyor.KernelView) void {
        if (self.posture == .reader) return;
        self.bound_kernel = view;
        for (self.volumes) |be| if (view) |v| be.cv.armKernel(v) else be.cv.disarmKernel();
    }

    /// The token recall on every member's pass (the wire plane's hook),
    /// kept for a successor incarnation; null disarms.
    pub fn armTokens(self: *MetaSet, hook: ?*const conveyor.TokenHook) void {
        self.bound_tokens = hook;
        for (self.volumes) |be| if (hook) |h| be.cv.armTokens(h) else be.cv.disarmTokens();
    }

    // ---- the re-join in place (decision 0003 §BS) ----

    fn keepRejoinInputs(self: *MetaSet, paths: []const []const u8, order: []const usize, opts: OpenOptions) OpenError!void {
        const gpa = self.gpa;
        const join = opts.join orelse return;
        const owned = try gpa.alloc([]u8, paths.len);
        var n: usize = 0;
        errdefer {
            for (owned[0..n]) |p| gpa.free(p);
            gpa.free(owned);
        }
        for (order) |oi| {
            owned[n] = try gpa.dupe(u8, paths[oi]);
            n += 1;
        }
        const mp = try gpa.dupe(u8, join.mountpoint);
        self.rejoin_paths = owned;
        self.rejoin_join = .{ .mountpoint = mp, .endpoint = join.endpoint };
        self.rejoin_opts = .{ .env = opts.env, .pool = self.pool, .cache_budget_bytes = opts.cache_budget_bytes, .cpus = opts.cpus, .test_error_logs = opts.test_error_logs, .guard = opts.guard };
    }

    /// The successor incarnations of every member: each volume JOINED
    /// afresh through the mount's own door (`KvBackend.openJoined` — the
    /// census, the endpoint poll, `awaitManagerListener`, `JoinAppender`,
    /// the rotor, the first checkpoint). All or none: a failed open closes
    /// the ones already opened (a fresh join's clean leave) and answers.
    pub fn openSuccessors(self: *MetaSet) OpenError![]*KvBackend {
        return self.openSuccessorsOf(self.volumes);
    }

    fn openSuccessorsOf(self: *MetaSet, predecessors: []const *KvBackend) OpenError![]*KvBackend {
        const gpa = self.gpa;
        if (self.posture != .joined or self.rejoin_paths.len != self.volumes.len) return error.SetRefused;
        const fresh = try gpa.alloc(*KvBackend, self.volumes.len);
        var n: usize = 0;
        errdefer {
            for (fresh[0..n]) |be| be.close();
            gpa.free(fresh);
        }
        for (self.rejoin_paths, 0..) |p, i| {
            var o = self.rejoin_opts;
            o.index = @intCast(i);
            var j = self.rejoin_join;
            j.predecessor = if (predecessors[i].joined) |js| js.appenderId() else null;
            if (i > 0) j.secret = fresh[0].joined.?.secret;
            fresh[n] = try KvBackend.openJoined(gpa, self.io, p, o, j);
            n += 1;
        }
        return fresh;
    }

    /// Every retired incarnation's node cache not yet released (the beat,
    /// once the ops in flight at the publication answered). Answers the
    /// bytes released.
    pub fn releaseRetiredCaches(self: *MetaSet) u64 {
        var bytes: u64 = 0;
        for (self.retired.items[self.retired_released..]) |be| bytes += be.releaseRetiredCache();
        self.retired_released = self.retired.items.len;
        return bytes;
    }

    /// Publish the successor incarnations (owned slice, freed here): each
    /// carries every binding its predecessor carried, then replaces it in
    /// `volumes` — ONE pointer store per member (Release). A reader that
    /// loaded the predecessor before the store keeps a live object: the
    /// retired incarnation stays mapped until `close` (it refuses every
    /// mutation `Deferred` and serves reads off the state its home shard
    /// recovered).
    pub fn publishSuccessors(self: *MetaSet, fresh: []*KvBackend) Allocator.Error!void {
        const gpa = self.gpa;
        defer gpa.free(fresh);
        try self.retired.ensureUnusedCapacity(gpa, fresh.len);
        for (fresh, 0..) |be, i| {
            const old = self.volumes[i];
            if (self.bound_moves) |m| be.slots.table.bindMoves(m);
            if (self.bound_memo) |m| be.slots.bindMemo(m);
            if (self.bound_times_drain) |d| be.slots.bindTimesDrain(d);
            if (self.bound_kernel) |v| be.cv.armKernel(v);
            if (self.bound_tokens) |h| be.cv.armTokens(h);
            be.tail_announce = old.tail_announce;
            if (be.tail_announce) |a| if (be.tail) |t| {
                if (!a.announce(a.ctx, be.index, t.fd)) log.warn("meta volume {s}: the successor incarnation's journal tail could not be handed to the supervisor — a crash of this process has the flush-window exposure on it", .{be.path});
            };
            // The token recall leaves the predecessor's pass (its batches
            // refuse at the latch before they recall anything).
            old.cv.disarmTokens();
            @atomicStore(*KvBackend, &self.volumes[i], be, .release);
            self.retired.appendAssumeCapacity(old);
        }
        self.rejoins += 1;
    }

    /// The one closed cutover slot across the members, if any.
    fn closedSlot(self: *MetaSet) ?u16 {
        if (self.posture == .reader) return null;
        for (self.volumes) |be| if (be.slots.door.closedSlot()) |s| return s;
        return null;
    }

    // ---- the engine's faces ----

    /// The set's delta tee, reset for `slot` (one migration at a time).
    pub fn teeFor(self: *MetaSet, slot: u16, cap: usize) Allocator.Error!*routing.migration_tee.MigrationTee {
        if (self.tee) |t| {
            if (t.cap == cap) {
                t.reset(slot);
                return t;
            }
            t.deinit();
            self.tee = null;
        }
        const t = try routing.migration_tee.MigrationTee.init(self.gpa, slot, cap);
        self.tee = t;
        self.tee_hook = .{ .ctx = t, .note = teeNote };
        return t;
    }

    fn teeNote(ctx: *anyopaque, recs: []const tx.Rec) void {
        const t: *routing.migration_tee.MigrationTee = @ptrCast(@alignCast(ctx));
        t.noteCommitted(recs);
    }

    /// The published table (caller releases the snapshot).
    pub fn table(self: *MetaSet) route_table.Published.Snapshot {
        return self.route.load();
    }

    /// Re-home slot `slot` to member `volume` in the RAM table (the flip's
    /// swap; the gate protocol guarantees no admitted mutation straddles
    /// it) and rebuild both rotors.
    pub fn publishSlot(self: *MetaSet, slot: u16, volume: route_table.VolumeIndex) Allocator.Error!void {
        const snap = self.route.load();
        defer snap.release();
        const from = snap.value.volumeOf(slot);
        const next = snap.value.withSlot(slot, volume);
        try self.route.publish(self.gpa, next);
        var buf: [router.MINT_SPREAD]u16 = undefined;
        self.rotors[from] = router.MintRotor.init(next.hostedAscending(from, &buf));
        self.rotors[volume] = router.MintRotor.init(next.hostedAscending(volume, &buf));
    }

    // ---- stats ----

    /// The set-level words + every member's per-volume families.
    pub fn writeStatsJson(self: *MetaSet, w: *Io.Writer) Io.Writer.Error!void {
        try w.print("{{\"routing_volumes\":{d},\"routing_width\":{d},\"routing_set_epoch\":{d},\"routing_closed_slot\":{?d},\"mount_posture\":\"{s}\",\"meta_rejoins_in_place\":{d},\"volumes\":[", .{ self.volumes.len, route_table.W, self.setEpoch(), self.closedSlot(), @tagName(self.posture), self.rejoins });
        for (self.volumes, 0..) |be, i| {
            if (i != 0) try w.writeByte(',');
            try w.print("{{\"index\":{d},\"epoch\":{d},\"hosted_slots\":{d},\"trees\":{d},\"native\":{},\"failed\":{},\"conveyor\":", .{ i, be.setEpoch(), be.stamp.hostedCount(), be.trees.load().len, be.stamp.native_slot != null, be.isFailed() });
            if (be.isReader()) {
                // A reader runs no conveyor and no checkpointer: the
                // projection's checkpoint words stand in.
                try w.print("null,\"checkpointer\":null,\"projection_seq\":{d},\"sym\":{{\"manager_lease\":\"reader\"}}}}", .{be.mounted.seq});
                continue;
            }
            try be.conveyorStats().writeJson(w);
            // The heap census (`defrag --meta`'s before/after words): node
            // images = allocated extents less the retirements awaiting their
            // covering barrier.
            const census = be.residentNodeCensus();
            try w.print(",\"heap_total_extents\":{d},\"heap_free_extents\":{d},\"heap_pending_free\":{d},\"resident_leaves\":{d},\"resident_interior\":{d},\"mint_files\":{d},\"mint_dirs\":{d}", .{ be.alloc.totalExtents(), be.alloc.freeExtents(), be.alloc.pendingCount(), census.leaves, census.interior, self.mints[i].files.load(), self.mints[i].dirs.load() });
            // The member's durability points (decision 0003 §CH): its lane's
            // barriers, what the set asked of it, what its lane joined.
            const lane = be.laneSnapshot();
            const fc = &self.fsync_counts[i];
            try w.print(",\"fsync\":{{\"lane_barriers\":{d},\"requests\":{d},\"requests_own\":{d},\"requests_chain\":{d},\"requests_none\":{d},\"skips\":{d},\"joins\":{d},\"durable_at_admit\":{d},\"durable_floor\":{d}}}", .{ lane.barriers_coalesced + lane.barriers_strict, fc.requests.load(), fc.requests_own.load(), fc.requests_chain.load(), fc.requests_none.load(), fc.skips.load(), lane.fsync_joins, lane.fsync_durable_at_admit, be.durableFloor() });
            try w.writeAll(",\"checkpointer\":");
            try counters.writeFieldsJson(&be.cp, w);
            if (be.guard) |g| {
                try w.writeAll(",\"guard\":");
                try g.writeStatsJson(w);
            }
            try w.writeAll(",\"sym\":");
            if (be.sym) |set| try set.writeStatsJson(w) else if (be.joined) |js| try js.writeStatsJson(w) else try backend.KvBackend.writeSoloSymStatsJson(be, w);
            try w.writeByte('}');
        }
        try w.writeAll("]}");
    }
};

// ---------------------------------------------------------------------------
// Tests (Linux; regular files under /build/tmp, O_DIRECT): the two-member
// set end to end — format, discovery in either URI order, mints spread
// over both members, the cross-member creates as intents, remount.
// ---------------------------------------------------------------------------

const testing = std.testing;
const exec = @import("../exec/exec.zig");
const wire_auth = @import("../wire/auth.zig");
const economy = @import("economy.zig");
const record = @import("record.zig");
const geometry = @import("geometry.zig");
const align_probe = @import("../io/align_probe.zig");

pub const test_dir = "/build/tmp";
const test_volume_len: u64 = 64 << 20;
const test_set_identity: geometry.SetIdentity = .{ .set_uuid = "meta-set-fixture".*, .hash_seed = 0x5E7_5EED_0000_0001 };

/// A formatted N-member set on regular files (the harness every routing
/// e2e shares; `pub` for the engine's and the verbs' tests).
pub const TestSet = struct {
    gpa: Allocator,
    env: std.process.Environ.Map,
    sqz: *exec.SqzIo,
    paths: [8][]u8 = undefined,
    n: usize,

    /// The set's `job:enroll` secret (what `format` mints; member 0's
    /// control tree carries it, so the cluster wire can arm over a test set).
    pub const secret: wire_auth.Secret = @splat(0x5C);

    pub fn create(gpa: Allocator, name: []const u8, members: u16) !*TestSet {
        return createClass(gpa, name, members, .symmetric);
    }

    /// `create` with the format class: `.single_writer` is the flat class
    /// (`--single-writer`: no appender region, no SYMMETRIC_PLANE bit).
    pub fn createClass(gpa: Allocator, name: []const u8, members: u16, class: geometry.Class) !*TestSet {
        const self = try gpa.create(TestSet);
        errdefer gpa.destroy(self);
        self.* = .{ .gpa = gpa, .env = std.process.Environ.Map.init(gpa), .sqz = undefined, .n = members };
        errdefer self.env.deinit();
        self.sqz = try exec.SqzIo.testInstance();
        errdefer self.sqz.deinit();
        var zbuf: [linux.PATH_MAX]u8 = undefined;
        _ = linux.mkdirat(linux.AT.FDCWD, try align_probe.pathZ(&zbuf, test_dir), 0o755);
        for (0..members) |i| {
            self.paths[i] = try std.fmt.allocPrint(gpa, "{s}/sqz-set-{d}-{s}-{d}.img", .{ test_dir, linux.getpid(), name, i });
            try truncateFile(self.paths[i], test_volume_len);
            var uuid: [16]u8 = "meta-set-vol-000".*;
            uuid[15] = @intCast('0' + i);
            const enroll = wire_auth.encodeEnroll(secret);
            try backend.formatVolume(gpa, self.paths[i], .{
                .env = &self.env,
                .class = class,
                .node_size = 65536,
                .journal_len_override = 1 << 20,
                .uuid = uuid,
                .set = test_set_identity,
                .member_position = @intCast(i),
                .member_count = members,
                .control_records = if (i == 0) &.{.{ .key = wire_auth.ENROLL_KEY, .seq = 0, .kind = .put, .value = &enroll }} else &.{},
            });
        }
        return self;
    }

    pub fn destroy(self: *TestSet) void {
        for (self.paths[0..self.n]) |p| {
            var zbuf: [linux.PATH_MAX]u8 = undefined;
            if (align_probe.pathZ(&zbuf, p)) |z| {
                _ = linux.unlink(z);
            } else |_| {}
            self.gpa.free(p);
        }
        self.sqz.deinit();
        self.env.deinit();
        self.gpa.destroy(self);
    }

    pub fn io(self: *TestSet) Io {
        return self.sqz.io();
    }

    pub fn pathList(self: *TestSet) []const []const u8 {
        return @ptrCast(self.paths[0..self.n]);
    }

    pub fn open(self: *TestSet) !*MetaSet {
        return MetaSet.open(self.gpa, self.io(), self.pathList(), .{ .env = &self.env, .cache_budget_bytes = 64 << 20, .cpus = 4 });
    }

    /// Open with the URI reversed (order independence).
    pub fn openReversed(self: *TestSet) !*MetaSet {
        var rev: [8][]const u8 = undefined;
        for (0..self.n) |i| rev[i] = self.paths[self.n - 1 - i];
        return MetaSet.open(self.gpa, self.io(), rev[0..self.n], .{ .env = &self.env, .cache_budget_bytes = 64 << 20, .cpus = 4 });
    }
};

fn truncateFile(path: []const u8, len: u64) !void {
    var zbuf: [linux.PATH_MAX]u8 = undefined;
    const z = try align_probe.pathZ(&zbuf, path);
    const rc = linux.open(z, .{ .ACCMODE = .RDWR, .CLOEXEC = true, .CREAT = true, .TRUNC = true }, 0o644);
    if (linux.errno(rc) != .SUCCESS) return error.CannotCreateDevice;
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    if (linux.errno(linux.ftruncate(fd, @intCast(len))) != .SUCCESS) return error.CannotSizeDevice;
}

/// One create under `parent_global` through the facade: the mint, then
/// either the one-member entry or — when the child's rotor lives on
/// another member — the cross-owner INTENT over a plane bound to the set
/// (the product's `kv_fs` arm). Returns the GLOBAL ino.
pub fn createFile(set: *MetaSet, parent_global: u64, name: []const u8) !u64 {
    return createChild(set, parent_global, name, economy.S.IFREG | 0o644);
}

/// `createFile` for any mode (a directory parent for the mint-placement
/// rows): a directory child records its depth off the parent's record,
/// exactly as `kv_fs.createChildAt` does.
pub fn createChild(set: *MetaSet, parent_global: u64, name: []const u8, mode: u32) !u64 {
    const parent = set.effectiveIno(parent_global);
    const pv_bytes = (try set.lookup(set.gpa, key.TREE_INODES, &key.inodeKey(parent))) orelse return error.ParentMissing;
    defer set.gpa.free(pv_bytes);
    const pv = try record.InodeValue.decode(pv_bytes);
    const depth: u8 = if (economy.isDirMode(mode)) dir_place.childDepth(parent_global == inos.ROOT_INO, pv.dirDepth()) else 0;
    const flags2 = record.InodeValue.flags2ForDepth(depth);
    // The ordinal as the handler keeps it: the directories already under
    // the parent (`nlink − 2` on a serial harness).
    const child = try set.allocateIno(parent, false, false, 0, if (depth != 0) .{ .dir = .{ .depth = depth, .ordinal = pv.nlink -| 2 } } else .file);
    const global = set.globalIno(child);
    const parent_slot = route_table.slotOfEffective(parent);
    if (set.volumeIndexOfSlot(parent_slot) != set.volumeIndexOfSlot(route_table.slotOf(global))) {
        var locks = try dlm.Manager.init(set.gpa, 64);
        defer locks.deinit();
        const bound = try xv.set_bind.Bound.create(set.gpa, set.io, set, &locks, set.env);
        defer bound.destroy(set.gpa);
        const plan = xv.steps.planCreate(.{ .parent = parent_global, .name = name, .child = global, .mode = mode, .uid = 0, .gid = 0, .now = 1000, .flags2 = flags2 });
        const scope = bound.plane.mintScope();
        var held = try bound.plane.acquirePlanGuards(&plan, scope, false);
        defer held.release(set.gpa);
        const done = try bound.plane.execute(&plan, scope, 1000);
        if (done.refused) |e| return e;
        return global;
    }
    const t = try set.begin(@src());
    _ = economy.stageCreate(t, .{ .name_key = set.nameKey(), .now = 1000, .parent = parent, .parent_value = pv, .name = name, .mode = mode, .uid = 0, .gid = 0, .local_ino = child, .global_ino = global, .flags2 = flags2 }) catch |e| {
        set.abandon(t);
        return e;
    };
    try set.commit(t);
    return global;
}

pub fn present(set: *MetaSet, global: u64) !bool {
    const got = try set.lookup(testing.allocator, key.TREE_INODES, &key.inodeKey(set.effectiveIno(global)));
    defer if (got) |g| testing.allocator.free(g);
    return got != null;
}

test "a two-member set: format → open (either URI order) → 200 creates spread over both members' rotors (the cross-member creates as intents) → remount: every record present on its home, the stamps carry the cursors, the sweep-style cross-tree range sees everything" {
    const gpa = testing.allocator;
    const s = try TestSet.create(gpa, "two", 2);
    defer s.destroy();
    var acked: [200]u64 = undefined;
    {
        const set = try s.open();
        try testing.expectEqual(@as(usize, 2), set.volumeCount());
        try testing.expectEqual(@as(?u16, 0), set.volumes[0].stamp.native_slot);
        try testing.expectEqual(@as(?u16, null), set.volumes[1].stamp.native_slot);
        var nb: [16]u8 = undefined;
        for (&acked, 0..) |*a, i| a.* = try createFile(set, inos.ROOT_INO, try std.fmt.bufPrint(&nb, "f{d}", .{i}));
        var on: [2]u32 = @splat(0);
        for (acked, 0..) |g, i| {
            if (!try present(set, g)) {
                std.debug.print("acked[{d}] global {d} (slot {d}, member {d}) is absent\n", .{ i, g, route_table.slotOf(g), set.volumeIndexOfSlot(route_table.slotOf(g)) });
                return error.TestUnexpectedResult;
            }
            on[set.volumeIndexOfSlot(route_table.slotOf(g))] += 1;
        }
        // Both members minted (the 9/10 band of two equal volumes is both).
        try testing.expect(on[0] > 50 and on[1] > 50);
        try testing.expect(xv.intent.stats.xv_cross_owner_intents_minted.load() > 0);
        try testing.expectEqual(xv.intent.stats.xv_cross_owner_intents_minted.load(), xv.intent.stats.xv_cross_owner_intents_retired.load() + xv.intent.stats.xv_cross_owner_intents_open.load());
        try testing.expect(rstats.routing_guest_trees_minted.load() >= 64);
        // A cross-tree range (the corpse sweep's shape) over the whole
        // ino-major space sees every inode record on both members.
        var kvs: std.ArrayList(tree.KeyValue) = .empty;
        defer tree.deinitKeyValues(gpa, &kvs);
        const start = key.inodeKey(0);
        const end = key.inodeKey(std.math.maxInt(u64));
        try set.range(gpa, key.TREE_INODES, &start, &end, 10_000, &kvs);
        var inode_records: usize = 0;
        for (kvs.items) |kv| if (kv.key.len == key.INODE_KEY_LEN and kv.key[key.INO_MAJOR_KIND_OFF] == key.TREE_INODES) {
            inode_records += 1;
        };
        try testing.expectEqual(@as(usize, 201), inode_records);
        // Strictly ascending: one copy of every key.
        for (kvs.items[1..], 1..) |kv, i| try testing.expect(key.lessThan(kvs.items[i - 1].key, kv.key));
        try set.checkpointNow();
        set.close();
    }
    {
        const set = try s.openReversed();
        defer set.close();
        for (acked, 0..) |g, i| if (!try present(set, g)) {
            std.debug.print("after the remount acked[{d}] global {d} (slot {d}, member {d}) is absent\n", .{ i, g, route_table.slotOf(g), set.volumeIndexOfSlot(route_table.slotOf(g)) });
            return error.TestUnexpectedResult;
        };
        // The cursors travelled through the stamp: a new mint never
        // collides with an acked ino.
        const fresh = try createFile(set, inos.ROOT_INO, "fresh");
        for (acked) |g| try testing.expect(g != fresh);
        try testing.expect(try present(set, fresh));
        try testing.expectEqual(@as(u64, 201), set.allocatedInos());
    }
}

test "the mint window follows its CALLER, not the thread (decision 0003 §BV): a lone caller keeps the census's per-mint spread; a one-off caller beside it takes a tree of its own and never makes it concurrent; a second caller's window — off every other window's tree — follows it to another thread, and a third caller on the first thread takes a third tree" {
    const gpa = testing.allocator;
    const s = try TestSet.create(gpa, "mint-caller", 1);
    defer s.destroy();
    const set = try s.open();
    defer set.close();
    const root = set.effectiveIno(inos.ROOT_INO);
    const slotOfEff = struct {
        fn of(st: *MetaSet, eff: u64) u16 {
            return route_table.slotOf(st.globalIno(eff));
        }
    }.of;
    // A lone caller: no window, the per-mint spread (two mints, two trees).
    const g0 = routing_slots.mint_window.stats.mint_window_grants.load();
    const l0 = slotOfEff(set, try set.allocateIno(root, false, false, 900, .file));
    const l1 = slotOfEff(set, try set.allocateIno(root, false, false, 900, .file));
    try testing.expect(l0 != l1);
    // A one-off caller beside it (a `mkdir`): it takes a window of its own
    // (its mint off the creator's trees) and never makes the creator
    // concurrent — the creator's spread goes on.
    const off = slotOfEff(set, try set.allocateIno(root, false, false, 7, .file));
    try testing.expectEqual(g0 + 1, routing_slots.mint_window.stats.mint_window_grants.load());
    const l2 = slotOfEff(set, try set.allocateIno(root, false, false, 900, .file));
    try testing.expect(l2 != l1 and l2 != off);
    try testing.expectEqual(g0 + 1, routing_slots.mint_window.stats.mint_window_grants.load());
    // A second caller: its first decision opens its window beside the
    // active creator, on a tree no window claims.
    const t2 = slotOfEff(set, try set.allocateIno(root, false, false, 1002, .file));
    try testing.expectEqual(g0 + 2, routing_slots.mint_window.stats.mint_window_grants.load());
    try testing.expect(t2 != off);
    // The creator's census skips both windows' trees.
    for (0..4) |_| {
        const l = slotOfEff(set, try set.allocateIno(root, false, false, 900, .file));
        try testing.expect(l != t2 and l != off);
    }
    // Caller 1002 from another thread (the rank moved CPUs) stays in its
    // window's tree; a third caller on THIS thread (two ranks one drain
    // worker serves) takes a tree of its own and stays there.
    const Other = struct {
        fn run(st: *MetaSet, r: u64, out: *[8]u16, err: *?anyerror) void {
            for (out) |*o| o.* = route_table.slotOf(st.globalIno(st.allocateIno(r, false, false, 1002, .file) catch |e| {
                err.* = e;
                return;
            }));
        }
    };
    var moved: [8]u16 = undefined;
    var err: ?anyerror = null;
    const th = try std.Thread.spawn(.{}, Other.run, .{ set, root, &moved, &err });
    th.join();
    if (err) |e| return e;
    for (moved) |sl| try testing.expectEqual(t2, sl);
    const t3 = slotOfEff(set, try set.allocateIno(root, false, false, 1003, .file));
    try testing.expect(t3 != t2 and t3 != off);
    for (0..8) |_| try testing.expectEqual(t3, slotOfEff(set, try set.allocateIno(root, false, false, 1003, .file)));
    try testing.expectEqual(g0 + 3, routing_slots.mint_window.stats.mint_window_grants.load());
}

test "the mint window across a remount (decision 0003 §BV): eight threads create under the root at once — their mints in windows on distinct rotor trees — then a clean close and a reopen, and eight threads create again: no ino twice, every first-session file present after the remount, the windows' trees past every travelled ino" {
    const gpa = testing.allocator;
    const s = try TestSet.create(gpa, "mint-remount", 1);
    defer s.destroy();
    const threads = 8;
    const Creator = struct {
        set: *MetaSet,
        tag: usize,
        n: usize,
        out: std.ArrayList(u64) = .empty,
        failed: ?anyerror = null,

        fn run(self: *@This()) void {
            var nb: [32]u8 = undefined;
            for (0..self.n) |i| {
                const name = std.fmt.bufPrint(&nb, "t{d}-{d}", .{ self.tag, i }) catch unreachable;
                const g = createFile(self.set, inos.ROOT_INO, name) catch |e| {
                    self.failed = e;
                    return;
                };
                self.out.append(testing.allocator, g) catch |e| {
                    self.failed = e;
                    return;
                };
            }
        }
    };
    var first: std.ArrayList(u64) = .empty;
    defer first.deinit(gpa);
    const grants0 = routing_slots.mint_window.stats.mint_window_grants.load();
    for (0..2) |session| {
        const set = try s.open();
        defer set.close();
        const per = set.volumes[0].slots.window_mints + 64;
        var cs: [threads]Creator = undefined;
        for (&cs, 0..) |*c, t| c.* = .{ .set = set, .tag = session * threads + t, .n = per };
        defer for (&cs) |*c| c.out.deinit(gpa);
        var ths: [threads]std.Thread = undefined;
        for (&ths, &cs) |*th, *c| th.* = try std.Thread.spawn(.{}, Creator.run, .{c});
        for (ths) |th| th.join();
        for (&cs) |*c| {
            if (c.failed) |e| return e;
            try testing.expectEqual(per, c.out.items.len);
        }
        if (session == 1) for (first.items) |g| try testing.expect(try present(set, g));
        var all: std.ArrayList(u64) = .empty;
        defer all.deinit(gpa);
        try all.appendSlice(gpa, first.items);
        for (&cs) |*c| try all.appendSlice(gpa, c.out.items);
        std.mem.sort(u64, all.items, {}, std.sort.asc(u64));
        for (all.items[1..], 1..) |g, i| if (g == all.items[i - 1]) {
            std.debug.print("session {d}: ino {d} (slot {d}) minted twice\n", .{ session, g, route_table.slotOf(g) });
            return error.TestUnexpectedResult;
        };
        if (session == 0) for (&cs) |*c| try first.appendSlice(gpa, c.out.items);
    }
    // Both sessions ran their creates in windows (a refill each).
    try testing.expect(routing_slots.mint_window.stats.mint_window_grants.load() >= grants0 + 2 * threads);
}

test "a two-member set: every member's writer claim refreshes on the beat and is deleted at the clean dismount (red: member 1's claim sat at its local ino 1 — slot 0's keyspace, hosted by member 0 alone — so its armed door refused every refresh and the dismount's delete SlotBusy; the claim went stale under a live writer and outlived the unmount)" {
    const gpa = testing.allocator;
    const s = try TestSet.create(gpa, "claims", 2);
    defer s.destroy();
    const pool = try uring_fs.Pool.init(gpa, uring_fs.Config.resolve(&s.env, derive.Facts.probe()));
    defer pool.deinit();
    {
        const set = try s.open();
        defer set.close();
        for (set.volumes) |be| {
            const beats0 = guard.stats.heartbeats.load();
            const fails0 = guard.stats.heartbeat_failures.load();
            be.guard.?.beat();
            try testing.expectEqual(fails0, guard.stats.heartbeat_failures.load());
            try testing.expect(guard.stats.heartbeats.load() > beats0);
        }
    }
    for (s.pathList()) |p| {
        const raw = try guard.gate.probeClaim(gpa, s.io(), &s.env, pool, p);
        defer if (raw) |r| gpa.free(r);
        try testing.expect(raw == null);
    }
}

test "a child of a directory this mount leases outside the native slot mints on the directory's member (charter §5.5's affinity ranks above §5.4's round-robin): 100 creates under such a directory mint ZERO cross-owner intents — each a single-member entry, no intent barriers; the root's children still spread over both members" {
    const gpa = testing.allocator;
    const s = try TestSet.create(gpa, "home", 2);
    defer s.destroy();
    const set = try s.open();
    defer set.close();
    var nb: [16]u8 = undefined;
    // The root's children (the native slot) spread: both members take some.
    var dirs: [8]u64 = undefined;
    var on: [2]u32 = @splat(0);
    for (&dirs, 0..) |*d, i| {
        d.* = try createChild(set, inos.ROOT_INO, try std.fmt.bufPrint(&nb, "d{d}", .{i}), economy.S.IFDIR | 0o755);
        on[set.volumeIndexOfSlot(route_table.slotOf(d.*))] += 1;
    }
    try testing.expect(on[0] > 0 and on[1] > 0);
    // Under each directory: every child on the directory's member, no intent.
    const minted0 = xv.intent.stats.xv_cross_owner_intents_minted.load();
    for (dirs, 0..) |d, di| {
        const home = set.volumeIndexOfSlot(route_table.slotOf(d));
        for (0..100 / dirs.len + 1) |i| {
            const c = try createFile(set, d, try std.fmt.bufPrint(&nb, "f{d}.{d}", .{ di, i }));
            try testing.expectEqual(home, set.volumeIndexOfSlot(route_table.slotOf(c)));
            try testing.expect(try present(set, c));
        }
    }
    try testing.expectEqual(minted0, xv.intent.stats.xv_cross_owner_intents_minted.load());
}

fn laneBarriers(set: *MetaSet) [2]u64 {
    var out: [2]u64 = undefined;
    for (&out, set.volumes[0..2]) |*o, be| {
        const snap = be.laneSnapshot();
        o.* = snap.barriers_coalesced + snap.barriers_strict;
    }
    return out;
}

/// One more entry naming `global` (its inode record re-put as it stands).
fn touchInode(set: *MetaSet, global: u64) !void {
    const k = key.inodeKey(set.effectiveIno(global));
    const v = (try set.lookup(testing.allocator, key.TREE_INODES, &k)) orelse return error.Absent;
    defer testing.allocator.free(v);
    const t = try set.begin(@src());
    t.stagePut(key.TREE_INODES, &k, v) catch |e| {
        set.abandon(t);
        return e;
    };
    try set.commit(t);
}

test "an fsync of one inode barriers only the members holding its un-durable entries (decision 0003 §CH; RED under the control arm `set`: member 1 barriered for a file whose every entry is on member 0): the file's entry → member 0 alone; again with nothing new → no member; a directory made on member 1 (the dentry chain's) → member 1 alone; the every-member point asks each member holding anything, at once" {
    const gpa = testing.allocator;
    const s = try TestSet.create(gpa, "fsync-scope", 2);
    defer s.destroy();
    // Only the requests barrier (the cadence's own barrier parked).
    try s.env.put("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "600000");
    for ([_][]const u8{ "set", "ino" }) |arm| {
        try s.env.put("SQUEEZEFS_META_FSYNC_SCOPE", arm);
        const shipped = std.mem.eql(u8, arm, "ino");
        const set = try s.open();
        defer set.close();
        var nb: [16]u8 = undefined;
        // A directory on each member (the root's children spread).
        var home: [2]?u64 = .{ null, null };
        var i: usize = 0;
        while (home[0] == null or home[1] == null) : (i += 1) {
            const d = try createChild(set, inos.ROOT_INO, try std.fmt.bufPrint(&nb, "{s}{d}", .{ arm, i }), economy.S.IFDIR | 0o755);
            const v = set.volumeIndexOfSlot(route_table.slotOf(d));
            if (home[v] == null) home[v] = d;
        }
        const f = try createFile(set, home[0].?, "f");
        try testing.expectEqual(@as(route_table.VolumeIndex, 0), set.volumeIndexOfSlot(route_table.slotOf(f)));
        const ino = set.effectiveIno(f);
        try set.fsync();
        // 1. The file's own entry, on member 0.
        try touchInode(set, f);
        var b0 = laneBarriers(set);
        try set.fsyncIno(ino);
        var b1 = laneBarriers(set);
        std.debug.print("\nfsync scope {s}: the file's entry: member barriers +{d} +{d}\n", .{ arm, b1[0] - b0[0], b1[1] - b0[1] });
        try testing.expectEqual(@as(u64, 1), b1[0] - b0[0]);
        try testing.expectEqual(@as(u64, if (shipped) 0 else 1), b1[1] - b0[1]);
        // 2. Nothing new: nothing owed.
        b0 = b1;
        try set.fsyncIno(ino);
        b1 = laneBarriers(set);
        try testing.expectEqual(@as(u64, if (shipped) 0 else 1), b1[0] - b0[0]);
        try testing.expectEqual(@as(u64, if (shipped) 0 else 1), b1[1] - b0[1]);
        // 3. A directory made on member 1: a dentry chain any file's name
        //    may hang from — the file's fsync asks member 1 for it.
        const sub = try createChild(set, home[1].?, "sub", economy.S.IFDIR | 0o755);
        try testing.expectEqual(@as(route_table.VolumeIndex, 1), set.volumeIndexOfSlot(route_table.slotOf(sub)));
        b0 = b1;
        try set.fsyncIno(ino);
        b1 = laneBarriers(set);
        try testing.expectEqual(@as(u64, if (shipped) 0 else 1), b1[0] - b0[0]);
        try testing.expectEqual(@as(u64, 1), b1[1] - b0[1]);
        // 4. The every-member point: an entry on each member, one barrier
        //    each; with nothing new, none.
        try touchInode(set, home[0].?);
        try touchInode(set, home[1].?);
        b0 = b1;
        try set.fsync();
        b1 = laneBarriers(set);
        try testing.expectEqual([2]u64{ b0[0] + 1, b0[1] + 1 }, b1);
        b0 = b1;
        try set.fsync();
        b1 = laneBarriers(set);
        if (shipped) try testing.expectEqual(b0, b1) else try testing.expectEqual([2]u64{ b0[0] + 1, b0[1] + 1 }, b1);
    }
}

fn depthOf(set: *MetaSet, global: u64) !u8 {
    const bytes = (try set.lookup(testing.allocator, key.TREE_INODES, &key.inodeKey(set.effectiveIno(global)))) orelse return error.Absent;
    defer testing.allocator.free(bytes);
    return (try record.InodeValue.decode(bytes)).dirDepth();
}

test "mdtest's shape on a two-member set (decision 0003 §BZ; RED on §AA.1 alone — 100 % on one member): `md/#test-dir.0/mdtest_tree.N.0` at the set's default depth 3 puts the 32 rank directories 16/16 over the members; each rank's files and directories (depth 4) stay on ITS member — 0 intents over the timed phases; every record carries its depth; at depth 0 (the control arm) the whole tree sits on `md`'s member" {
    const gpa = testing.allocator;
    const s = try TestSet.create(gpa, "mdtest-shape", 2);
    defer s.destroy();
    const set = try s.open();
    defer set.close();
    var nb: [32]u8 = undefined;
    const ranks = 32;
    // The default depth, as a `format` without the flag stamps it.
    set.setDirSpreadDepth(dir_place.DEFAULT_DEPTH);
    try testing.expectEqual(@as(u8, 3), set.dirSpreadDepth());
    const md = try createChild(set, inos.ROOT_INO, "md", economy.S.IFDIR | 0o755);
    const tdir = try createChild(set, md, "#test-dir.0", economy.S.IFDIR | 0o755);
    try testing.expectEqual(@as(u8, 1), try depthOf(set, md));
    try testing.expectEqual(@as(u8, 2), try depthOf(set, tdir));
    var rank_dirs: [ranks]u64 = undefined;
    var on: [2]u32 = @splat(0);
    const rotor0 = rstats.routing_dir_place_rotor.load();
    for (&rank_dirs, 0..) |*d, r| {
        d.* = try createChild(set, tdir, try std.fmt.bufPrint(&nb, "mdtest_tree.{d}.0", .{r}), economy.S.IFDIR | 0o755);
        try testing.expectEqual(@as(u8, 3), try depthOf(set, d.*));
        on[set.volumeIndexOfSlot(route_table.slotOf(d.*))] += 1;
    }
    // The setup spreads: the round-robin over two equal members alternates.
    try testing.expectEqual(@as(u32, ranks / 2), on[0]);
    try testing.expectEqual(@as(u32, ranks / 2), on[1]);
    try testing.expectEqual(rotor0 + ranks, rstats.routing_dir_place_rotor.load());
    // The timed phases: every rank's children on the rank's member, no intent.
    const minted0 = xv.intent.stats.xv_cross_owner_intents_minted.load();
    const remote0 = rstats.routing_dir_place_remote.load();
    const local0 = rstats.routing_dir_place_local.load();
    var per_member_children: [2]u32 = @splat(0);
    for (rank_dirs, 0..) |d, r| {
        const home = set.volumeIndexOfSlot(route_table.slotOf(d));
        for (0..4) |i| {
            const sub = try createChild(set, d, try std.fmt.bufPrint(&nb, "dir.mdtest.{d}.{d}", .{ r, i }), economy.S.IFDIR | 0o755);
            try testing.expectEqual(home, set.volumeIndexOfSlot(route_table.slotOf(sub)));
            try testing.expectEqual(@as(u8, 4), try depthOf(set, sub));
            const f = try createFile(set, d, try std.fmt.bufPrint(&nb, "file.mdtest.{d}.{d}", .{ r, i }));
            try testing.expectEqual(home, set.volumeIndexOfSlot(route_table.slotOf(f)));
            try testing.expectEqual(@as(u8, 0), try depthOf(set, f));
            per_member_children[home] += 2;
        }
    }
    try testing.expectEqual(minted0, xv.intent.stats.xv_cross_owner_intents_minted.load());
    try testing.expectEqual(remote0, rstats.routing_dir_place_remote.load());
    try testing.expectEqual(local0 + ranks * 4, rstats.routing_dir_place_local.load());
    try testing.expectEqual(per_member_children[0], per_member_children[1]);
    try testing.expectEqual(set.mints[0].dirs.load() + set.mints[1].dirs.load(), @as(u64, 2 + ranks + ranks * 4));
    // The control arm (`--dir-spread-depth 0` = §AA.1): a second tree
    // sits whole on its top directory's member.
    set.setDirSpreadDepth(0);
    const md0 = try createChild(set, inos.ROOT_INO, "md0", economy.S.IFDIR | 0o755);
    const t0 = try createChild(set, md0, "#test-dir.0", economy.S.IFDIR | 0o755);
    const home0 = set.volumeIndexOfSlot(route_table.slotOf(md0));
    try testing.expectEqual(home0, set.volumeIndexOfSlot(route_table.slotOf(t0)));
    for (0..ranks) |r| {
        const d = try createChild(set, t0, try std.fmt.bufPrint(&nb, "mdtest_tree.{d}.0", .{r}), economy.S.IFDIR | 0o755);
        try testing.expectEqual(home0, set.volumeIndexOfSlot(route_table.slotOf(d)));
        // The depth is recorded whatever D is: a later `set-dir-spread-depth` reads it.
        try testing.expectEqual(@as(u8, 3), try depthOf(set, d));
    }
}

test "the spread depth is the set's dial (decision 0003 §BZ): raised 3 → 10 on a live tree, new directories at depths 4 … 10 take the rotor and depth 11 stays local; a directory made before the field (depth byte 0) roots a subtree that never spreads (its children record 255); a saturated chain stays saturated" {
    const gpa = testing.allocator;
    const s = try TestSet.create(gpa, "spread-dial", 2);
    defer s.destroy();
    const set = try s.open();
    defer set.close();
    var nb: [32]u8 = undefined;
    set.setDirSpreadDepth(3);
    // A chain /l1/l2/…/l12 at D = 3: levels 4 … 12 on level 3's member.
    var chain: [12]u64 = undefined;
    var parent: u64 = inos.ROOT_INO;
    for (&chain, 1..) |*c, lvl| {
        c.* = try createChild(set, parent, try std.fmt.bufPrint(&nb, "l{d}", .{lvl}), economy.S.IFDIR | 0o755);
        try testing.expectEqual(@as(u8, @intCast(lvl)), try depthOf(set, c.*));
        if (lvl > 3) try testing.expectEqual(set.volumeIndexOfSlot(route_table.slotOf(chain[2])), set.volumeIndexOfSlot(route_table.slotOf(c.*)));
        parent = c.*;
    }
    // The dial turned to 10 (the verb's effect at the next mount): new
    // siblings at depths 4 … 10 alternate members, 11 and 12 stay put.
    set.setDirSpreadDepth(10);
    for (chain[2..], 3..) |p, parent_lvl| {
        const lvl = parent_lvl + 1;
        var on: [2]u32 = @splat(0);
        for (0..4) |i| {
            const d = try createChild(set, p, try std.fmt.bufPrint(&nb, "n{d}.{d}", .{ lvl, i }), economy.S.IFDIR | 0o755);
            try testing.expectEqual(@as(u8, @intCast(lvl)), try depthOf(set, d));
            on[set.volumeIndexOfSlot(route_table.slotOf(d))] += 1;
        }
        const parent_home = set.volumeIndexOfSlot(route_table.slotOf(p));
        if (lvl <= 10) {
            try testing.expectEqual(@as(u32, 2), on[0]);
            try testing.expectEqual(@as(u32, 2), on[1]);
        } else {
            try testing.expectEqual(@as(u32, 4), on[parent_home]);
        }
    }
    // The old homes stand: the chain's records are what they were.
    for (chain, 1..) |c, lvl| try testing.expectEqual(@as(u8, @intCast(lvl)), try depthOf(set, c));
    // A legacy directory (no depth byte — made before the field): its
    // children record SATURATED and sit on its member whatever D is.
    const legacy_local = try set.allocateIno(set.effectiveIno(chain[0]), true, false, 0, .{ .dir = .{ .depth = dir_place.SATURATED } });
    const legacy = set.globalIno(legacy_local);
    {
        const t = try set.begin(@src());
        const pv_bytes = (try set.lookup(gpa, key.TREE_INODES, &key.inodeKey(set.effectiveIno(chain[0])))).?;
        defer gpa.free(pv_bytes);
        const pv = try record.InodeValue.decode(pv_bytes);
        _ = try economy.stageCreate(t, .{ .name_key = set.nameKey(), .now = 1000, .parent = set.effectiveIno(chain[0]), .parent_value = pv, .name = "legacy", .mode = economy.S.IFDIR | 0o755, .uid = 0, .gid = 0, .local_ino = legacy_local, .global_ino = legacy });
        try set.commit(t);
    }
    try testing.expectEqual(@as(u8, 0), try depthOf(set, legacy));
    const legacy_home = set.volumeIndexOfSlot(route_table.slotOf(legacy));
    set.setDirSpreadDepth(dir_place.DEPTH_MAX);
    var under = legacy;
    for (0..3) |i| {
        under = try createChild(set, under, try std.fmt.bufPrint(&nb, "u{d}", .{i}), economy.S.IFDIR | 0o755);
        try testing.expectEqual(dir_place.SATURATED, try depthOf(set, under));
        try testing.expectEqual(legacy_home, set.volumeIndexOfSlot(route_table.slotOf(under)));
    }
}

test "a refs range always merges every slot tree: one volume's references owned by inos in three slots on two members come back together, in key order, through the volume range and through one block's range" {
    const gpa = testing.allocator;
    const s = try TestSet.create(gpa, "refs", 2);
    defer s.destroy();
    const set = try s.open();
    defer set.close();
    const vol: u64 = 0xBEEF;
    // Owners in slot 0 (member 0, native), slot 1 (member 1) and slot 2
    // (member 0, guest): a `+ref` each on blocks 7 and 9.
    const owners = [_]u64{ 2, route_table.effectiveIno(inos.makeGlobalIno(2, 1, inos.ROUTING_WIDTH)), route_table.effectiveIno(inos.makeGlobalIno(2, 2, inos.ROUTING_WIDTH)) };
    for (owners, 0..) |o, i| {
        const t = try set.begin(@src());
        try t.stagePut(key.TREE_BLOCK_REFS, &key.blockRefKey(vol, 7, o, 0), "r");
        try t.stagePut(key.TREE_BLOCK_REFS, &key.blockRefKey(vol, 9, o, @intCast(i)), "r");
        try set.commit(t);
    }
    var kvs: std.ArrayList(tree.KeyValue) = .empty;
    defer tree.deinitKeyValues(gpa, &kvs);
    const vr = key.volumeRange(vol);
    try set.range(gpa, key.TREE_BLOCK_REFS, vr.lo.slice(), vr.hi.slice(), 100, &kvs);
    try testing.expectEqual(@as(usize, 6), kvs.items.len);
    for (kvs.items[1..], 1..) |kv, i| try testing.expect(key.lessThan(kvs.items[i - 1].key, kv.key));
    tree.deinitKeyValues(gpa, &kvs);
    kvs = .empty;
    const br = key.blockRange(vol, 7);
    try set.range(gpa, key.TREE_BLOCK_REFS, br.lo.slice(), br.hi.slice(), 100, &kvs);
    try testing.expectEqual(@as(usize, 3), kvs.items.len);
    for (kvs.items, owners) |kv, o| try testing.expectEqual(o, (try key.BlockRefKey.decode(kv.key)).ownerIno());
}

test "a read-only open of a LIVE set beside a commit storm at a fast checkpoint cadence never refuses Corrupt (the ONLINE FSCK row: `scrub --online`'s storage-trust open answered 'cannot open the set read-only (Corrupt)' about half the time under load)" {
    const gpa = testing.allocator;
    const s = try TestSet.create(gpa, "ro-storm", 1);
    defer s.destroy();
    try s.env.put("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "10");
    const set = try s.open();
    defer set.close();
    const Storm = struct {
        fn run(ms: *MetaSet, stop: *std.atomic.Value(bool)) void {
            var value: [200]u8 = undefined;
            var i: u64 = 0;
            while (!stop.load(.acquire)) : (i += 1) {
                @memset(&value, @truncate(i));
                const t = ms.begin(@src()) catch return;
                // 4,000 keys rewritten round-robin: leaves split, compact and retire.
                t.stagePut(key.TREE_INODES, &key.inodeKey(1000 + ((i *% 7919) % 20_000)), &value) catch {
                    ms.abandon(t);
                    return;
                };
                ms.commit(t) catch return;
            }
        }
    };
    var stop: std.atomic.Value(bool) = .init(false);
    var storms: [4]std.Thread = undefined;
    for (&storms) |*th| th.* = try std.Thread.spawn(.{}, Storm.run, .{ set, &stop });
    defer {
        stop.store(true, .release);
        for (storms) |th| th.join();
    }
    var refused: usize = 0;
    for (0..24) |_| {
        const reader = MetaSet.open(gpa, s.io(), s.pathList(), .{ .env = &s.env, .cache_budget_bytes = 64 << 20, .cpus = 4, .read_only = true }) catch |e| {
            std.debug.print("\nread-only open refused: {s}\n", .{@errorName(e)});
            if (@errorReturnTrace()) |tr| std.debug.dumpErrorReturnTrace(tr);
            refused += 1;
            continue;
        };
        reader.close();
    }
    try testing.expectEqual(@as(usize, 0), refused);
}

test "a `-o ro` reader of a symmetric set has no slot plane: the filesystem's handover binds (the parent memo, the step-0 times drain) are no-ops on it — never a dereference of the unarmed plane (rung 10 item 4's regression: the -o ro row's reader aborted at bindTimesDrain)" {
    const gpa = testing.allocator;
    const s = try TestSet.create(gpa, "ro-binds", 1);
    defer s.destroy();
    {
        const set = try s.open();
        set.close();
    }
    const reader = try MetaSet.open(gpa, s.io(), s.pathList(), .{ .env = &s.env, .cache_budget_bytes = 64 << 20, .cpus = 4, .read_only = true });
    defer reader.close();
    try testing.expect(reader.isReader());
    const Probe = struct {
        fn parentOf(_: *anyopaque, _: u64) ?u64 {
            return null;
        }
        fn drainSlot(_: *anyopaque, _: u16) u64 {
            return 0;
        }
    };
    var marker: u8 = 0;
    reader.bindParentMemo(.{ .ctx = &marker, .parentOf = &Probe.parentOf });
    reader.bindTimesDrain(.{ .ctx = &marker, .drainSlot = &Probe.drainSlot });
}

test "a tx spanning two members never reaches a member: refused EIO on the tripwire, nothing written; a one-member tx of a guest slot lands" {
    const gpa = testing.allocator;
    const s = try TestSet.create(gpa, "cross", 3);
    defer s.destroy();
    const set = try s.open();
    defer set.close();
    {
        const t = try set.begin(@src());
        try t.stagePut(key.TREE_INODES, &key.inodeKey(route_table.effectiveIno(inos.makeGlobalIno(2, 0, inos.ROUTING_WIDTH))), "a");
        try t.stagePut(key.TREE_INODES, &key.inodeKey(route_table.effectiveIno(inos.makeGlobalIno(2, 1, inos.ROUTING_WIDTH))), "b");
        const r0 = rstats.routing_split_refused.load();
        rstats.routing_split_refused.resetLogBudgetForTest();
        log.testing.expectErrorLogs(1);
        try testing.expectError(error.Corrupt, set.commit(t));
        try log.testing.expectAllFired();
        try testing.expectEqual(r0 + 1, rstats.routing_split_refused.load());
        try testing.expect((try set.lookup(gpa, key.TREE_INODES, &key.inodeKey(route_table.effectiveIno(inos.makeGlobalIno(2, 1, inos.ROUTING_WIDTH))))) == null);
    }
    {
        const child = route_table.effectiveIno(inos.makeGlobalIno(5, 1, inos.ROUTING_WIDTH));
        const t = try set.begin(@src());
        try t.stagePut(key.TREE_INODES, &key.inodeKey(child), &(record.InodeValue{ .mode = 0o100644, .nlink = 1 }).encode());
        try set.commit(t);
        const got = (try set.lookup(gpa, key.TREE_INODES, &key.inodeKey(child))).?;
        defer gpa.free(got);
    }
}

test "the try-only read contract through the SET face (decision 0003 §AB.1): a lookup, a view and a range whose leaf the cache does not hold answer WouldBlock — never Corrupt (the same answer a try-only descent gives from an SMO's swap window, which it never waits out) — so a caller that keys on WouldBlock defers instead of failing (the FORGET corpse judgement: WouldBlock ⇒ gathered for the cadence; Corrupt → InputOutput was judged 'live' and the corpse dropped — the mdtest row: ONE unlinked inode of 640,033 never destroyed by the live daemon under 4,882 compactions, found by the next mount's sweep)" {
    const gpa = testing.allocator;
    const s = try TestSet.createClass(gpa, "try-only", 1, .single_writer);
    defer s.destroy();
    // 64 records of 4000 B: several 64 KiB leaves under an interior root,
    // so after the reopen the root (pinned at open) is resident and no
    // leaf is.
    const N: u64 = 64;
    const value = try gpa.alloc(u8, 4000);
    defer gpa.free(value);
    {
        const set = try s.open();
        for (0..N) |i| {
            @memset(value, @truncate(i));
            const t = try set.begin(@src());
            try t.stagePut(key.TREE_INODES, &key.inodeKey(1000 + i), value);
            try set.commit(t);
        }
        try set.checkpointNow();
        set.close();
    }
    const set = try s.open();
    defer set.close();
    const first = key.inodeKey(1000);
    const last = key.inodeKey(1000 + N - 1);
    const Visit = struct {
        fn len(_: void, v: []const u8) usize {
            return v.len;
        }
    };
    var kvs: std.ArrayList(tree.KeyValue) = .empty;
    defer tree.deinitKeyValues(gpa, &kvs);
    {
        node_cache.try_only = true;
        defer node_cache.try_only = false;
        try testing.expectError(error.WouldBlock, set.lookup(gpa, key.TREE_INODES, &last));
        try testing.expectError(error.WouldBlock, set.lookupView(gpa, key.TREE_INODES, &last, usize, {}, Visit.len));
        try testing.expectError(error.WouldBlock, set.range(gpa, key.TREE_INODES, &first, &last, 8, &kvs));
    }
    // The blocking read pages the leaf in; the try-only repeat serves it.
    const got = (try set.lookup(gpa, key.TREE_INODES, &last)).?;
    defer gpa.free(got);
    try testing.expectEqual(@as(usize, 4000), got.len);
    node_cache.try_only = true;
    defer node_cache.try_only = false;
    const again = (try set.lookup(gpa, key.TREE_INODES, &last)).?;
    defer gpa.free(again);
    try testing.expectEqualSlices(u8, got, again);
    try testing.expectEqual(@as(?usize, 4000), try set.lookupView(gpa, key.TREE_INODES, &last, usize, {}, Visit.len));
}
