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
const guard = @import("guard.zig");
const routing = @import("routing.zig");
const routing_slots = @import("sym.zig").slots;
const xv = @import("sym.zig").xv;
const dlm = @import("dlm.zig");
const router = @import("../data/router.zig");

const route_table = routing.route_table;
const discovery = routing.discovery;
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
    mint_rr: std.atomic.Value(u64) = .init(0),
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
                .joined => try KvBackend.openJoined(gpa, io, paths[oi], be_opts, opts.join.?),
                .reader => try KvBackend.openReader(gpa, io, paths[oi], be_opts),
            };
            opened += 1;
        }
        const rotors = try gpa.alloc(router.MintRotor, paths.len);
        errdefer gpa.free(rotors);
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
            .set_uuid = disc.set_uuid,
            .hash_seed = disc.hash_seed,
            .name_key = .init(disc.hash_seed),
            .node_size = disc.node_size,
            .posture = posture,
        };
        errdefer self.route.deinit();
        // A joiner converges nothing: the stamps are the manager's.
        if (!opts.read_only and posture == .manager) try self.convergeDualClaims(disc.dual_claims);
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
        const gpa = self.gpa;
        if (!self.closed) {
            self.closed = true;
            for (self.volumes) |be| be.close();
        }
        if (self.tee) |t| t.deinit();
        self.route.deinit();
        gpa.free(self.rotors);
        gpa.free(self.volumes);
        if (self.pool_owned) self.pool.deinit();
        gpa.destroy(self);
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

    /// `fsync`: every member's trailing barrier.
    pub fn fsync(self: *MetaSet) Error!void {
        for (self.volumes) |be| try be.fsync();
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

    /// Mint a child of `parent_eff` (charter §5.5 "Slot acquisition and
    /// minting" first: a child of a directory this mount leases outside the
    /// native slot stays on its parent's member — `Plane.homesChildren`,
    /// decision 0003 §AA.1; else charter §5.4 "Mint spread": round-robin
    /// over the healthy members. On an armed member the plane's decision —
    /// `gather`, the parent's slot under the affinity ceiling, else the
    /// rotor tree with the most headroom, an overflow rotor past the cap —
    /// else the member's rotor). `prefer_parent` keeps the mint on the
    /// parent's member (a rename's whiteout: the op stays on two members at
    /// most). The child's slot takes its door token at the commit (the
    /// record names it). Returns the child's EFFECTIVE (key) ino;
    /// `globalIno` inverts it.
    pub fn allocateIno(self: *MetaSet, parent_eff: u64, prefer_parent: bool, gather: bool) Error!u64 {
        if (self.posture == .reader) return error.PartitionRefused;
        const parent_v = self.route.volumeOfEffective(parent_eff);
        const parent_slot = route_table.slotOfEffective(parent_eff);
        const home = prefer_parent or self.volumes.len == 1 or self.volumes[parent_v].slots.homesChildren(parent_slot);
        var hb: [route_table.MAX_VOLUMES]?u32 = undefined;
        const v: route_table.VolumeIndex = if (home) parent_v else @intCast(router.pickMintVolume(parent_v, self.healths(hb[0..self.volumes.len]), &self.mint_rr));
        if (v != parent_v) rstats.routing_mint_volume_picks.inc();
        const rr = self.slot_rr.fetchAdd(1, .monotonic);
        const slot = self.volumes[v].slots.mintSlot(parent_slot, v == parent_v, gather, rr) orelse self.rotors[v].pickSlot();
        const be = self.volumeOfSlot(slot);
        const local = be.allocateInoInSlot(slot) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            else => error.Io,
        };
        return route_table.effectiveIno(inos.makeGlobalIno(local, slot, inos.ROUTING_WIDTH));
    }

    /// The filesystem's directory-parent memo for the handover's subtree
    /// credit (charter §5.5 "`ops_h` counts the holder's namespace ops on
    /// the slot's SUBTREE"): bound on every member's plane.
    pub fn bindParentMemo(self: *MetaSet, memo: routing_slots.handover.ParentMemo) void {
        if (self.posture == .reader) return;
        for (self.volumes) |be| be.slots.bindMemo(memo);
    }

    /// The mount's slot-move epoch into every member's lease table
    /// (decision 0003 §X.1). A `-o ro` reader has no slot plane: nothing
    /// it holds moves, its move gate re-polls.
    pub fn bindMoves(self: *MetaSet, m: *routing_slots.moves.Moves) void {
        if (self.posture == .reader) return;
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
        for (self.volumes) |be| be.slots.bindTimesDrain(drain);
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
        try w.print("{{\"routing_volumes\":{d},\"routing_width\":{d},\"routing_set_epoch\":{d},\"routing_closed_slot\":{?d},\"mount_posture\":\"{s}\",\"volumes\":[", .{ self.volumes.len, route_table.W, self.setEpoch(), self.closedSlot(), @tagName(self.posture) });
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
            try w.print(",\"heap_total_extents\":{d},\"heap_free_extents\":{d},\"heap_pending_free\":{d},\"resident_leaves\":{d},\"resident_interior\":{d}", .{ be.alloc.totalExtents(), be.alloc.freeExtents(), be.alloc.pendingCount(), census.leaves, census.interior });
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

/// `createFile` for any mode (a directory parent for the mint-placement rows).
pub fn createChild(set: *MetaSet, parent_global: u64, name: []const u8, mode: u32) !u64 {
    const parent = set.effectiveIno(parent_global);
    const pv_bytes = (try set.lookup(set.gpa, key.TREE_INODES, &key.inodeKey(parent))) orelse return error.ParentMissing;
    defer set.gpa.free(pv_bytes);
    const pv = try record.InodeValue.decode(pv_bytes);
    const child = try set.allocateIno(parent, false, false);
    const global = set.globalIno(child);
    const parent_slot = route_table.slotOfEffective(parent);
    if (set.volumeIndexOfSlot(parent_slot) != set.volumeIndexOfSlot(route_table.slotOf(global))) {
        var locks = try dlm.Manager.init(set.gpa, 64);
        defer locks.deinit();
        const bound = try xv.set_bind.Bound.create(set.gpa, set.io, set, &locks, set.env);
        defer bound.destroy(set.gpa);
        const plan = xv.steps.planCreate(.{ .parent = parent_global, .name = name, .child = global, .mode = mode, .uid = 0, .gid = 0, .now = 1000 });
        const scope = bound.plane.mintScope();
        var held = try bound.plane.acquirePlanGuards(&plan, scope, false);
        defer held.release(set.gpa);
        const done = try bound.plane.execute(&plan, scope, 1000);
        if (done.refused) |e| return e;
        return global;
    }
    const t = try set.begin(@src());
    _ = economy.stageCreate(t, .{ .name_key = set.nameKey(), .now = 1000, .parent = parent, .parent_value = pv, .name = name, .mode = mode, .uid = 0, .gid = 0, .local_ino = child, .global_ino = global }) catch |e| {
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
