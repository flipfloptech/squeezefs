//! THE FILESYSTEM OVER A STRIPED DIRECTORY (charter §5.5 "Directory
//! striping"; `docs/porting/sym_tokens_intents_striping.md` §4.4–§4.7):
//! the striped PRELUDE every dentry op of `kv_fs` runs — the directory's
//! verdict (plain / striped with its map / a stripe), the ROUTE of a name
//! to its stripe (`hash54 % K`), the lazy RE-HOME of a name still in `D`'s
//! own tree before any mutation, the dying-parent refusal, the K-way
//! merged `readdir`, the `stat` fold, the one-intent `rmdir`, the explicit
//! flip COMMAND (`setfattr -n user.squeezefs.stripes -v K`) and the
//! census-triggered flip's hand-back, the background migration on the
//! cadence tick, and the plane's striping FACE (`isDomain`, the
//! `SupplyStripeInos` served side, the shipped command).
//!
//! The verdict cache: a map is IMMUTABLE until its directory's `rmdir`
//! (the directory is never re-striped, an ino never reused), so a
//! positive entry stands; a PLAIN verdict is the holder's negative hint
//! keyed by the slot's lease generation ("only the holder flips a
//! directory it holds, so its own flip is the one invalidation; a stale
//! lease stamp reads as no hint") — a foreign directory's verdict is
//! re-read off its token every time (RAM while the token is cached). A
//! stripe is known by its RECORD's flag (`record.INODE_FLAG_STRIPE`),
//! wherever it is read. Generic over the filesystem (`kv_fs.KvFs`);
//! portable.

const std = @import("std");
const Allocator = std.mem.Allocator;
const Io = std.Io;
const counters = @import("../../core/counters.zig");
const errors = @import("../../core/errors.zig");
const clock = @import("../../core/clock.zig");
const log = @import("../../core/log.zig");
const sync = @import("../../sync/sync.zig");
const key = @import("../../meta/key.zig");
const record = @import("../../meta/record.zig");
const economy = @import("../../meta/economy.zig");
const node_cache = @import("../../meta/node_cache.zig");
const route_table = @import("../../meta/routing/route_table.zig");
const sym_xv = @import("../../meta/sym/xv.zig");

const stripe = sym_xv.stripe;
const ops = sym_xv.ops;
const Error = errors.Error;
pub const StripeMap = stripe.StripeMap;
pub const InodeValue = record.InodeValue;

/// Positive maps this mount keeps: `W / 16` — one hot shared directory
/// per 16 routing slots; a map is 520 B, the cap ≈ 2 MiB. Past it the
/// cache is flushed whole (a re-read is one marker probe per op).
pub const MAPS_CACHE_MAX: usize = route_table.W / 16;
/// The holder's negative hints: one per routing slot (`W`), 16 B each;
/// flushed whole at the cap.
pub const PLAIN_CACHE_MAX: usize = route_table.W;

pub const stats = struct {
    /// Prelude verdicts served off the cache / read off the records.
    pub var dir_stripe_verdict_hits: counters.ShardedCounter = .{};
    pub var dir_stripe_verdict_reads: counters.ShardedCounter = .{};
    /// Names routed to a stripe by a lookup / a mutation / a create.
    pub var dir_stripe_routed_lookups: counters.ShardedCounter = .{};
    pub var dir_stripe_routed_mutations: counters.ShardedCounter = .{};
    /// Explicit flips served here (the command, local or shipped).
    pub var dir_stripe_explicit_flips: counters.ShardedCounter = .{};
    /// Migration rounds the cadence ran and directories whose
    /// `migrating` marker cleared.
    pub var dir_stripe_migration_rounds: counters.ShardedCounter = .{};
    pub var dir_stripe_migrations_done: counters.ShardedCounter = .{};
    /// A verdict cache flushed whole at its cap.
    pub var dir_stripe_cache_flushes: counters.ShardedCounter = .{};
    /// A verdict-cache entry (or a whole map's install) refused for
    /// memory: nothing cached, the next verdict re-reads the records.
    pub var dir_stripe_cache_drops: counters.ShardedCounter = .{};
    /// A try-only verdict (the drain worker's probe) that met the cache
    /// lock held: `WouldBlock`, the lane re-asks the op.
    pub var dir_stripe_verdict_try_refusals: counters.ShardedCounter = .{};
};

/// What the prelude knows of a directory.
pub const Verdict = union(enum) {
    plain,
    striped: StripeMap,
    /// A stripe of `owner` (0 = the owning directory is not known here —
    /// a stripe whose slot arrived by handover or recovery).
    stripe: u64,
};

pub fn Stripes(comptime Fs: type) type {
    return struct {
        const Self = @This();
        pub const Store = Fs.StoreType;
        pub const Plane = Fs.XvPlane;
        pub const Core = stripe.Striping(Store);

        gpa: Allocator,
        io: Io,
        fs: *Fs,
        plane: *Plane,
        core: Core,
        /// The verdict cache's lock: the verdict's probe (two per create,
        /// on every FUSE worker) reads SHARED; installs, forgets and the
        /// miss path's inserts write. One mutex here refused a third of the
        /// drain workers' inline probes (`dir_stripe_verdict_try_refusals`
        /// 0.32–0.39 per create on mdtest -np 32) into handler-fiber
        /// demotions and parked the rest behind each other.
        lock: sync.RwLock = .init,
        maps: std.AutoHashMapUnmanaged(u64, StripeMap) = .empty,
        /// stripe → its directory (the ancestry walk's hop; 0 = unknown).
        stripe_of: std.AutoHashMapUnmanaged(u64, u64) = .empty,
        /// dir → the lease generation its plain verdict was read under.
        plain: std.AutoHashMapUnmanaged(u64, u32) = .empty,
        /// Directories whose lazy migration the cadence drives.
        migrating: std.ArrayList(u64) = .empty,

        pub fn create(gpa: Allocator, io: Io, fs: *Fs, plane: *Plane, k: u8, floor: stripe.Floor) Allocator.Error!*Self {
            const self = try gpa.create(Self);
            errdefer gpa.destroy(self);
            self.* = .{ .gpa = gpa, .io = io, .fs = fs, .plane = plane, .core = undefined };
            self.core = try Core.init(plane, .{ .ctx = self, .page = &pageFn, .inode = &inodeFn }, .{ .ctx = self, .mint = &mintFn }, k, floor);
            self.core.flipped = .{ .ctx = self, .note = &flippedFn };
            // The caches grow to their caps on demand; the first reserve
            // is a rotor's width of directories (`MINT_SPREAD`) for the
            // maps and the census' directory bound for the hints.
            try self.maps.ensureTotalCapacity(gpa, stripe.K_MAX);
            try self.plain.ensureTotalCapacity(gpa, stripe.CENSUS_DIRS_MAX);
            return self;
        }

        pub fn destroy(self: *Self) void {
            self.core.deinit();
            self.maps.deinit(self.gpa);
            self.stripe_of.deinit(self.gpa);
            self.plain.deinit(self.gpa);
            self.migrating.deinit(self.gpa);
            self.gpa.destroy(self);
        }

        /// The plane's faces, bound before the wire serves.
        pub fn face(self: *Self) ops.StripingFace {
            return .{ .ctx = self, .isDomain = &isDomainFn, .supply = &supplyFn, .flip = &flipFn };
        }

        pub fn servedInsertSink(self: *Self) ops.ServedInsertSink {
            return self.core.servedInsertSink();
        }

        // ---- the directory source behind the core ----

        fn pageFn(ctx: *anyopaque, arena: Allocator, dir: u64, after: u64, max: usize, out: *std.ArrayList(stripe.DirRec)) Error!bool {
            const self: *Self = @ptrCast(@alignCast(ctx));
            return self.fs.dirPage(arena, try Fs.localOf(dir), after, max, out);
        }

        fn inodeFn(ctx: *anyopaque, ino: u64) Error!?InodeValue {
            const self: *Self = @ptrCast(@alignCast(ctx));
            return self.fs.readInodeOpt(try Fs.localOf(ino));
        }

        /// A stripe ino in this mount's slot on `dir`'s volume: the
        /// store's mint with the parent preferred (the holder's affinity
        /// slot or its rotor on that volume; a creator has no affinity
        /// to a directory it does not hold, so its rotor).
        fn mintFn(ctx: *anyopaque, dir: u64) ?u64 {
            const self: *Self = @ptrCast(@alignCast(ctx));
            const local = self.fs.store.allocateIno(Fs.localOf(dir) catch return null, true, false, 0) catch return null;
            return Fs.globalOf(local);
        }

        // ---- the plane's face ----

        fn isDomainFn(ctx: *anyopaque, ino: u64) bool {
            const self: *Self = @ptrCast(@alignCast(ctx));
            const v = self.verdictOf(ino) catch return false;
            return v != .plain;
        }

        fn supplyFn(ctx: *anyopaque, dir: u64, attrs: ops.StripeAttrs, indices: []const u8, out: []u64) usize {
            const self: *Self = @ptrCast(@alignCast(ctx));
            var n: usize = 0;
            while (n < indices.len) : (n += 1) out[n] = mintFn(ctx, dir) orelse break;
            if (n == 0) return 0;
            stripe.writeStripeRecords(Store, self.fs.store, out[0..n], attrs) catch return 0;
            self.lock.lockUncancelable(self.io);
            defer self.lock.unlock(self.io);
            for (out[0..n]) |s| self.stripe_of.put(self.gpa, s, dir) catch stats.dir_stripe_cache_drops.inc();
            return n;
        }

        fn flipFn(ctx: *anyopaque, dir: u64, k: u8) Error!void {
            const self: *Self = @ptrCast(@alignCast(ctx));
            _ = try self.flipExplicit(dir, k);
        }

        /// The census-triggered flip's hand-back: the map installed, the
        /// migration queued.
        fn flippedFn(ctx: *anyopaque, map: StripeMap) void {
            const self: *Self = @ptrCast(@alignCast(ctx));
            self.install(map);
        }

        fn install(self: *Self, map: StripeMap) void {
            self.lock.lockUncancelable(self.io);
            defer self.lock.unlock(self.io);
            self.installLocked(map);
        }

        /// The cache lock as the caller may take it. A try-only reader
        /// (`node_cache.try_only`: the FUSE drain worker's probe, off a
        /// fiber on the thread that DRIVES its own handler lane) never
        /// queues behind it: a held lock is `WouldBlock` and the lane
        /// re-asks the op. Queued, the worker parks until its tick while
        /// the holder's release goes to the queue's front — a handler
        /// fiber of its own lane, which runs only in the worker's pass:
        /// 2 s per contention and the hot directory's guards held
        /// through it (`docs/evidence/rung-10-mdtest-wedge.md`).
        fn lockCache(self: *Self) Error!void {
            if (node_cache.try_only) {
                if (self.lock.tryLock(self.io)) return;
                stats.dir_stripe_verdict_try_refusals.inc();
                return error.WouldBlock;
            }
            self.lock.lockUncancelable(self.io);
        }

        /// The probe's shared hold — refused only while a WRITER holds the
        /// cache (an install, a forget, a miss's insert).
        fn lockCacheShared(self: *Self) Error!void {
            if (node_cache.try_only) {
                if (self.lock.tryLockShared(self.io)) return;
                stats.dir_stripe_verdict_try_refusals.inc();
                return error.WouldBlock;
            }
            self.lock.lockSharedUncancelable(self.io);
        }

        fn installLocked(self: *Self, map: StripeMap) void {
            if (self.maps.count() >= MAPS_CACHE_MAX) {
                self.maps.clearRetainingCapacity();
                self.stripe_of.clearRetainingCapacity();
                stats.dir_stripe_cache_flushes.inc();
            }
            // All or nothing: a cached map is never re-read, so a part
            // cached without its queue entry would never migrate.
            self.maps.ensureUnusedCapacity(self.gpa, 1) catch return stats.dir_stripe_cache_drops.inc();
            self.stripe_of.ensureUnusedCapacity(self.gpa, map.k) catch return stats.dir_stripe_cache_drops.inc();
            self.migrating.ensureUnusedCapacity(self.gpa, 1) catch return stats.dir_stripe_cache_drops.inc();
            _ = self.plain.remove(map.dir);
            self.maps.putAssumeCapacity(map.dir, map);
            for (map.slice()) |s| self.stripe_of.putAssumeCapacity(s, map.dir);
            if (map.migrating) self.migrating.appendAssumeCapacity(map.dir);
        }

        // ---- the verdict ----

        /// The prelude's question for a directory (or any ino): plain,
        /// striped (its map), or a stripe.
        pub fn verdictOf(self: *Self, global: u64) Error!Verdict {
            return self.verdict(global, .remember);
        }

        /// A miss's plain answer: remembered for a directory that lives on
        /// (its children's ops ask again), or not for an rmdir's victim — a
        /// write would take the cache EXCLUSIVE once per rmdir for an entry
        /// nobody asks again, and every worker's shared probe queued behind
        /// those inserts (mdtest -np 32: 600 µs per rmdir, 560 of it there).
        const PlainMiss = enum { remember, forget };

        fn verdict(self: *Self, global: u64, plain_miss: PlainMiss) Error!Verdict {
            const g = self.plane.localGen(global);
            {
                try self.lockCacheShared();
                defer self.lock.unlockShared(self.io);
                if (self.maps.get(global)) |m| {
                    stats.dir_stripe_verdict_hits.inc();
                    return .{ .striped = m };
                }
                if (self.stripe_of.get(global)) |owner| {
                    stats.dir_stripe_verdict_hits.inc();
                    return .{ .stripe = owner };
                }
                if (g) |gen| if (self.plain.get(global)) |seen| if (seen == gen) {
                    stats.dir_stripe_verdict_hits.inc();
                    return .plain;
                };
            }
            stats.dir_stripe_verdict_reads.inc();
            const local = try Fs.localOf(global);
            const v = (try self.fs.readInodeOpt(local)) orelse return .plain;
            if (!economy.isDirMode(v.mode)) return .plain;
            if (stripe.isStripeRecord(v)) {
                try self.lockCache();
                defer self.lock.unlock(self.io);
                self.stripe_of.put(self.gpa, global, 0) catch stats.dir_stripe_cache_drops.inc();
                return .{ .stripe = 0 };
            }
            // The commit marker first (one targeted probe); present ⇒ the
            // whole map by name.
            const commit = (try self.fs.findDentry(local, stripe.STRIPED)) orelse {
                if (plain_miss == .forget) return .plain;
                if (g) |gen| {
                    try self.lockCache();
                    defer self.lock.unlock(self.io);
                    if (self.plain.count() >= PLAIN_CACHE_MAX) {
                        self.plain.clearRetainingCapacity();
                        stats.dir_stripe_cache_flushes.inc();
                    }
                    self.plain.put(self.gpa, global, gen) catch stats.dir_stripe_cache_drops.inc();
                }
                return .plain;
            };
            var markers: [stripe.K_MAX + 2]stripe.Marker = undefined;
            var names: [stripe.K_MAX][stripe.MARKER_NAME_LEN]u8 = undefined;
            var n: usize = 0;
            markers[n] = .{ .name = stripe.STRIPED, .child = commit.global };
            n += 1;
            if (try self.fs.findDentry(local, stripe.MIGRATING)) |m| {
                markers[n] = .{ .name = stripe.MIGRATING, .child = m.global };
                n += 1;
            }
            var i: u8 = 0;
            while (i < stripe.K_MAX) : (i += 1) {
                const nm = stripe.markerName(i, &names[i]);
                const f = (try self.fs.findDentry(local, nm)) orelse break;
                markers[n] = .{ .name = nm, .child = f.global };
                n += 1;
            }
            const map = stripe.decodeMap(global, markers[0..n]) orelse return .plain;
            try self.lockCache();
            defer self.lock.unlock(self.io);
            self.installLocked(map);
            return .{ .striped = map };
        }

        /// The map of a striped directory, null for anything else.
        pub fn mapOf(self: *Self, global: u64) Error!?StripeMap {
            return switch (try self.verdictOf(global)) {
                .striped => |m| m,
                else => null,
            };
        }

        /// `mapOf` for an rmdir's victim: a plain answer is not remembered.
        pub fn victimMapOf(self: *Self, global: u64) Error!?StripeMap {
            return switch (try self.verdict(global, .forget)) {
                .striped => |m| m,
                else => null,
            };
        }

        /// The directory a stripe belongs to, if known here.
        pub fn stripeOwner(self: *Self, ino: u64) ?u64 {
            self.lock.lockSharedUncancelable(self.io);
            defer self.lock.unlockShared(self.io);
            const owner = self.stripe_of.get(ino) orelse return null;
            return if (owner == 0) null else owner;
        }

        // ---- the routes ----

        /// `lookup(D/name)`: the stripe first, then `D`'s own tree (LWW),
        /// then the stripe again (a name moved between the two reads,
        /// `stripe.locateName`).
        pub fn locate(self: *Self, map: *const StripeMap, name: []const u8) Error!?Fs.Found {
            stats.dir_stripe_routed_lookups.inc();
            const r = map.routeName(name, self.fs.store.nameKey());
            const reader: DentryReader = .{ .fs = self.fs };
            return stripe.locateName(Fs.Found, reader, r.stripe, map.dir, name);
        }

        /// One directory's dentries by name, global inos in.
        const DentryReader = struct {
            fs: *Fs,
            pub fn find(self: DentryReader, dir: u64, name: []const u8) Error!?Fs.Found {
                return self.fs.findDentry(try Fs.localOf(dir), name);
            }
        };

        /// A NEW name's key parent: EEXIST over `D`'s own tree, the
        /// insert keys the stripe; a dying stripe (no record / nlink 0)
        /// refuses ENOENT; a foreign stripe is a ship.
        pub fn insertParent(self: *Self, map: *const StripeMap, name: []const u8) Error!u64 {
            if (try self.fs.findDentry(try Fs.localOf(map.dir), name) != null) return error.PathAlreadyExists;
            const r = map.routeName(name, self.fs.store.nameKey());
            try self.refuseDying(r.stripe);
            if (!self.plane.isLocal(r.stripe)) stripe.stats.dir_stripe_ships.inc();
            stats.dir_stripe_routed_mutations.inc();
            return r.stripe;
        }

        /// A MUTATION's key parent: the name re-homed into its stripe
        /// first when `D`'s own tree still holds it, then the stripe.
        pub fn mutationParent(self: *Self, map: *const StripeMap, name: []const u8) Error!u64 {
            stats.dir_stripe_routed_mutations.inc();
            return self.core.mutationParent(map, name, Fs.nowNs()) catch |e| return coreError(e);
        }

        fn refuseDying(self: *Self, stripe_ino: u64) Error!void {
            const v = try self.fs.readInodeOpt(try Fs.localOf(stripe_ino));
            if (v == null or v.?.nlink == 0) {
                stripe.stats.dir_stripe_dying_refusals.inc();
                return error.FileNotFound;
            }
        }

        /// `stat D`: the fold over every stripe's record at its holder.
        pub fn fold(self: *Self, map: *const StripeMap, d: InodeValue) Error!InodeValue {
            return self.core.statFold(map, d, Fs.nowNs()) catch |e| return coreError(e);
        }

        /// One merged page of `D` strictly after `offset` (markers never
        /// show; the cookie order is the continuation).
        pub fn readdir(self: *Self, arena: Allocator, map: *const StripeMap, offset: u64, max: usize, out: *std.ArrayList(stripe.DirRec)) Error!void {
            return stripe.readdirMerged(.{ .ctx = self, .page = &pageFn, .inode = &inodeFn }, arena, map, offset, max, out);
        }

        /// `rmdir` of a striped directory named `name` under `parent`:
        /// ONE intent under ONE exclusive set; the map forgotten after.
        pub fn rmdir(self: *Self, parent_global: u64, name: []const u8, dir: u64) Error!void {
            self.core.rmdir(parent_global, name, Fs.nowNs()) catch |e| return switch (e) {
                error.NotEmpty => error.DirectoryNotEmpty,
                error.Changed => error.Deferred,
                else => coreError(e),
            };
            self.forget(dir);
        }

        fn forget(self: *Self, dir: u64) void {
            self.lock.lockUncancelable(self.io);
            defer self.lock.unlock(self.io);
            if (self.maps.fetchRemove(dir)) |kv| for (kv.value.slice()) |s| {
                _ = self.stripe_of.remove(s);
            };
            for (self.migrating.items, 0..) |d, i| if (d == dir) {
                _ = self.migrating.swapRemove(i);
                break;
            };
        }

        // ---- the explicit flip ----

        /// `setfattr -n user.squeezefs.stripes -v K <dir>` at the holder:
        /// the census' heaviest creators supply when it knows any, the
        /// holder mints the rest; the map installed, the migration queued.
        pub fn flipExplicit(self: *Self, dir: u64, k: u8) Error!StripeMap {
            var suppliers: [stripe.DirCensus.MAX_CREATORS * 2]u32 = undefined;
            var n_sup: usize = 0;
            {
                self.core.census.lock.lockUncancelable(self.io);
                defer self.core.census.lock.unlock(self.io);
                if (self.core.census.dirs.getPtr(dir)) |c| n_sup = c.heaviest(&suppliers).len;
            }
            const map = self.core.flip(dir, k, suppliers[0..n_sup]) catch |e| return switch (e) {
                error.StripingOff => error.NotSupported,
                error.NotADirectory => error.NotDir,
                error.AlreadyStriped, error.IsAStripe => error.PathAlreadyExists,
                error.NotHolder => error.SlotMoved,
                error.Changed => error.Deferred,
                else => coreError(e),
            };
            self.core.census.flipDone(dir);
            self.install(map);
            stats.dir_stripe_explicit_flips.inc();
            log.info("directory {d} striped over {d} stripes (explicit)", .{ dir, map.k });
            return map;
        }

        // ---- the cadence ----

        /// The census' due flips, then one migration round per migrating
        /// directory: a page of names moved into their stripes; a directory
        /// re-verified empty of user names clears `migrating` and leaves
        /// the queue.
        pub fn tick(self: *Self) void {
            self.core.runPending();
            var dirs: [16]u64 = undefined;
            var n: usize = 0;
            {
                self.lock.lockUncancelable(self.io);
                defer self.lock.unlock(self.io);
                for (self.migrating.items) |d| {
                    if (n == dirs.len) break;
                    dirs[n] = d;
                    n += 1;
                }
            }
            for (dirs[0..n]) |d| {
                var map = (self.mapOf(d) catch continue) orelse {
                    self.forget(d);
                    continue;
                };
                if (!self.plane.isLocal(d)) continue;
                const done = self.core.migrateRound(&map, Fs.nowNs()) catch continue;
                stats.dir_stripe_migration_rounds.inc();
                if (done) {
                    stats.dir_stripe_migrations_done.inc();
                    self.lock.lockUncancelable(self.io);
                    defer self.lock.unlock(self.io);
                    if (self.maps.getPtr(d)) |m| m.migrating = false;
                    for (self.migrating.items, 0..) |x, i| if (x == d) {
                        _ = self.migrating.swapRemove(i);
                        break;
                    };
                }
            }
        }

        fn coreError(e: Core.Error) Error {
            return switch (e) {
                error.NotADirectory => error.NotDir,
                error.AlreadyStriped, error.IsAStripe => error.PathAlreadyExists,
                error.StripingOff => error.NotSupported,
                error.NotHolder => error.SlotMoved,
                error.NotEmpty => error.DirectoryNotEmpty,
                error.Changed => error.Deferred,
                else => |x| ops.coreError(x),
            };
        }
    };
}

// ---------------------------------------------------------------------------
// Tests: the filesystem over a FLIPPED directory on the RAM store (one
// home: every stripe local; the two-home shape rides `set_bind.zig`).
// ---------------------------------------------------------------------------

const testing = std.testing;
const kv_fs = @import("kv_fs.zig");
const filesystem = @import("filesystem.zig");
const op_trace = @import("op_trace.zig");
const abi = @import("../abi.zig");
const codec = @import("../codec.zig");
const inos = @import("../../meta/inos.zig");
const tree = @import("../../meta/tree.zig");

/// Every slot local on volume 0 (the one-home plane).
const OneHome = struct {
    fn homeOf(_: *anyopaque, _: u16) ops.Home {
        return .{ .local = .{ .volume = 0 } };
    }
    fn localSlots(_: *anyopaque, out: []u16) []u16 {
        out[0] = 0;
        return out[0..1];
    }
    fn ownHome(_: *anyopaque) ?u16 {
        return 0;
    }
    fn linkOf(_: *anyopaque, _: u32) ?ops.Link {
        return null;
    }
    var ctx: u8 = 0;
    fn resolver() ops.Resolver {
        return .{ .ctx = &ctx, .homeOf = &homeOf, .refresh = &homeOf, .localSlots = &localSlots, .ownHome = &ownHome, .linkOf = &linkOf };
    }
};

/// One READDIR page of `dir` from `offset` into `window`: the names and
/// the last cookie (0 when the page was empty).
fn readdirPage(fs: *kv_fs.MemFs, c: *filesystem.Ctx, dir: u64, offset: u64, size: u32, names: *std.ArrayList([]const u8), arena: Allocator) !struct { last: u64, n: usize } {
    const d = try fs.readdir(c, .{ .ino = dir, .fh = 1, .offset = offset, .size = size, .flags = 0 });
    var off: usize = 0;
    var last: u64 = 0;
    var n: usize = 0;
    while (off + abi.fuse_dirent.NAME_OFFSET <= d.len) {
        const ent = abi.decodePrefix(abi.fuse_dirent, c.window[off..d.len]).?;
        const name = c.window[off + abi.fuse_dirent.NAME_OFFSET ..][0..ent.namelen];
        try names.append(arena, try arena.dupe(u8, name));
        try testing.expect(ent.off > last);
        last = ent.off;
        n += 1;
        off += abi.fuse_dirent.recordLen(ent.namelen);
    }
    return .{ .last = last, .n = n };
}

test "kv_fs over a FLIPPED directory (RAM store): the explicit flip through setxattr maps K = 64 stripes (getxattr answers 64 on it, ENODATA on a plain directory; a second flip EEXIST); 200 creates each key the stripe hash54 % K (never D's own tree), lookup finds every name through its stripe, readdir yields every entry exactly once in cookie order restarted mid-stream at every page's last cookie, stat D folds nlink over the stripes (a mkdir inside bumps it; `..` of a child is D), a rename across two stripes moves the name, unlink empties them, rmdir D is ONE intent under the WIDE local set (66 inodes + 67 names) leaving no map, no stripe record alive and no marker" {
    const gpa = testing.allocator;
    const io = testing.io;
    var store = try kv_fs.MemStore.init(gpa);
    defer store.deinit();
    var nodata: kv_fs.NoData = .{};
    var fs = try kv_fs.MemFs.mount(gpa, io, &store, &nodata, .{ .dlm_stripes = 64 });
    defer fs.unmount();
    var plane = try kv_fs.MemFs.XvPlane.init(gpa, io, &store, &fs.locks, 1, 0, 7, OneHome.resolver(), .{}, 16 << 10);
    defer plane.deinit();
    try testing.expectEqual(@as(u32, 0), try fs.bindXv(&plane));
    _ = try fs.bindStripes(64, .{ .fixed = 2 });
    defer fs.unbindStripes();
    var tr = op_trace.OpTrace.begin(0, .other);
    defer tr.finish();
    var window: [4096]u8 = undefined;
    var c: filesystem.Ctx = .{ .unique = 1, .uid = 0, .gid = 0, .pid = 1, .io = io, .trace = &tr, .wd_slot = null, .window = &window };
    var arena_state = std.heap.ArenaAllocator.init(gpa);
    defer arena_state.deinit();
    const arena = arena_state.allocator();
    const root = inos.ROOT_INO;
    const d = (try fs.mkdir(&c, .{ .parent = root, .name = "hot", .mode = 0o755, .umask = 0 })).nodeid;
    // Plain: no K.
    try testing.expectError(error.NoAttribute, fs.getxattr(&c, .{ .ino = d, .name = stripe.COMMAND_XATTR, .size = 0 }));
    // The explicit flip; K clamps to 4; the map reads back through the
    // command's read face; a second flip refuses EEXIST.
    const flips0 = stripe.stats.dir_stripe_flips.load();
    try fs.setxattr(&c, .{ .ino = d, .name = stripe.COMMAND_XATTR, .value = "64", .flags = 0 });
    try testing.expectEqual(flips0 + 1, stripe.stats.dir_stripe_flips.load());
    const got = try fs.getxattr(&c, .{ .ino = d, .name = stripe.COMMAND_XATTR, .size = 16 });
    try testing.expectEqualStrings("64\n", window[0..got.data]);
    try testing.expectError(error.PathAlreadyExists, fs.setxattr(&c, .{ .ino = d, .name = stripe.COMMAND_XATTR, .value = "8", .flags = 0 }));
    try testing.expectError(error.NoAttribute, fs.getxattr(&c, .{ .ino = root, .name = stripe.COMMAND_XATTR, .size = 0 }));
    const map = (try fs.stripes.?.mapOf(d)).?;
    try testing.expectEqual(stripe.K_MAX, map.k);
    for (map.slice()) |s| try testing.expect(stripe.isStripeRecord((try fs.readInodeOpt(try kv_fs.MemFs.localOf(s))).?));
    // 200 creates: every name's record in the stripe its hash names.
    const N = 200;
    var inos_of: [N]u64 = undefined;
    var nb: [16]u8 = undefined;
    for (0..N) |i| {
        const name = try std.fmt.bufPrint(&nb, "f{d}", .{i});
        inos_of[i] = (try fs.create(&c, .{ .parent = d, .name = name, .mode = 0o644, .flags = 0, .umask = 0, .open_flags = 0 })).entry.nodeid;
        const r = map.routeName(name, store.nameKey());
        try testing.expect((try fs.findDentry(try kv_fs.MemFs.localOf(r.stripe), name)) != null);
        try testing.expect((try fs.findDentry(try kv_fs.MemFs.localOf(d), name)) == null);
        try testing.expectEqual(inos_of[i], (try fs.lookup(&c, .{ .parent = d, .name = name })).nodeid);
    }
    // D's own tree holds the K + 2 markers and nothing else.
    {
        var recs: std.ArrayList(stripe.DirRec) = .empty;
        _ = try fs.dirPage(arena, try kv_fs.MemFs.localOf(d), 0, 512, &recs);
        try testing.expectEqual(@as(usize, stripe.K_MAX + 2), recs.items.len);
        for (recs.items) |r| try testing.expect(stripe.isMarkerName(r.name));
    }
    // readdir in small pages, restarted at every page's last cookie:
    // every entry exactly once, `.`/`..` first, no marker.
    {
        var names: std.ArrayList([]const u8) = .empty;
        var offset: u64 = 0;
        var pages: usize = 0;
        while (true) {
            const pg = try readdirPage(&fs, &c, d, offset, 400, &names, arena);
            if (pg.n == 0) break;
            pages += 1;
            offset = pg.last;
        }
        try testing.expect(pages > 5);
        try testing.expectEqual(@as(usize, N + 2), names.items.len);
        try testing.expectEqualStrings(".", names.items[0]);
        try testing.expectEqualStrings("..", names.items[1]);
        var seen: std.StringHashMapUnmanaged(void) = .empty;
        defer seen.deinit(gpa);
        for (names.items[2..]) |nm| {
            try testing.expect(!stripe.isMarkerName(nm));
            try testing.expect(!seen.contains(nm));
            try seen.put(gpa, nm, {});
        }
        for (0..N) |i| try testing.expect(seen.contains(try std.fmt.bufPrint(&nb, "f{d}", .{i})));
    }
    // stat D: the fold — a subdirectory in a stripe adds one to D's nlink.
    try testing.expectEqual(@as(u32, 2), (try fs.getattr(&c, .{ .ino = d, .fh = null })).attr.nlink);
    const sub = (try fs.mkdir(&c, .{ .parent = d, .name = "sub", .mode = 0o755, .umask = 0 })).nodeid;
    try testing.expectEqual(@as(u32, 3), (try fs.getattr(&c, .{ .ino = d, .fh = null })).attr.nlink);
    try testing.expectEqual(@as(u32, 3), (try fs.lookup(&c, .{ .parent = root, .name = "hot" })).attr.nlink);
    // `..` of a child is D, not the stripe.
    try testing.expectEqual(d, (try fs.lookup(&c, .{ .parent = sub, .name = ".." })).nodeid);
    // A rename across two stripes: the name moves stripe, the ino stays.
    var moved: ?usize = null;
    var newname_buf: [16]u8 = undefined;
    var newname: []const u8 = "";
    for (0..N) |i| {
        const old = try std.fmt.bufPrint(&nb, "f{d}", .{i});
        newname = try std.fmt.bufPrint(&newname_buf, "g{d}", .{i});
        if (map.routeName(old, store.nameKey()).index != map.routeName(newname, store.nameKey()).index) {
            moved = i;
            break;
        }
    }
    const mi = moved.?;
    const oldname = try std.fmt.bufPrint(&nb, "f{d}", .{mi});
    try fs.rename(&c, .{ .parent = d, .name = oldname, .newparent = d, .newname = newname, .flags = 0 });
    try testing.expectEqual(inos_of[mi], (try fs.lookup(&c, .{ .parent = d, .name = newname })).nodeid);
    try testing.expectEqual(@as(u64, 0), (try fs.lookup(&c, .{ .parent = d, .name = oldname })).nodeid);
    try testing.expect((try fs.findDentry(try kv_fs.MemFs.localOf(map.routeName(newname, store.nameKey()).stripe), newname)) != null);
    // rmdir refuses while names remain; unlink everything; rmdir D is ONE
    // intent: no map, every stripe record dying, no marker left.
    try testing.expectError(error.DirectoryNotEmpty, fs.rmdir(&c, .{ .parent = root, .name = "hot" }));
    for (0..N) |i| {
        const name = if (i == mi) newname else try std.fmt.bufPrint(&nb, "f{d}", .{i});
        try fs.unlink(&c, .{ .parent = d, .name = name });
    }
    try fs.rmdir(&c, .{ .parent = d, .name = "sub" });
    try testing.expectEqual(@as(u32, 2), (try fs.getattr(&c, .{ .ino = d, .fh = null })).attr.nlink);
    const rmdirs0 = stripe.stats.dir_stripe_rmdirs.load();
    try fs.rmdir(&c, .{ .parent = root, .name = "hot" });
    try testing.expectEqual(rmdirs0 + 1, stripe.stats.dir_stripe_rmdirs.load());
    try testing.expectEqual(@as(u64, 0), (try fs.lookup(&c, .{ .parent = root, .name = "hot" })).nodeid);
    try testing.expect((try fs.stripes.?.mapOf(d)) == null);
    for (map.slice()) |s| {
        const v = try fs.readInodeOpt(try kv_fs.MemFs.localOf(s));
        try testing.expect(v == null or v.?.nlink == 0);
    }
    try testing.expectEqual(@as(usize, 0), store.countKind(key.TREE_DENTRIES));
    try testing.expectEqual(@as(u32, 0), (try fs.getattr(&c, .{ .ino = d, .fh = null })).attr.nlink);
}

test "a verdict install is all or nothing: an allocation failure caches no part of a migrating map, so the next verdict re-reads it and queues its migration for the cadence" {
    const gpa = testing.allocator;
    const io = testing.io;
    var store = try kv_fs.MemStore.init(gpa);
    defer store.deinit();
    var nodata: kv_fs.NoData = .{};
    var fs = try kv_fs.MemFs.mount(gpa, io, &store, &nodata, .{ .dlm_stripes = 64 });
    defer fs.unmount();
    var plane = try kv_fs.MemFs.XvPlane.init(gpa, io, &store, &fs.locks, 1, 0, 7, OneHome.resolver(), .{}, 16 << 10);
    defer plane.deinit();
    try testing.expectEqual(@as(u32, 0), try fs.bindXv(&plane));
    const st = try fs.bindStripes(64, .{ .fixed = 2 });
    defer fs.unbindStripes();
    var tr = op_trace.OpTrace.begin(0, .other);
    defer tr.finish();
    var window: [4096]u8 = undefined;
    var c: filesystem.Ctx = .{ .unique = 1, .uid = 0, .gid = 0, .pid = 1, .io = io, .trace = &tr, .wd_slot = null, .window = &window };
    const d = (try fs.mkdir(&c, .{ .parent = inos.ROOT_INO, .name = "hot", .mode = 0o755, .umask = 0 })).nodeid;
    try fs.setxattr(&c, .{ .ino = d, .name = stripe.COMMAND_XATTR, .value = "64", .flags = 0 });
    // A fresh cache (the map's slot kept, the rest unallocated), then a
    // verdict under an allocator that refuses everything.
    st.maps.clearRetainingCapacity();
    st.stripe_of.deinit(gpa);
    st.stripe_of = .empty;
    st.migrating.deinit(gpa);
    st.migrating = .empty;
    var failing = std.testing.FailingAllocator.init(gpa, .{ .fail_index = 0, .resize_fail_index = 0 });
    st.gpa = failing.allocator();
    try testing.expect((try st.verdictOf(d)) == .striped);
    st.gpa = gpa;
    try testing.expect((try st.verdictOf(d)) == .striped);
    try testing.expectEqual(@as(usize, 1), st.migrating.items.len);
    try testing.expectEqual(d, st.migrating.items[0]);
}

test "two concurrent folds of one striped directory (a stat and a lookup of D on two workers, both past the cadence) persist D's times ONCE: the first claims the cadence slot before its commit, the second sees the claim and skips — one time batch, one stamp writer, D's record carrying the fold" {
    const gpa = testing.allocator;
    const io = testing.io;
    var store = try kv_fs.MemStore.init(gpa);
    defer store.deinit();
    var nodata: kv_fs.NoData = .{};
    var fs = try kv_fs.MemFs.mount(gpa, io, &store, &nodata, .{ .dlm_stripes = 64 });
    defer fs.unmount();
    var plane = try kv_fs.MemFs.XvPlane.init(gpa, io, &store, &fs.locks, 1, 0, 7, OneHome.resolver(), .{}, 16 << 10);
    defer plane.deinit();
    try testing.expectEqual(@as(u32, 0), try fs.bindXv(&plane));
    _ = try fs.bindStripes(64, .{ .fixed = 2 });
    defer fs.unbindStripes();
    var tr = op_trace.OpTrace.begin(0, .other);
    defer tr.finish();
    var window: [4096]u8 = undefined;
    var c: filesystem.Ctx = .{ .unique = 1, .uid = 0, .gid = 0, .pid = 1, .io = io, .trace = &tr, .wd_slot = null, .window = &window };
    const d = (try fs.mkdir(&c, .{ .parent = inos.ROOT_INO, .name = "hot", .mode = 0o755, .umask = 0 })).nodeid;
    try fs.setxattr(&c, .{ .ino = d, .name = stripe.COMMAND_XATTR, .value = "64", .flags = 0 });
    // A create into a stripe: the stripe's times pass D's, so every fold
    // of D is a persist candidate.
    _ = try fs.create(&c, .{ .parent = d, .name = "f", .mode = 0o644, .flags = 0, .umask = 0, .open_flags = 0 });
    const before = (try fs.readInodeOpt(try kv_fs.MemFs.localOf(d))).?;
    const seams = &stripe.test_seams;

    // Fold A (another worker's `stat D`) wins the cadence check and holds
    // before its persist.
    const batches0 = stripe.stats.dir_stripe_time_batches.load();
    seams.arm();
    const Folder = struct {
        fs: *kv_fs.MemFs,
        io: Io,
        ino: u64,
        result: Error!filesystem.Attr = error.Unexpected,
        fn run(self: *@This()) void {
            var t = op_trace.OpTrace.begin(2, .other);
            defer t.finish();
            var ctx: filesystem.Ctx = .{ .unique = 2, .uid = 0, .gid = 0, .pid = 1, .io = self.io, .trace = &t, .wd_slot = null, .window = &.{} };
            self.result = self.fs.getattr(&ctx, .{ .ino = self.ino, .fh = null });
        }
    };
    var a: Folder = .{ .fs = &fs, .io = io, .ino = d };
    const th = try std.Thread.spawn(.{}, Folder.run, .{&a});
    // A failed expectation below must still release A and join it (its
    // frame is this test's).
    var joined = false;
    defer if (!joined) {
        seams.release(io);
        th.join();
    };
    while (!seams.held()) try std.Thread.yield();

    // Fold B (this worker's `lookup D`) runs to completion inside A's
    // window: the cadence slot is A's, so B persists nothing.
    const ent = try fs.lookup(&c, .{ .parent = inos.ROOT_INO, .name = "hot" });
    const batches_after_b = stripe.stats.dir_stripe_time_batches.load();

    seams.release(io);
    th.join();
    joined = true;
    try testing.expectEqual(d, ent.nodeid);
    try testing.expectEqual(batches0, batches_after_b);
    const attr = try a.result;
    try testing.expectEqual(batches0 + 1, stripe.stats.dir_stripe_time_batches.load());
    // D's record carries the fold's times once; both folds answered them.
    const after = (try fs.readInodeOpt(try kv_fs.MemFs.localOf(d))).?;
    try testing.expect(economy.laterThan(after.mtime, before.mtime));
    const shown = codec.attrFromInode(d, after, fs.blksize);
    try testing.expectEqual(shown.mtime, attr.attr.mtime);
    try testing.expectEqual(shown.mtimensec, attr.attr.mtimensec);
    try testing.expectEqual(shown.mtime, ent.attr.mtime);
    try testing.expectEqual(shown.mtimensec, ent.attr.mtimensec);
    // A third fold within the cadence: fresh stamp, nothing persisted.
    _ = try fs.getattr(&c, .{ .ino = d, .fh = null });
    try testing.expectEqual(batches0 + 1, stripe.stats.dir_stripe_time_batches.load());
}

test "the drain worker's try-only probe never queues behind the verdict cache's lock: a WRITER-held lock answers WouldBlock at once with nobody parked (the lane re-asks); a free lock serves the probe; another reader inside shares it" {
    const gpa = testing.allocator;
    const io = testing.io;
    var store = try kv_fs.MemStore.init(gpa);
    defer store.deinit();
    var nodata: kv_fs.NoData = .{};
    var fs = try kv_fs.MemFs.mount(gpa, io, &store, &nodata, .{ .dlm_stripes = 64 });
    defer fs.unmount();
    var plane = try kv_fs.MemFs.XvPlane.init(gpa, io, &store, &fs.locks, 1, 0, 7, OneHome.resolver(), .{}, 16 << 10);
    defer plane.deinit();
    try testing.expectEqual(@as(u32, 0), try fs.bindXv(&plane));
    const st = try fs.bindStripes(64, .{ .fixed = 2 });
    defer fs.unbindStripes();
    var tr = op_trace.OpTrace.begin(0, .other);
    defer tr.finish();
    var window: [4096]u8 = undefined;
    var c: filesystem.Ctx = .{ .unique = 1, .uid = 0, .gid = 0, .pid = 1, .io = io, .trace = &tr, .wd_slot = null, .window = &window };
    const d = (try fs.mkdir(&c, .{ .parent = inos.ROOT_INO, .name = "hot", .mode = 0o755, .umask = 0 })).nodeid;
    const Prober = struct {
        fs: *kv_fs.MemFs,
        io: Io,
        result: Error!filesystem.Entry = error.Unexpected,
        done: std.atomic.Value(bool) = .init(false),
        fn run(self: *@This()) void {
            var t = op_trace.OpTrace.begin(2, .other);
            defer t.finish();
            var ctx: filesystem.Ctx = .{ .unique = 2, .uid = 0, .gid = 0, .pid = 1, .io = self.io, .trace = &t, .wd_slot = null, .window = &.{} };
            self.result = self.fs.probe_lookup(&ctx, .{ .parent = inos.ROOT_INO, .name = "hot" });
            self.done.store(true, .release);
        }
    };
    // The lock held by another party (the holder is this thread; the
    // probe runs where the drain worker would): the probe must come back
    // refused without ever joining the lock's queue.
    const refusals0 = stats.dir_stripe_verdict_try_refusals.load();
    st.lock.lockUncancelable(io);
    var p: Prober = .{ .fs = &fs, .io = io };
    const th = try std.Thread.spawn(.{}, Prober.run, .{&p});
    var parked = false;
    while (!p.done.load(.acquire)) {
        if (st.lock.waiters(io) != 0) {
            parked = true;
            break;
        }
        try std.Thread.yield();
    }
    st.lock.unlock(io);
    th.join();
    try testing.expect(!parked);
    try testing.expectError(error.WouldBlock, p.result);
    try testing.expectEqual(refusals0 + 1, stats.dir_stripe_verdict_try_refusals.load());
    // Free: the probe serves the entry.
    var p2: Prober = .{ .fs = &fs, .io = io };
    p2.run();
    try testing.expectEqual(d, (try p2.result).nodeid);
    // Another READER inside the cache (a verdict's probe on another
    // worker): the probe shares the hold and is served, no refusal.
    const refusals1 = stats.dir_stripe_verdict_try_refusals.load();
    st.lock.lockSharedUncancelable(io);
    var p3: Prober = .{ .fs = &fs, .io = io };
    const th3 = try std.Thread.spawn(.{}, Prober.run, .{&p3});
    th3.join();
    st.lock.unlockShared(io);
    try testing.expectEqual(d, (try p3.result).nodeid);
    try testing.expectEqual(refusals1, stats.dir_stripe_verdict_try_refusals.load());
}

test "an rmdir reads its victim's verdict and never remembers it: with a reader inside the verdict cache, rmdir of a plain directory completes without joining the cache's queue — the cache's writes are for directories that live on (on mdtest -np 32 every rmdir inserted its dying victim's plain verdict under the EXCLUSIVE lock, and every rank's shared probe queued behind those inserts: 189 µs before the parent's verdict, 373 µs around the victim's, the whole op 600 µs)" {
    const gpa = testing.allocator;
    const io = testing.io;
    var store = try kv_fs.MemStore.init(gpa);
    defer store.deinit();
    var nodata: kv_fs.NoData = .{};
    var fs = try kv_fs.MemFs.mount(gpa, io, &store, &nodata, .{ .dlm_stripes = 64 });
    defer fs.unmount();
    var plane = try kv_fs.MemFs.XvPlane.init(gpa, io, &store, &fs.locks, 1, 0, 7, OneHome.resolver(), .{}, 16 << 10);
    defer plane.deinit();
    try testing.expectEqual(@as(u32, 0), try fs.bindXv(&plane));
    const st = try fs.bindStripes(64, .{ .fixed = 2 });
    defer fs.unbindStripes();
    var tr = op_trace.OpTrace.begin(0, .other);
    defer tr.finish();
    var window: [4096]u8 = undefined;
    var c: filesystem.Ctx = .{ .unique = 1, .uid = 0, .gid = 0, .pid = 1, .io = io, .trace = &tr, .wd_slot = null, .window = &window };
    const d = (try fs.mkdir(&c, .{ .parent = inos.ROOT_INO, .name = "rank", .mode = 0o755, .umask = 0 })).nodeid;
    _ = try fs.mkdir(&c, .{ .parent = d, .name = "dir.0", .mode = 0o755, .umask = 0 });
    const Remover = struct {
        fs: *kv_fs.MemFs,
        io: Io,
        parent: u64,
        result: Error!void = error.Unexpected,
        done: std.atomic.Value(bool) = .init(false),
        fn run(self: *@This()) void {
            var t = op_trace.OpTrace.begin(2, .unlink);
            defer t.finish();
            var ctx: filesystem.Ctx = .{ .unique = 2, .uid = 0, .gid = 0, .pid = 1, .io = self.io, .trace = &t, .wd_slot = null, .window = &.{} };
            self.result = self.fs.rmdir(&ctx, .{ .parent = self.parent, .name = "dir.0" });
            self.done.store(true, .release);
        }
    };
    const reads0 = stats.dir_stripe_verdict_reads.load();
    // A reader inside (another worker's probe): the rmdir shares it, or
    // queues for the exclusive hold an insert needs.
    st.lock.lockSharedUncancelable(io);
    var r: Remover = .{ .fs = &fs, .io = io, .parent = d };
    const th = try std.Thread.spawn(.{}, Remover.run, .{&r});
    var queued = false;
    while (!r.done.load(.acquire)) {
        if (st.lock.waiters(io) != 0) {
            queued = true;
            break;
        }
        try std.Thread.yield();
    }
    st.lock.unlockShared(io);
    th.join();
    try testing.expect(!queued);
    try r.result;
    // The victim's verdict was read (no stale answer), not remembered.
    try testing.expectEqual(reads0 + 1, stats.dir_stripe_verdict_reads.load());
    // Gone: the lookup answers a negative entry.
    try testing.expectEqual(@as(u64, 0), (try fs.lookup(&c, .{ .parent = d, .name = "dir.0" })).nodeid);
}
