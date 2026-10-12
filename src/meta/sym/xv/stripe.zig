//! DIRECTORY STRIPING — the DNE-2 shape, K = 64 (charter §5.5 "Directory
//! striping"; `docs/porting/sym_tokens_intents_striping.md` §4): the MAP
//! is `K + 2` reserved NUL-led dentries in `D`'s own tree —
//! `\0sqz.stripe:NN → D_i` (exactly two decimal digits: "the decoder is
//! the encoder's inverse and nothing wider"), `\0sqz.migrating → D_0`,
//! `\0sqz.striped → D_0` LAST = the commit marker ("the map is live iff
//! it exists and names stripe 0"); no marker names `D` itself. WHY
//! dentries: the map RIDES `D`'s token verbatim, the flip is `K + 2`
//! `InsertDentry` steps of ONE intent (no new step kind, no new applier),
//! C9's dentry pass marks every marker's target so a mapped stripe is
//! REFERENCED; no slot-tree key kind is added. A NUL-led name is
//! unreachable from FUSE and filtered from `readdir` at the ONE pager. A
//! stripe never carries a map.
//!
//! `stripe(name) = hash54(name) % K` over the SEEDED hash of `D`'s volume
//! — every stripe lives on `D`'s volume so the K-way merge has ONE
//! `(hash54, coll)` order and the shipped cookie is its exact
//! continuation. Stripe inos are minted in DIFFERENT creators' slots (the
//! census' heaviest suppliers first, the remainder in the holder's). The
//! FLIP is ONE intent in four phases: A the supplied stripes, B the
//! remainder's inos, C the op's ONE guard set (`I{D}` + `I{D_i}` minted
//! here + `D{D, marker}` × K + 2, canonical, taken AFTER the suppliers'
//! asks and BEFORE the remainder's mints — a second acquisition on the
//! same table was a self-deadlock), D the remainder's records; then the
//! intent with the commit marker LAST. Triggers: the census (foreign
//! ships into `D` exceed `N_floor` per `T_idle` from MORE THAN ONE creator
//! — "a directory ONE foreign node writes is a handover candidate, not a
//! striping one"), the explicit `setfattr -n user.squeezefs.stripes -v K`
//! (a COMMAND, never a stored value), `-o stripe_dirs` at `mkdir`. Lazy
//! re-home: `D`'s tree is a permanent SECONDARY home for reads (the stripe
//! wins — LWW); every mutation keys the STRIPE and re-homes the name
//! first. `readdir` is the streamed K-way merge in cookie order; `stat D`
//! folds `nlink = D.nlink + Σ max(D_i.nlink − 2, 0)` and the max times,
//! persisted once per checkpoint cadence; `rmdir D` is ONE intent under
//! ONE exclusive guard set over `D` and every stripe (decision 0002 Q15:
//! the stripes' exclusive guards travel, so no create lands between the
//! emptiness probe and the dying mark). fsck C17 is report-only.
//! PORTABLE, generic over `Store` where it touches records.

const std = @import("std");
const Allocator = std.mem.Allocator;
const Io = std.Io;
const counters = @import("../../../core/counters.zig");
const errors = @import("../../../core/errors.zig");
const clock = @import("../../../core/clock.zig");
const knobs = @import("../../../core/knobs.zig");
const log = @import("../../../core/log.zig");
const sync = @import("../../../sync/sync.zig");
const service = @import("../../../wire/service.zig");
const key = @import("../../key.zig");
const record = @import("../../record.zig");
const tree = @import("../../tree.zig");
const dlm = @import("../../dlm.zig");
const economy = @import("../../economy.zig");
const route_table = @import("../../routing/route_table.zig");
const acquire = @import("../slots/acquire.zig");
const token_table = @import("../tokens/token_table.zig");
const intent = @import("intent.zig");
const steps = @import("steps.zig");
const ops = @import("ops.zig");

const assert = std.debug.assert;
const S = economy.S;
pub const InodeValue = record.InodeValue;
pub const Plan = steps.Plan;

pub const stats = struct {
    /// Striped directories this mount knows (gauge).
    pub var dir_striped_dirs: counters.Gauge = .{};
    pub var dir_stripe_flips: counters.ShardedCounter = .{};
    /// `SupplyStripeInos` asks sent (one BATCH per creator per flip) and
    /// the stripe records they brought back; the served side's count.
    pub var dir_stripe_supply_rpcs: counters.ShardedCounter = .{};
    pub var dir_stripe_supplied: counters.ShardedCounter = .{};
    pub var dir_stripe_supplies_served: counters.ShardedCounter = .{};
    /// A creator that declined or could not be reached: its batch was
    /// minted by the holder instead.
    pub var dir_stripe_supply_declined: counters.ShardedCounter = .{};
    /// A census-triggered flip that did not land (named loud; the
    /// directory re-enters the census at its next served insert).
    pub var dir_stripe_flip_failures: counters.ShardedCounter = .{};
    /// Creates into a striped directory keyed to a FOREIGN stripe.
    pub var dir_stripe_ships: counters.ShardedCounter = .{};
    pub var dir_stripe_readdir_merges: counters.ShardedCounter = .{};
    /// Names moved from `D`'s tree into their stripe (the lazy re-home).
    pub var dir_stripe_migrated_names: counters.ShardedCounter = .{};
    /// Names re-homed by the mutation that touched them.
    pub var dir_stripe_rehomed_on_touch: counters.ShardedCounter = .{};
    /// The fold's times persisted onto `D` (once per cadence per directory).
    pub var dir_stripe_time_batches: counters.ShardedCounter = .{};
    /// An insert refused: the key parent is dying (nlink 0 / no record).
    pub var dir_stripe_dying_refusals: counters.ShardedCounter = .{};
    pub var dir_stripe_rmdirs: counters.ShardedCounter = .{};
    /// A stripe whose holder could not be reached contributed nothing to
    /// a fold (never an EIO for the window).
    pub var dir_stripe_fold_unreachable: counters.ShardedCounter = .{};
    /// fsck C17 findings (report-only; must stay 0 on a healthy set).
    pub var fsck_stripe_findings: counters.Tripwire = .init("fsck_stripe_findings");
    /// Directories the census tracks (gauge).
    pub var dir_stripe_census_dirs: counters.Gauge = .{};
};

/// `K = MINT_SPREAD = 64`: the shipped granularity constant — 64 stripe
/// inos per directory is cheap; a directory is never re-striped.
pub const K_MAX: u8 = @intCast(acquire.MINT_SPREAD);
/// Two decimal digits: `STRIPES_MAX` is 64 — the codec's width IS the bound.
pub const MARKER_DIGITS: usize = 2;
/// The reserved NUL-led names.
pub const STRIPE_PREFIX: []const u8 = "\x00sqz.stripe:";
pub const MIGRATING: []const u8 = "\x00sqz.migrating";
pub const STRIPED: []const u8 = "\x00sqz.striped";
pub const MARKER_NAME_LEN: usize = STRIPE_PREFIX.len + MARKER_DIGITS;
/// The explicit command's xattr name (intercepted BEFORE the allowlist).
pub const COMMAND_XATTR: []const u8 = "user.squeezefs.stripes";
/// The merged readdir's smallest useful stream page: below it the
/// per-refill overhead dominates ("a merged page reads ≤ max + (K + 1) ×
/// page raw entries whatever the stripes hold — never K × max").
pub const MERGE_STREAM_PAGE_FLOOR: usize = 8;
/// One merge-floor page per stripe of the widest map = the KV walkers'
/// page.
pub const MIGRATION_SCAN_PAGE: usize = MERGE_STREAM_PAGE_FLOOR * K_MAX;
/// `W / MINT_SPREAD`: one hot shared directory per rotor-sized slice of
/// the slot namespace.
pub const CENSUS_DIRS_MAX: usize = @intCast(route_table.W / acquire.MINT_SPREAD);
/// `COVER_CYCLES_MAX`: the bound every bounded cover loop shares.
pub const MIGRATION_ROUNDS_MAX: u32 = 64;
/// The trigger's half-window pair: `T_idle` = `T_owner` by derivation.
pub const T_IDLE_NS: u64 = token_table.T_OWNER_NS;

comptime {
    assert(K_MAX == 64 and MIGRATION_SCAN_PAGE == 512 and CENSUS_DIRS_MAX == 1024);
}

/// The striping width in force: the knob verbatim (1 = OFF), else K_MAX.
pub fn resolveK(env: *const std.process.Environ.Map) u8 {
    return knobs.optIntKnob("SQUEEZEFS_SYM_DIR_STRIPES", u8, env) orelse K_MAX;
}

/// `setfattr -v K`: clamped `2..=64`; unparseable takes the width in force.
pub fn clampExplicitK(value: []const u8, in_force: u8) u8 {
    const parsed = std.fmt.parseInt(u8, std.mem.trim(u8, value, " \n\t\x00"), 10) catch return in_force;
    return @intCast(std.math.clamp(@as(u32, parsed), 2, K_MAX));
}

// ---------------------------------------------------------------------------
// Markers.
// ---------------------------------------------------------------------------

pub fn markerName(index: u8, buf: *[MARKER_NAME_LEN]u8) []const u8 {
    assert(index < K_MAX);
    @memcpy(buf[0..STRIPE_PREFIX.len], STRIPE_PREFIX);
    buf[STRIPE_PREFIX.len] = '0' + index / 10;
    buf[STRIPE_PREFIX.len + 1] = '0' + index % 10;
    return buf[0..MARKER_NAME_LEN];
}

/// The stripe index a marker name encodes: exactly two digits
/// (`\0sqz.stripe:5` and `:005` are NOT markers).
pub fn stripeIndexOfMarker(name: []const u8) ?u8 {
    if (name.len != MARKER_NAME_LEN or !std.mem.startsWith(u8, name, STRIPE_PREFIX)) return null;
    const a = name[STRIPE_PREFIX.len];
    const b = name[STRIPE_PREFIX.len + 1];
    if (a < '0' or a > '9' or b < '0' or b > '9') return null;
    const i: u8 = (a - '0') * 10 + (b - '0');
    return if (i < K_MAX) i else null;
}

/// A NUL-led name of the reserved family: filtered from `readdir`,
/// unreachable from FUSE.
pub fn isMarkerName(name: []const u8) bool {
    return name.len > 0 and name[0] == 0;
}

/// `stripe_of(h, K) = h % max(K, 1)`.
pub fn stripeOf(hash54: u64, k: u8) u8 {
    return @intCast(hash54 % @max(@as(u64, k), 1));
}

// ---------------------------------------------------------------------------
// The map.
// ---------------------------------------------------------------------------

/// `StripeRoute{index, stripe}`.
pub const Route = struct { index: u8, stripe: u64 };

pub const StripeMap = struct {
    dir: u64,
    k: u8,
    stripes: [K_MAX]u64,
    migrating: bool,

    pub fn slice(self: *const StripeMap) []const u64 {
        return self.stripes[0..self.k];
    }

    /// `stripe_for(hash54)`: the index and the stripe ino.
    pub fn route(self: *const StripeMap, hash54: u64) Route {
        const i = stripeOf(hash54, self.k);
        return .{ .index = i, .stripe = self.stripes[i] };
    }

    pub fn routeName(self: *const StripeMap, name: []const u8, name_key: *const key.NameKey) Route {
        return self.route(key.dentryNameHash54(name, name_key));
    }

    pub fn isStripe(self: *const StripeMap, ino: u64) bool {
        for (self.slice()) |s| if (s == ino) return true;
        return false;
    }
};

/// One marker as found in `D`'s tree.
pub const Marker = struct { name: []const u8, child: u64 };

/// Decode the map from `D`'s dentries (every entry handed in; non-markers
/// ignored): live iff the commit marker exists and names stripe 0, the
/// indices `0..k` are all present and contiguous, and no marker names
/// `D` itself.
pub fn decodeMap(dir: u64, markers: []const Marker) ?StripeMap {
    var m: StripeMap = .{ .dir = dir, .k = 0, .stripes = @splat(0), .migrating = false };
    var present: [K_MAX]bool = @splat(false);
    var striped: ?u64 = null;
    var max_index: ?u8 = null;
    for (markers) |mk| {
        if (mk.child == dir) return null;
        if (std.mem.eql(u8, mk.name, STRIPED)) {
            striped = mk.child;
        } else if (std.mem.eql(u8, mk.name, MIGRATING)) {
            m.migrating = true;
        } else if (stripeIndexOfMarker(mk.name)) |i| {
            present[i] = true;
            m.stripes[i] = mk.child;
            if (max_index == null or i > max_index.?) max_index = i;
        }
    }
    const commit = striped orelse return null;
    const top = max_index orelse return null;
    if (top < 1) return null;
    var i: u8 = 0;
    while (i <= top) : (i += 1) if (!present[i]) return null;
    if (m.stripes[0] != commit) return null;
    m.k = top + 1;
    return m;
}

// ---------------------------------------------------------------------------
// Plans.
// ---------------------------------------------------------------------------

/// The flip as ONE intent: K stripe markers, `migrating`, then the
/// commit marker LAST with the exclusive times update.
pub fn planFlip(dir: u64, stripes: []const u64, names: *[K_MAX][MARKER_NAME_LEN]u8) Plan {
    assert(stripes.len >= 2 and stripes.len <= K_MAX);
    var p: Plan = .{ .op = .stripe_dir };
    for (stripes, 0..) |s, i| {
        assert(s != dir);
        const nm = markerName(@intCast(i), &names[i]);
        p.push(.{ .insert_dentry = .{ .parent = dir, .name = nm, .child = s, .ft_bits = S.IFDIR, .parent_update = .none } });
    }
    p.push(.{ .insert_dentry = .{ .parent = dir, .name = MIGRATING, .child = stripes[0], .ft_bits = S.IFDIR, .parent_update = .none } });
    p.push(.{ .insert_dentry = .{ .parent = dir, .name = STRIPED, .child = stripes[0], .ft_bits = S.IFDIR, .parent_update = .exclusive_times } });
    return p;
}

/// One name's migration: insert into the STRIPE first, remove from `D`
/// second ("a kill between the two leaves the name in both places and the
/// stripe wins"); a FILE loser (the stripe already names another child)
/// goes with its count.
pub fn planMigrateOne(dir: u64, stripe: u64, name: []const u8, child: u64, ft_bits: u32, loser_count: ?struct { ino: u64, nlink: u32 }, now: u64) Plan {
    var p: Plan = .{ .op = .rename };
    p.push(.{ .insert_dentry = .{ .parent = stripe, .name = name, .child = child, .ft_bits = ft_bits, .parent_update = .none } });
    p.push(.{ .remove_dentry = .{ .parent = dir, .name = name, .expect_child = child, .parent_update = .none } });
    if (loser_count) |l| p.push(.{ .set_nlink = .{ .ino = l.ino, .pre = l.nlink, .post = l.nlink -| 1, .ctime = now } });
    return p;
}

/// `rmdir D` as ONE intent: the parent's name, the commit marker FIRST
/// ("a reader between steps sees no map"), `migrating` if set, every
/// stripe marker, the dying mark on every stripe with count 2, `D`'s own.
pub fn planRmdirStriped(parent: u64, name: []const u8, map: *const StripeMap, stripe_nlinks: []const u32, d_nlink: u32, names: *[K_MAX][MARKER_NAME_LEN]u8, now: u64) Plan {
    assert(stripe_nlinks.len == map.k);
    var p: Plan = .{ .op = .rmdir };
    p.push(.{ .remove_dentry = .{ .parent = parent, .name = name, .expect_child = map.dir, .parent_update = .exclusive_times_bump } });
    p.push(.{ .remove_dentry = .{ .parent = map.dir, .name = STRIPED, .expect_child = map.stripes[0], .parent_update = .none } });
    if (map.migrating) p.push(.{ .remove_dentry = .{ .parent = map.dir, .name = MIGRATING, .expect_child = map.stripes[0], .parent_update = .none } });
    for (map.slice(), 0..) |s, i| {
        const nm = markerName(@intCast(i), &names[i]);
        p.push(.{ .remove_dentry = .{ .parent = map.dir, .name = nm, .expect_child = s, .parent_update = .none } });
    }
    for (map.slice(), stripe_nlinks) |s, n| {
        if (n == 2) p.push(.{ .set_nlink = .{ .ino = s, .pre = 2, .post = 0, .ctime = now } });
    }
    p.push(.{ .set_nlink = .{ .ino = map.dir, .pre = d_nlink, .post = 0, .ctime = now } });
    return p;
}

// ---------------------------------------------------------------------------
// The fold.
// ---------------------------------------------------------------------------

/// `stat D`: `nlink = D.nlink + Σ max(D_i.nlink − 2, 0)`, times = the max
/// over `D` and its stripes; an unreachable stripe (null) contributes
/// nothing for the window.
pub fn foldAttrs(d: InodeValue, stripes: []const ?InodeValue) InodeValue {
    var out = d;
    for (stripes) |opt| {
        const s = opt orelse {
            stats.dir_stripe_fold_unreachable.inc();
            continue;
        };
        out.nlink += s.nlink -| 2;
        if (economy.laterThan(s.mtime, out.mtime)) out.mtime = s.mtime;
        if (economy.laterThan(s.ctime, out.ctime)) out.ctime = s.ctime;
    }
    return out;
}

// ---------------------------------------------------------------------------
// The census (the automatic trigger).
// ---------------------------------------------------------------------------

/// `N_floor = max(2, ceil(ewma_handover / ewma_ship))`: a handover must
/// pay for itself in ships saved; 2 = a single touch never moves
/// anything. Cold (no handover measured yet) the handover term is `4 ×
/// barrier + 3 × ship_rtt`.
pub fn nFloor(ewma_handover_ns: u64, ewma_ship_ns: u64, ewma_barrier_ns: u64) u32 {
    const ship = @max(ewma_ship_ns, 1);
    const handover = if (ewma_handover_ns != 0) ewma_handover_ns else 4 * ewma_barrier_ns + 3 * ewma_ship_ns;
    return @intCast(@max(2, (handover + ship - 1) / ship));
}

/// The half-window epoch: `now / (T_idle / 2)`.
pub fn windowEpoch(now_ns: u64, t_idle_ns: u64) u64 {
    return now_ns / @max(t_idle_ns / 2, 1);
}

/// Per-directory creator census over the half-window pair: O(1)
/// increments, `distinct` and `total` folded over `cur` + `prev`.
pub const DirCensus = struct {
    pub const MAX_CREATORS = 8;
    const Bucket = struct {
        epoch: u64 = 0,
        creators: [MAX_CREATORS]u32 = @splat(0),
        ops: [MAX_CREATORS]u32 = @splat(0),
        n: u8 = 0,
        total: u32 = 0,

        fn note(self: *Bucket, creator: u32) void {
            self.total += 1;
            for (self.creators[0..self.n], 0..) |c, i| if (c == creator) {
                self.ops[i] += 1;
                return;
            };
            if (self.n == MAX_CREATORS) return;
            self.creators[self.n] = creator;
            self.ops[self.n] = 1;
            self.n += 1;
        }
    };

    cur: Bucket = .{},
    prev: Bucket = .{},
    last_touch_ns: u64 = 0,

    pub fn note(self: *DirCensus, creator: u32, now_ns: u64, t_idle_ns: u64) void {
        const e = windowEpoch(now_ns, t_idle_ns);
        if (self.cur.epoch != e) {
            self.prev = if (self.cur.epoch + 1 == e) self.cur else .{};
            self.cur = .{ .epoch = e };
        }
        self.cur.note(creator);
        self.last_touch_ns = now_ns;
    }

    pub fn total(self: *const DirCensus) u32 {
        return self.cur.total + self.prev.total;
    }

    pub fn distinct(self: *const DirCensus) u32 {
        var n: u32 = self.cur.n;
        for (self.prev.creators[0..self.prev.n]) |c| {
            var seen = false;
            for (self.cur.creators[0..self.cur.n]) |d| if (c == d) {
                seen = true;
            };
            if (!seen) n += 1;
        }
        return n;
    }

    /// `flip_due`: `distinct ≥ 2 ∧ total > N_floor`.
    pub fn due(self: *const DirCensus, n_floor: u32) bool {
        return self.distinct() >= 2 and self.total() > n_floor;
    }

    /// The creators heaviest first (the suppliers' order).
    pub fn heaviest(self: *const DirCensus, out: []u32) []u32 {
        var ids: [2 * MAX_CREATORS]u32 = undefined;
        var w: [2 * MAX_CREATORS]u32 = undefined;
        var n: usize = 0;
        for ([_]*const Bucket{ &self.cur, &self.prev }) |b| {
            for (b.creators[0..b.n], b.ops[0..b.n]) |c, o| {
                var found = false;
                for (ids[0..n], 0..) |x, i| if (x == c) {
                    w[i] += o;
                    found = true;
                };
                if (!found) {
                    ids[n] = c;
                    w[n] = o;
                    n += 1;
                }
            }
        }
        // Insertion sort by weight descending (≤ 16 entries).
        var i: usize = 1;
        while (i < n) : (i += 1) {
            var j = i;
            while (j > 0 and w[j - 1] < w[j]) : (j -= 1) {
                std.mem.swap(u32, &w[j - 1], &w[j]);
                std.mem.swap(u32, &ids[j - 1], &ids[j]);
            }
        }
        const m = @min(n, out.len);
        @memcpy(out[0..m], ids[0..m]);
        return out[0..m];
    }
};

/// `N_floor` as the census reads it: the handover judge's live derivation
/// where a slot plane is armed ("the striping trigger reads its window"),
/// else a fixed word (the harness's).
pub const Floor = union(enum) {
    fixed: u32,
    live: struct { ctx: *anyopaque, read: *const fn (ctx: *anyopaque) u32 },

    pub fn now(self: Floor) u32 {
        return switch (self) {
            .fixed => |f| f,
            .live => |l| l.read(l.ctx),
        };
    }
};

/// The per-mount trigger state: a bounded census (least-recently-touched
/// evicted at the cap) and the single-flight set.
pub const Census = struct {
    io: Io,
    lock: sync.Mutex = .init,
    dirs: std.AutoHashMapUnmanaged(u64, DirCensus) = .empty,
    in_flight: std.AutoHashMapUnmanaged(u64, void) = .empty,
    t_idle_ns: u64 = T_IDLE_NS,
    floor: Floor,

    pub fn init(gpa: Allocator, io: Io, floor: Floor) Allocator.Error!Census {
        var c: Census = .{ .io = io, .floor = floor };
        try c.dirs.ensureTotalCapacity(gpa, CENSUS_DIRS_MAX + 1);
        try c.in_flight.ensureTotalCapacity(gpa, 64);
        return c;
    }

    pub fn deinit(self: *Census, gpa: Allocator) void {
        self.dirs.deinit(gpa);
        self.in_flight.deinit(gpa);
    }

    /// A served insert into `dir` by `creator`: due ⇒ the heaviest
    /// creators into `suppliers` and `true` (the caller flips, contained).
    pub fn noteServedInsert(self: *Census, dir: u64, creator: u32, now_ns: u64, suppliers: []u32) ?[]u32 {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        if (self.in_flight.contains(dir)) return null;
        const gop = self.dirs.getOrPutAssumeCapacity(dir);
        if (!gop.found_existing) {
            gop.value_ptr.* = .{};
            if (self.dirs.count() > CENSUS_DIRS_MAX) self.evictOldest(dir);
            stats.dir_stripe_census_dirs.set(self.dirs.count());
        }
        const c = self.dirs.getPtr(dir).?;
        c.note(creator, now_ns, self.t_idle_ns);
        if (!c.due(self.floor.now())) return null;
        const out = c.heaviest(suppliers);
        self.in_flight.putAssumeCapacity(dir, {});
        return out;
    }

    fn evictOldest(self: *Census, keep: u64) void {
        var oldest: ?u64 = null;
        var oldest_ns: u64 = std.math.maxInt(u64);
        var it = self.dirs.iterator();
        while (it.next()) |e| {
            if (e.key_ptr.* == keep) continue;
            if (e.value_ptr.last_touch_ns < oldest_ns) {
                oldest_ns = e.value_ptr.last_touch_ns;
                oldest = e.key_ptr.*;
            }
        }
        if (oldest) |o| _ = self.dirs.remove(o);
    }

    /// The flip finished (or failed): the directory leaves the census.
    pub fn flipDone(self: *Census, dir: u64) void {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        _ = self.in_flight.remove(dir);
        _ = self.dirs.remove(dir);
        stats.dir_stripe_census_dirs.set(self.dirs.count());
    }
};

// ---------------------------------------------------------------------------
// The directory sources behind the merge and the fold.
// ---------------------------------------------------------------------------

/// One dentry as a stream yields it (cookie order).
pub const DirRec = struct { cookie: u64, child: u64, file_type: u8, name: []const u8 };

/// A directory's dentry pages AT ITS HOLDER (local: the store's range;
/// foreign: the token divert's dentry set), strictly after `after`.
pub const DirSource = struct {
    ctx: *anyopaque,
    /// Appends ≤ `max` entries after cookie `after` into `out` (names
    /// allocated from `arena`); returns whether MORE remain.
    page: *const fn (ctx: *anyopaque, arena: Allocator, dir: u64, after: u64, max: usize, out: *std.ArrayList(DirRec)) errors.Error!bool,
    /// The record of `ino` at its holder (null = absent; an unreachable
    /// holder is the error).
    inode: *const fn (ctx: *anyopaque, ino: u64) errors.Error!?InodeValue,
};

/// The dentry records of a KEY range as `DirRec`s (the local source's
/// page over a `Store`).
pub fn pageFromStore(comptime Store: type, store: *Store, arena: Allocator, dir: u64, after: u64, max: usize, out: *std.ArrayList(DirRec)) errors.Error!bool {
    const dk = route_table.effectiveIno(dir);
    // The scan starts AT the cookie's key and skips it: strictly after.
    const start = switch (key.decodeReaddirCookie(after) catch return error.InvalidArgument) {
        .start, .after_dot, .after_dotdot => key.dentryKey(dk, 0, 0),
        .after_entry => |e| key.dentryKey(dk, e.hash54, e.coll_seq),
    };
    const end = key.dentryKey(dk, key.HASH54_MAX, std.math.maxInt(u8));
    var kvs: std.ArrayList(tree.KeyValue) = .empty;
    defer tree.deinitKeyValues(arena, &kvs);
    store.range(arena, key.TREE_DENTRIES, &start, &end, max + 2, &kvs) catch return error.InputOutput;
    var n: usize = 0;
    var more = false;
    for (kvs.items) |kv| {
        const k = key.DentryKey.decode(kv.key) catch return error.InputOutput;
        const cookie = key.encodeReaddirCookie(k.hash54(), k.collSeq());
        if (cookie <= after) continue;
        if (n == max) {
            more = true;
            break;
        }
        const dv = record.DentryValue.decode(kv.value) catch return error.InputOutput;
        try out.append(arena, .{ .cookie = cookie, .child = dv.child_ino, .file_type = dv.file_type, .name = try arena.dupe(u8, dv.name) });
        n += 1;
    }
    return more;
}

/// Read `D`'s map off its own dentry set.
pub fn readMap(src: DirSource, arena: Allocator, dir: u64) errors.Error!?StripeMap {
    var recs: std.ArrayList(DirRec) = .empty;
    var after: u64 = 0;
    var markers: std.ArrayList(Marker) = .empty;
    while (true) {
        recs.clearRetainingCapacity();
        const more = try src.page(src.ctx, arena, dir, after, MIGRATION_SCAN_PAGE, &recs);
        for (recs.items) |r| {
            if (isMarkerName(r.name)) try markers.append(arena, .{ .name = r.name, .child = r.child });
            after = r.cookie;
        }
        if (!more or recs.items.len == 0) break;
    }
    return decodeMap(dir, markers.items);
}

/// `page_names(D, 0, 1)`: does `D`'s own tree hold a USER name (markers
/// skipped)?
pub fn hasUserName(src: DirSource, arena: Allocator, dir: u64) errors.Error!bool {
    var recs: std.ArrayList(DirRec) = .empty;
    var after: u64 = 0;
    while (true) {
        recs.clearRetainingCapacity();
        const more = try src.page(src.ctx, arena, dir, after, MIGRATION_SCAN_PAGE, &recs);
        for (recs.items) |r| {
            if (!isMarkerName(r.name)) return true;
            after = r.cookie;
        }
        if (!more or recs.items.len == 0) return false;
    }
}

/// `stripe_locate`: the stripe first, then `D`'s own tree (LWW: the
/// stripe wins).
pub fn locate(src: DirSource, arena: Allocator, map: *const StripeMap, name: []const u8, name_key: *const key.NameKey) errors.Error!?DirRec {
    const r = map.routeName(name, name_key);
    if (try findName(src, arena, r.stripe, name)) |d| return d;
    return findName(src, arena, map.dir, name);
}

fn findName(src: DirSource, arena: Allocator, dir: u64, name: []const u8) errors.Error!?DirRec {
    var recs: std.ArrayList(DirRec) = .empty;
    var after: u64 = 0;
    while (true) {
        recs.clearRetainingCapacity();
        const more = try src.page(src.ctx, arena, dir, after, MIGRATION_SCAN_PAGE, &recs);
        for (recs.items) |r| {
            if (std.mem.eql(u8, r.name, name)) return r;
            after = r.cookie;
        }
        if (!more or recs.items.len == 0) return null;
    }
}

// ---------------------------------------------------------------------------
// The K-way merge.
// ---------------------------------------------------------------------------

const Stream = struct {
    dir: u64,
    recs: std.ArrayList(DirRec) = .empty,
    pos: usize = 0,
    after: u64,
    more: bool = true,

    fn head(self: *const Stream) ?DirRec {
        return if (self.pos < self.recs.items.len) self.recs.items[self.pos] else null;
    }

    fn refill(self: *Stream, src: DirSource, arena: Allocator, page: usize) errors.Error!void {
        if (!self.more) return;
        self.recs.clearRetainingCapacity();
        self.pos = 0;
        self.more = try src.page(src.ctx, arena, self.dir, self.after, page, &self.recs);
        if (self.recs.items.len > 0) self.after = self.recs.items[self.recs.items.len - 1].cookie;
        if (self.recs.items.len == 0) self.more = false;
    }
};

/// One merged page of a striped directory from `offset`: `K + 1` streams
/// (every stripe + `D`'s own tree) refilled only when consumed, in pages
/// of `max(max / (K + 1), floor)`; the smallest head cookie pops; an
/// entry from `D`'s own stream whose stripe holds the same name is the
/// LWW loser and skipped; markers never show. The returned cookie is the
/// exact continuation.
pub fn readdirMerged(src: DirSource, arena: Allocator, map: *const StripeMap, offset: u64, max: usize, name_key: *const key.NameKey, out: *std.ArrayList(DirRec)) errors.Error!void {
    stats.dir_stripe_readdir_merges.inc();
    const page = @max(max / (@as(usize, map.k) + 1), MERGE_STREAM_PAGE_FLOOR);
    var streams: [K_MAX + 1]Stream = undefined;
    const n = @as(usize, map.k) + 1;
    for (0..map.k) |i| streams[i] = .{ .dir = map.stripes[i], .after = offset };
    streams[map.k] = .{ .dir = map.dir, .after = offset };
    for (streams[0..n]) |*s| try s.refill(src, arena, page);
    while (out.items.len < max) {
        var best: ?usize = null;
        for (streams[0..n], 0..) |*s, i| {
            const h = s.head() orelse continue;
            if (best == null or h.cookie < streams[best.?].head().?.cookie) best = i;
        }
        const bi = best orelse break;
        const s = &streams[bi];
        const rec = s.head().?;
        s.pos += 1;
        if (s.head() == null) try s.refill(src, arena, page);
        if (isMarkerName(rec.name)) continue;
        if (bi == map.k) {
            // D's own tree: the LWW loser when its stripe holds the name.
            const r = map.routeName(rec.name, name_key);
            if (try findName(src, arena, r.stripe, rec.name) != null) continue;
        }
        try out.append(arena, rec);
    }
}

// ---------------------------------------------------------------------------
// fsck C17 (report-only).
// ---------------------------------------------------------------------------

pub const C17Shape = enum { multiply_mapped, missing_stripe, incomplete_map, misrouted_name, unmigrated_name };

pub const Finding = struct { shape: C17Shape, dir: u64, ino: u64 };

/// The census' view of one directory: its markers and its user names.
pub const DirView = struct {
    dir: u64,
    markers: []const Marker,
    /// `(name, child)` of every non-marker dentry.
    names: []const DirRec,
};

/// The five shapes over ONE dentry walk: `dirs` = every directory with a
/// NUL-led name (the nomination), `records` = the inode census (an ino
/// → present), `names_of(stripe)` the stripes' own names.
pub fn verifyStripes(gpa: Allocator, views: []const DirView, hasRecord: *const fn (ino: u64) bool, name_key: *const key.NameKey, out: *std.ArrayList(Finding)) Allocator.Error!void {
    var mapped: std.AutoHashMapUnmanaged(u64, u64) = .empty;
    defer mapped.deinit(gpa);
    for (views) |v| {
        if (v.markers.len == 0) continue;
        const map = decodeMap(v.dir, v.markers) orelse {
            try out.append(gpa, .{ .shape = .incomplete_map, .dir = v.dir, .ino = v.dir });
            continue;
        };
        for (map.slice()) |s| {
            if (!hasRecord(s)) try out.append(gpa, .{ .shape = .missing_stripe, .dir = v.dir, .ino = s });
            const gop = try mapped.getOrPut(gpa, s);
            if (gop.found_existing and gop.value_ptr.* != v.dir) {
                try out.append(gpa, .{ .shape = .multiply_mapped, .dir = v.dir, .ino = s });
            } else gop.value_ptr.* = v.dir;
        }
        if (!map.migrating) {
            for (v.names) |nm| try out.append(gpa, .{ .shape = .unmigrated_name, .dir = v.dir, .ino = nm.child });
        }
        // Every stripe's names route to it.
        for (views) |sv| {
            var idx: ?u8 = null;
            for (map.slice(), 0..) |s, i| if (s == sv.dir) {
                idx = @intCast(i);
            };
            const i = idx orelse continue;
            for (sv.names) |nm| if (stripeOf(key.dentryNameHash54(nm.name, name_key), map.k) != i) {
                try out.append(gpa, .{ .shape = .misrouted_name, .dir = sv.dir, .ino = nm.child });
            };
        }
    }
    if (out.items.len != 0) stats.fsck_stripe_findings.fire("a stripe map shape fsck C17 names");
}

// ---------------------------------------------------------------------------
// The flip, the re-home, the rmdir — over a plane.
// ---------------------------------------------------------------------------

/// A stripe's record: nameless, `S_IFDIR` with its directory's mode bits,
/// owner and stamp, `INODE_FLAG_STRIPE` set (the durable "I am a stripe"
/// every reader judges by — a stripe never carries a map, feeds no
/// census and is never flipped).
pub fn stripeRecord(attrs: ops.StripeAttrs) InodeValue {
    return .{ .mode = S.IFDIR | (attrs.mode & 0o7777), .uid = attrs.uid, .gid = attrs.gid, .nlink = 2, .flags = record.INODE_FLAG_STRIPE, .atime = attrs.ts_ns, .mtime = attrs.ts_ns, .ctime = attrs.ts_ns };
}

pub fn isStripeRecord(v: InodeValue) bool {
    return economy.isDirMode(v.mode) and v.flags & record.INODE_FLAG_STRIPE != 0;
}

/// The directory's words a stripe record is minted with.
pub fn attrsOf(dv: InodeValue) ops.StripeAttrs {
    return .{ .mode = dv.mode, .uid = dv.uid, .gid = dv.gid, .ts_ns = dv.mtime };
}

/// Write the stripe records of `inos` in ONE entry (the holder's
/// remainder under the flip's guard set; a creator's supplied batch —
/// brand-new inos nobody can reach need no guard of their own).
pub fn writeStripeRecords(comptime Store: type, store: *Store, inos_: []const u64, attrs: ops.StripeAttrs) errors.Error!void {
    const t = store.begin(@src()) catch return error.OutOfMemory;
    var owned = true;
    errdefer if (owned) store.abandon(t);
    const v = stripeRecord(attrs).encode();
    for (inos_) |ino| try t.stagePut(key.TREE_INODES, &key.inodeKey(route_table.effectiveIno(ino)), &v);
    owned = false;
    store.commit(t) catch |e| return Store.coreError(e);
}

/// Phase A's share: with `creators` suppliers each supplies `K /
/// (creators + 1)` stripes and the holder keeps the rest — the load
/// spreads over every writer of the directory from the flip (decision
/// 0003 §Q.2).
pub fn supplyShare(k: u8, creators: usize) u8 {
    return @intCast(@as(usize, k) / (creators + 1));
}

/// The holder's own mint of a stripe ino in ITS slot on `dir`'s volume
/// (the store's allocator behind the filesystem): null = no space.
pub const Minter = struct {
    ctx: *anyopaque,
    mint: *const fn (ctx: *anyopaque, dir: u64) ?u64,
};

/// `SupplyStripeInos` over a link: the creator's stripe inos for
/// `indices` of `dir` (in order; fewer = the rest declined).
pub fn supplyRemote(link: ops.Link, volume: u16, dir: u64, attrs: ops.StripeAttrs, indices: []const u8, out: []u64) errors.Error!usize {
    assert(indices.len >= 1 and indices.len <= K_MAX and out.len >= indices.len);
    const rep = try link.call(link.ctx, .{ .supply_stripe_inos = .{ .head = .{ .volume = volume, .n = @intCast(indices.len), .reserved = 0, .dir = dir, .mode = attrs.mode, .uid = attrs.uid, .gid = attrs.gid, .reserved2 = 0, .ts_ns = attrs.ts_ns }, .indices = indices } });
    const items = rep.supply_stripe_inos.items;
    const n = @min(items.len, indices.len);
    for (items[0..n], 0..) |ino, i| out[i] = ino;
    return n;
}

/// The served side of `SupplyStripeInos` (a creator's wire face): the
/// records minted in THIS mount's slot on the directory's volume through
/// the plane's striping face; a plane without the face declines (0).
pub fn SupplyService(comptime Store: type) type {
    return struct {
        const Self = @This();
        plane: *ops.Plane(Store),

        pub fn register(self: *Self, svc: *service.Service) void {
            svc.register(.supply_stripe_inos, .{ .ctx = self, .serve = &serve });
        }

        fn serve(ctx: *anyopaque, req: *const service.Request, scratch: []u8) service.Answer {
            const self: *Self = @ptrCast(@alignCast(ctx));
            const r = req.payload.supply_stripe_inos;
            if (r.head.volume != self.plane.volume) return service.Answer.rejected("supply_stripe_inos names a volume this node does not hold");
            const face = self.plane.striping orelse return .{ .ok = .{ .supply_stripe_inos = .{ .head = .{ .n = 0, .reserved = @splat(0) }, .items = &.{} } } };
            var minted: [K_MAX]u64 = undefined;
            const n = face.supply(face.ctx, r.head.dir, .{ .mode = r.head.mode, .uid = r.head.uid, .gid = r.head.gid, .ts_ns = r.head.ts_ns }, r.indices, minted[0..r.indices.len]);
            // The reply follows this creator's durability lane: the
            // holder's flip names these records in its map, and a stripe
            // whose record died with its creator's ring is C17's
            // missing-stripe.
            if (n != 0) self.plane.apply.barrier() catch return service.Answer.refused(error.InputOutput, "the supplied stripe records could not be made durable");
            stats.dir_stripe_supplies_served.inc();
            const out = std.mem.bytesAsSlice(u64, scratch[0 .. n * @sizeOf(u64)]);
            for (minted[0..n], 0..) |ino, i| out[i] = ino;
            return .{ .ok = .{ .supply_stripe_inos = .{ .head = .{ .n = @intCast(n), .reserved = @splat(0) }, .items = out } } };
        }
    };
}

pub fn Striping(comptime Store: type) type {
    return struct {
        const Self = @This();
        const Plane = ops.Plane(Store);

        plane: *Plane,
        src: DirSource,
        minter: Minter,
        census: Census,
        k: u8,
        /// The last fold persisted per directory (the cadence bound).
        persisted: std.AutoHashMapUnmanaged(u64, u64) = .empty,
        /// What a flip the census triggered hands back (the filesystem's
        /// map cache, the migration's queue); null = the harness.
        flipped: ?FlippedSink = null,
        /// Directories the census found due, with their suppliers: the
        /// flips run on the cadence (`runPending`), NEVER inside the served
        /// step that made them due — that step's initiator still holds its
        /// travelling scope over `I{D}` at this holder while it waits for
        /// the step's reply, and the flip's own `I{D}` would wait for it
        /// (the scope's expiry sweep was the only way out: 45 s of every
        /// create into the directory parked).
        pending_lock: sync.Mutex = .init,
        pending: std.ArrayList(PendingFlip) = .empty,

        pub const FlippedSink = struct {
            ctx: *anyopaque,
            note: *const fn (ctx: *anyopaque, map: StripeMap) void,
        };

        const PendingFlip = struct { dir: u64, suppliers: [DirCensus.MAX_CREATORS * 2]u32, n: u8 };

        pub const Error = ops.Error || error{ NotADirectory, AlreadyStriped, IsAStripe, StripingOff, NotHolder, NotEmpty, Changed };

        pub fn init(plane: *Plane, src: DirSource, minter: Minter, k: u8, floor: Floor) Allocator.Error!Self {
            return .{ .plane = plane, .src = src, .minter = minter, .census = try Census.init(plane.gpa, plane.io, floor), .k = k };
        }

        pub fn deinit(self: *Self) void {
            self.census.deinit(self.plane.gpa);
            self.persisted.deinit(self.plane.gpa);
            self.pending.deinit(self.plane.gpa);
        }

        /// The served-insert census hook (the ops plane's sink): due ⇒ flip.
        pub fn servedInsertSink(self: *Self) ops.ServedInsertSink {
            return .{ .ctx = self, .note = &noteServed };
        }

        fn noteServed(ctx: *anyopaque, parent: u64, creator: u32) void {
            const self: *Self = @ptrCast(@alignCast(ctx));
            if (self.k <= 1) return;
            var pf: PendingFlip = .{ .dir = parent, .suppliers = undefined, .n = 0 };
            const sup = self.census.noteServedInsert(parent, creator, clock.monotonicNs(), &pf.suppliers) orelse return;
            pf.n = @intCast(sup.len);
            self.pending_lock.lockUncancelable(self.plane.io);
            defer self.pending_lock.unlock(self.plane.io);
            self.pending.append(self.plane.gpa, pf) catch self.census.flipDone(parent);
        }

        /// The cadence's half of the trigger: every directory the census
        /// found due is flipped now, contained (a failure is named loud and
        /// the directory re-enters the census at its next served insert).
        pub fn runPending(self: *Self) void {
            while (true) {
                const pf = blk: {
                    self.pending_lock.lockUncancelable(self.plane.io);
                    defer self.pending_lock.unlock(self.plane.io);
                    break :blk self.pending.pop() orelse return;
                };
                defer self.census.flipDone(pf.dir);
                const map = self.flip(pf.dir, self.k, pf.suppliers[0..pf.n]) catch |e| {
                    stats.dir_stripe_flip_failures.inc();
                    log.warn("directory {d}: the census flip over {d} stripes with {d} supplier(s) did not land ({t}); the directory re-enters the census at its next served insert", .{ pf.dir, self.k, pf.n, e });
                    continue;
                };
                if (self.flipped) |f| f.note(f.ctx, map);
            }
        }

        /// The flip, on the HOLDER of `D`: refuse off / not a directory /
        /// already striped / a stripe; phases A–D; ONE intent.
        pub fn flip(self: *Self, dir: u64, k: u8, suppliers: []const u32) Error!StripeMap {
            if (k <= 1) return error.StripingOff;
            if (!self.plane.isLocal(dir)) return error.NotHolder;
            var arena_state = std.heap.ArenaAllocator.init(self.plane.gpa);
            defer arena_state.deinit();
            const arena = arena_state.allocator();
            const dv = (self.plane.apply.readInode(dir) catch return error.Io) orelse return error.Corrupt;
            if (!economy.isDirMode(dv.mode)) return error.NotADirectory;
            if (isStripeRecord(dv)) return error.IsAStripe;
            if ((readMap(self.src, arena, dir) catch return error.Io) != null) return error.AlreadyStriped;
            const attrs = attrsOf(dv);
            // Phase A: the supplied stripes — one BATCH per creator
            // (heaviest first), each its share of the indices; a creator
            // that declines or is unreachable leaves its batch to the
            // holder.
            var stripes: [K_MAX]u64 = @splat(0);
            var supplied: [K_MAX]bool = @splat(false);
            var creators: usize = 0;
            for (suppliers) |c| {
                if (c != self.plane.own_id) creators += 1;
            }
            const share = supplyShare(k, creators);
            var next: u8 = 0;
            if (share > 0) for (suppliers) |c| {
                if (c == self.plane.own_id) continue;
                if (next + share > k) break;
                var indices: [K_MAX]u8 = undefined;
                for (0..share) |i| indices[i] = next + @as(u8, @intCast(i));
                stats.dir_stripe_supply_rpcs.inc();
                const link = self.plane.resolver.linkOf(self.plane.resolver.ctx, c);
                const got = if (link) |l| supplyRemote(l, self.plane.volume, dir, attrs, indices[0..share], stripes[next .. next + share]) catch 0 else 0;
                if (got < share) stats.dir_stripe_supply_declined.inc();
                stats.dir_stripe_supplied.add(got);
                for (0..got) |i| supplied[next + i] = true;
                next += share;
            };
            // Phase B: the remainder's inos in the holder's slot.
            var own: [K_MAX]u64 = undefined;
            var n_own: usize = 0;
            for (0..k) |i| if (!supplied[i]) {
                stripes[i] = self.minter.mint(self.minter.ctx, dir) orelse return error.NoSpace;
                own[n_own] = stripes[i];
                n_own += 1;
            };
            // Phase C: the ONE guard set — I{D} + I{D_i} minted here + every
            // marker name — canonical, taken once.
            var names: [K_MAX][MARKER_NAME_LEN]u8 = undefined;
            var inodes: [dlm.wide_requests]dlm.InodeReq = undefined;
            var dents: [dlm.wide_requests]dlm.DentryReq = undefined;
            var ni: usize = 0;
            var nd: usize = 0;
            inodes[ni] = .{ .ino = dir, .mode = .exclusive };
            ni += 1;
            for (own[0..n_own]) |s| {
                inodes[ni] = .{ .ino = s, .mode = .exclusive };
                ni += 1;
            }
            for (0..k) |i| {
                dents[nd] = .{ .parent = dir, .name = markerName(@intCast(i), &names[i]), .mode = .exclusive };
                nd += 1;
            }
            dents[nd] = .{ .parent = dir, .name = MIGRATING, .mode = .exclusive };
            nd += 1;
            dents[nd] = .{ .parent = dir, .name = STRIPED, .mode = .exclusive };
            nd += 1;
            var held = self.plane.locks.lockManyWide(self.plane.io, inodes[0..ni], dents[0..nd]) catch return error.PlanTooLarge;
            defer held.release(self.plane.io);
            // Phase D: the remainder's records under the set, ONE entry.
            if (n_own > 0) writeStripeRecords(Store, self.plane.apply.store, own[0..n_own], attrs) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.SlotMoved => error.SlotMoved,
                error.Deferred => error.Deferred,
                else => error.Io,
            };
            // ONE intent: the map, the commit marker last.
            var plan_names: [K_MAX][MARKER_NAME_LEN]u8 = undefined;
            const plan = planFlip(dir, stripes[0..k], &plan_names);
            const done = try self.plane.execute(&plan, 0, dv.mtime);
            if (done.refused) |_| return error.Changed;
            stats.dir_stripe_flips.inc();
            stats.dir_striped_dirs.add(1);
            var map: StripeMap = .{ .dir = dir, .k = k, .stripes = @splat(0), .migrating = true };
            @memcpy(map.stripes[0..k], stripes[0..k]);
            return map;
        }

        /// The key parent of a MUTATION of `name` under striped `D`: the
        /// stripe — a name still in `D`'s tree is re-homed FIRST (one
        /// intent: insert into the stripe, remove from `D`; a FILE loser
        /// goes with its count), then the caller revalidates against the
        /// stripe alone.
        pub fn mutationParent(self: *Self, map: *const StripeMap, name: []const u8, now: u64) Error!u64 {
            const r = map.routeName(name, self.plane.apply.store.nameKey());
            var arena_state = std.heap.ArenaAllocator.init(self.plane.gpa);
            defer arena_state.deinit();
            const arena = arena_state.allocator();
            const in_dir = (findName(self.src, arena, map.dir, name) catch return error.Io) orelse return r.stripe;
            try self.migrateOne(map, r.stripe, in_dir, now);
            stats.dir_stripe_rehomed_on_touch.inc();
            return r.stripe;
        }

        fn migrateOne(self: *Self, map: *const StripeMap, stripe: u64, rec: DirRec, now: u64) Error!void {
            var arena_state = std.heap.ArenaAllocator.init(self.plane.gpa);
            defer arena_state.deinit();
            const arena = arena_state.allocator();
            var loser: ?struct { ino: u64, nlink: u32 } = null;
            if (findName(self.src, arena, stripe, rec.name) catch return error.Io) |winner| {
                if (winner.child != rec.child) {
                    // The LWW loser: a file goes with its count; a directory
                    // is LEFT and named loud (C17 `unmigrated-name`).
                    const lv = (self.plane.readWitness(rec.child) catch null) orelse return;
                    if (economy.isDirMode(lv.mode)) return;
                    loser = .{ .ino = rec.child, .nlink = lv.nlink };
                }
            }
            const plan = if (loser) |l|
                planMigrateOne(map.dir, stripe, rec.name, rec.child, @as(u32, rec.file_type) << 12, .{ .ino = l.ino, .nlink = l.nlink }, now)
            else
                planMigrateOne(map.dir, stripe, rec.name, rec.child, @as(u32, rec.file_type) << 12, null, now);
            const scope = self.plane.mintScope();
            var held = try self.plane.acquirePlanGuards(&plan, scope, false);
            defer held.release(self.plane.gpa);
            _ = try self.plane.execute(&plan, scope, now);
            stats.dir_stripe_migrated_names.inc();
        }

        /// The background migration: every non-marker name in `D`'s tree
        /// moved to its stripe, ≤ `MIGRATION_ROUNDS_MAX` rounds; the
        /// `migrating` marker clears when `D`'s tree is re-verified empty of
        /// user names under the exclusive guard.
        pub fn migrate(self: *Self, map: *StripeMap, now: u64) Error!u32 {
            var moved: u32 = 0;
            var rounds: u32 = 0;
            while (rounds < MIGRATION_ROUNDS_MAX) : (rounds += 1) {
                const page = try self.migratePage(map, now);
                moved += page;
                if (page == 0) break;
            }
            _ = try self.clearMigrating(map, now);
            return moved;
        }

        /// ONE round of the migration (the cadence's unit): a page of
        /// names moved; an empty page re-verifies `D` and clears the
        /// marker — true when the directory is migrated.
        pub fn migrateRound(self: *Self, map: *StripeMap, now: u64) Error!bool {
            if (try self.migratePage(map, now) != 0) return false;
            return self.clearMigrating(map, now);
        }

        fn migratePage(self: *Self, map: *const StripeMap, now: u64) Error!u32 {
            if (!self.plane.isLocal(map.dir)) return error.NotHolder;
            const name_key = self.plane.apply.store.nameKey();
            var arena_state = std.heap.ArenaAllocator.init(self.plane.gpa);
            defer arena_state.deinit();
            const arena = arena_state.allocator();
            var recs: std.ArrayList(DirRec) = .empty;
            _ = self.src.page(self.src.ctx, arena, map.dir, 0, MIGRATION_SCAN_PAGE, &recs) catch return error.Io;
            var moved: u32 = 0;
            for (recs.items) |r| {
                if (isMarkerName(r.name)) continue;
                try self.migrateOne(map, map.route(key.dentryNameHash54(r.name, name_key)).stripe, r, now);
                moved += 1;
            }
            return moved;
        }

        /// Clear `migrating` under I{D} + D{D, migrating}, re-verified
        /// EMPTY of user names under the guard; false when a name remains.
        fn clearMigrating(self: *Self, map: *StripeMap, now: u64) Error!bool {
            var held = self.plane.locks.lockManyUncancelable(self.plane.io, &.{.{ .ino = map.dir, .mode = .exclusive }}, &.{.{ .parent = map.dir, .name = MIGRATING, .mode = .exclusive }}) catch return error.PlanTooLarge;
            defer held.release(self.plane.io);
            var arena_state = std.heap.ArenaAllocator.init(self.plane.gpa);
            defer arena_state.deinit();
            if (hasUserName(self.src, arena_state.allocator(), map.dir) catch return error.Io) return false;
            if (map.migrating) _ = self.plane.apply.apply(.{ .remove_dentry = .{ .parent = map.dir, .name = MIGRATING, .expect_child = map.stripes[0], .parent_update = .none } }, .none, .none, now) catch return error.Io;
            map.migrating = false;
            return true;
        }

        /// `stat D`: every stripe's record at its holder, folded; the
        /// times persisted onto `D` once per cadence (the holder only).
        pub fn statFold(self: *Self, map: *const StripeMap, d: InodeValue, now: u64) Error!InodeValue {
            var vals: [K_MAX]?InodeValue = undefined;
            for (map.slice(), 0..) |s, i| vals[i] = self.src.inode(self.src.ctx, s) catch null;
            const folded = foldAttrs(d, vals[0..map.k]);
            if (self.plane.isLocal(map.dir) and (economy.laterThan(folded.mtime, d.mtime) or economy.laterThan(folded.ctime, d.ctime))) {
                const last = self.persisted.get(map.dir) orelse 0;
                if (now -| last >= ops.CADENCE_TICK_NS) {
                    var v = d;
                    v.mtime = folded.mtime;
                    v.ctime = folded.ctime;
                    const t = self.plane.apply.store.begin(@src()) catch return error.OutOfMemory;
                    t.stagePut(key.TREE_INODES, &key.inodeKey(route_table.effectiveIno(map.dir)), &v.encode()) catch return error.OutOfMemory;
                    self.plane.apply.store.commit(t) catch return error.Io;
                    self.persisted.put(self.plane.gpa, map.dir, now) catch return error.OutOfMemory;
                    stats.dir_stripe_time_batches.inc();
                }
            }
            return folded;
        }

        /// `rmdir D` (the striped shape): ONE exclusive set over the
        /// parent's name, `D`, every stripe (travelling) and every marker;
        /// having written nothing — the dentry, the map and every stripe's
        /// emptiness revalidated; then ONE intent; the stripes' corpses
        /// destroyed after the guards drop.
        pub fn rmdir(self: *Self, parent: u64, name: []const u8, now: u64) Error!void {
            var arena_state = std.heap.ArenaAllocator.init(self.plane.gpa);
            defer arena_state.deinit();
            const arena = arena_state.allocator();
            const d = (findName(self.src, arena, parent, name) catch return error.Io) orelse return error.Corrupt;
            const map0 = (readMap(self.src, arena, d.child) catch return error.Io) orelse return error.Changed;
            // The ONE guard set: I{parent} + D{parent,name} + I{D} + every
            // I{D_i} + every D{D, marker} — one acquisition per table.
            var names: [K_MAX][MARKER_NAME_LEN]u8 = undefined;
            var inodes: [dlm.wide_requests]dlm.InodeReq = undefined;
            var dents: [dlm.wide_requests]dlm.DentryReq = undefined;
            var ni: usize = 0;
            var nd: usize = 0;
            inodes[ni] = .{ .ino = parent, .mode = .exclusive };
            ni += 1;
            inodes[ni] = .{ .ino = d.child, .mode = .exclusive };
            ni += 1;
            for (map0.slice()) |s| {
                inodes[ni] = .{ .ino = s, .mode = .exclusive };
                ni += 1;
            }
            dents[nd] = .{ .parent = parent, .name = name, .mode = .exclusive };
            nd += 1;
            for (0..map0.k) |i| {
                dents[nd] = .{ .parent = d.child, .name = markerName(@intCast(i), &names[i]), .mode = .exclusive };
                nd += 1;
            }
            dents[nd] = .{ .parent = d.child, .name = MIGRATING, .mode = .exclusive };
            nd += 1;
            dents[nd] = .{ .parent = d.child, .name = STRIPED, .mode = .exclusive };
            nd += 1;
            const scope = self.plane.mintScope();
            var held = @import("guards.zig").acquire(self.plane.gpa, self.plane.io, self.plane.locks, self.plane.tables(), .{ .own_id = self.plane.own_id, .volume = self.plane.volume, .scope = scope, .inodes = inodes[0..ni], .dentries = dents[0..nd] }) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.PlanTooLarge => error.PlanTooLarge,
                error.HolderUnreachable => error.ShipFailed,
                else => error.GuardRefused,
            };
            defer held.release(self.plane.gpa);
            // Revalidate under the set, having written nothing.
            const d2 = (findName(self.src, arena, parent, name) catch return error.Io) orelse return error.Corrupt;
            if (d2.child != d.child) return error.Changed;
            const map = (readMap(self.src, arena, d.child) catch return error.Io) orelse return error.Changed;
            if (map.k != map0.k or !std.mem.eql(u64, map.slice(), map0.slice())) return error.Changed;
            const dv = (self.plane.readWitness(d.child) catch return error.Io) orelse return error.Corrupt;
            if (hasUserName(self.src, arena, d.child) catch return error.Io) return error.NotEmpty;
            var nlinks: [K_MAX]u32 = undefined;
            for (map.slice(), 0..) |s, i| {
                const sv = (self.plane.readWitness(s) catch return error.Io) orelse {
                    nlinks[i] = 0;
                    continue;
                };
                if (sv.nlink != 0 and sv.nlink != 2) return error.NotEmpty;
                // `IsEmpty` under the travelling exclusive guard: exact.
                if (hasUserName(self.src, arena, s) catch return error.Io) return error.NotEmpty;
                nlinks[i] = sv.nlink;
            }
            var plan_names: [K_MAX][MARKER_NAME_LEN]u8 = undefined;
            const plan = planRmdirStriped(parent, name, &map, nlinks[0..map.k], dv.nlink, &plan_names, now);
            const done = try self.plane.execute(&plan, scope, now);
            if (done.refused) |_| return error.Changed;
            stats.dir_stripe_rmdirs.inc();
            stats.dir_striped_dirs.sub(1);
            // Hygiene: the local stripes' corpses go now (a foreign one is
            // its holder's nlink-0 corpse, swept there).
            for (map.slice()) |s| if (self.plane.isLocal(s)) {
                self.plane.apply.destroyUnnamed(s, .none) catch {};
            };
        }
    };
}

// ---------------------------------------------------------------------------
// Tests: the pure arms (the plane-bound contracts ride `two_holders.zig`).
// ---------------------------------------------------------------------------

const testing = std.testing;

test "markers: exactly two digits; the map decodes iff the commit marker names stripe 0 over a contiguous index set and no marker names D; stripe(name) = hash54 % K; the explicit K clamps 2..=64" {
    var buf: [MARKER_NAME_LEN]u8 = undefined;
    try testing.expectEqualStrings("\x00sqz.stripe:07", markerName(7, &buf));
    try testing.expectEqual(@as(?u8, 7), stripeIndexOfMarker("\x00sqz.stripe:07"));
    try testing.expectEqual(@as(?u8, 63), stripeIndexOfMarker("\x00sqz.stripe:63"));
    try testing.expectEqual(@as(?u8, null), stripeIndexOfMarker("\x00sqz.stripe:5"));
    try testing.expectEqual(@as(?u8, null), stripeIndexOfMarker("\x00sqz.stripe:005"));
    try testing.expectEqual(@as(?u8, null), stripeIndexOfMarker("\x00sqz.stripe:64"));
    try testing.expectEqual(@as(?u8, null), stripeIndexOfMarker("\x00sqz.stripe:a1"));
    try testing.expect(isMarkerName(MIGRATING) and isMarkerName(STRIPED) and !isMarkerName("sqz.striped"));
    const m = [_]Marker{
        .{ .name = "\x00sqz.stripe:00", .child = 100 },
        .{ .name = "\x00sqz.stripe:01", .child = 101 },
        .{ .name = "\x00sqz.stripe:02", .child = 102 },
        .{ .name = MIGRATING, .child = 100 },
        .{ .name = STRIPED, .child = 100 },
    };
    const map = decodeMap(9, &m).?;
    try testing.expectEqual(@as(u8, 3), map.k);
    try testing.expect(map.migrating);
    try testing.expectEqual(@as(u64, 101), map.route(7).stripe);
    try testing.expect(map.isStripe(102) and !map.isStripe(9));
    // No commit marker; a gap; the marker naming D; stripe 0 ≠ commit.
    try testing.expect(decodeMap(9, m[0..4]) == null);
    try testing.expect(decodeMap(9, &.{ m[0], m[2], m[4] }) == null);
    try testing.expect(decodeMap(100, &m) == null);
    try testing.expect(decodeMap(9, &.{ m[0], m[1], .{ .name = STRIPED, .child = 101 } }) == null);
    try testing.expectEqual(@as(u8, 1), stripeOf(65, 64));
    try testing.expectEqual(@as(u8, 0), stripeOf(65, 0));
    try testing.expectEqual(@as(u8, 2), clampExplicitK("1", 64));
    try testing.expectEqual(@as(u8, 64), clampExplicitK("900", 64));
    try testing.expectEqual(@as(u8, 16), clampExplicitK(" 16\n", 64));
    try testing.expectEqual(@as(u8, 64), clampExplicitK("x", 64));
}

test "the plans: the flip is K + 2 inserts with the commit marker LAST under the exclusive times; the migration inserts into the stripe FIRST; the one-intent rmdir removes the commit marker FIRST and marks every count-2 stripe dying; the fold's nlink and times; N_floor's shape" {
    var names: [K_MAX][MARKER_NAME_LEN]u8 = undefined;
    const stripes = [_]u64{ 100, 101, 102 };
    const fp = planFlip(9, &stripes, &names);
    try testing.expectEqual(@as(usize, 5), fp.len);
    try testing.expectEqualStrings(STRIPED, fp.steps[4].insert_dentry.name);
    try testing.expectEqual(steps.ParentUpdate.exclusive_times, fp.steps[4].insert_dentry.parent_update);
    try testing.expectEqual(steps.ParentUpdate.none, fp.steps[0].insert_dentry.parent_update);
    try testing.expectEqual(@as(u64, 100), fp.steps[3].insert_dentry.child);
    const mp = planMigrateOne(9, 101, "n", 500, S.IFREG, .{ .ino = 501, .nlink = 1 }, 7);
    try testing.expectEqual(@as(usize, 3), mp.len);
    try testing.expectEqual(steps.Kind.insert_dentry, @as(steps.Kind, mp.steps[0]));
    try testing.expectEqual(@as(u64, 101), mp.steps[0].insert_dentry.parent);
    try testing.expectEqual(@as(u32, 0), mp.steps[2].set_nlink.post);
    var map: StripeMap = .{ .dir = 9, .k = 3, .stripes = @splat(0), .migrating = true };
    @memcpy(map.stripes[0..3], &stripes);
    const rp = planRmdirStriped(1, "d", &map, &.{ 2, 0, 2 }, 5, &names, 7);
    // parent name, striped, migrating, 3 markers, 2 dying marks, D's own.
    try testing.expectEqual(@as(usize, 1 + 1 + 1 + 3 + 2 + 1), rp.len);
    try testing.expectEqualStrings(STRIPED, rp.steps[1].remove_dentry.name);
    try testing.expectEqual(@as(u64, 100), rp.steps[6].set_nlink.ino);
    try testing.expectEqual(@as(u64, 9), rp.steps[8].set_nlink.ino);
    try testing.expectEqual(@as(u32, 5), rp.steps[8].set_nlink.pre);
    // The WIDEST plan — the K = 64 rmdir under a NAME_MAX parent name with
    // every stripe at count 2 and `migrating` set — encodes inside the
    // image bound (the smoke's `rm -rf` of a 64-way directory answered
    // EINVAL off a 56 B-per-step buffer: the marker names pad to 16).
    {
        var wide: StripeMap = .{ .dir = 9, .k = K_MAX, .stripes = @splat(0), .migrating = true };
        for (0..K_MAX) |i| wide.stripes[i] = 1000 + i;
        const twos: [K_MAX]u32 = @splat(2);
        const long_name: [record.NAME_MAX]u8 = @splat('n');
        const wp = planRmdirStriped(1, &long_name, &wide, &twos, 2, &names, 7);
        try testing.expectEqual(@as(usize, 1 + 1 + 1 + K_MAX + K_MAX + 1), wp.len);
        var recs: [intent.XV_MAX_STEPS]intent.StepRec = undefined;
        var img: [intent.IMAGE_MAX]u8 = undefined;
        const image = try intent.encode(.rmdir, 0, 1, wp.recs(&recs), 16 << 10, &img);
        try testing.expect(image.len <= intent.IMAGE_MAX);
    }
    const folded = foldAttrs(.{ .nlink = 2, .mtime = 5, .ctime = 5 }, &.{ .{ .nlink = 4, .mtime = 9, .ctime = 1 }, null, .{ .nlink = 2, .mtime = 1, .ctime = 12 }, .{ .nlink = 1 } });
    try testing.expectEqual(@as(u32, 4), folded.nlink);
    try testing.expectEqual(@as(u64, 9), folded.mtime);
    try testing.expectEqual(@as(u64, 12), folded.ctime);
    try testing.expectEqual(@as(u32, 2), nFloor(10, 100, 0));
    try testing.expectEqual(@as(u32, 5), nFloor(500, 100, 0));
    try testing.expectEqual(@as(u32, 7), nFloor(0, 100, 100));
    try testing.expectEqual(@as(u32, 2), nFloor(0, 0, 0));
}

test "the census: due iff ≥ 2 distinct creators and total > N_floor over the half-window pair; a stale window drops out; the heaviest creators come first; the directory count is bounded; a flip in flight is single-flight" {
    var c: DirCensus = .{};
    const t: u64 = 1_000_000;
    c.note(7, 1, t);
    c.note(7, 2, t);
    c.note(7, 3, t);
    try testing.expect(!c.due(2));
    try testing.expectEqual(@as(u32, 1), c.distinct());
    c.note(8, 4, t);
    try testing.expect(c.due(2));
    try testing.expect(!c.due(4));
    var out: [4]u32 = undefined;
    try testing.expectEqualSlices(u32, &.{ 7, 8 }, c.heaviest(&out));
    // The next half-window keeps the previous; two later drops it.
    c.note(9, t / 2 + 1, t);
    try testing.expectEqual(@as(u32, 5), c.total());
    try testing.expectEqual(@as(u32, 3), c.distinct());
    c.note(9, 2 * t, t);
    try testing.expectEqual(@as(u32, 1), c.total());
    const gpa = testing.allocator;
    var census = try Census.init(gpa, testing.io, .{ .fixed = 2 });
    defer census.deinit(gpa);
    var sup: [4]u32 = undefined;
    try testing.expect(census.noteServedInsert(50, 1, 10, &sup) == null);
    try testing.expect(census.noteServedInsert(50, 1, 11, &sup) == null);
    // The third ship from a SECOND creator: total 3 > 2, distinct 2 ⇒ due.
    const due = census.noteServedInsert(50, 2, 12, &sup).?;
    try testing.expectEqual(@as(usize, 2), due.len);
    try testing.expectEqual(@as(u32, 1), due[0]);
    // In flight: no second trigger until done.
    try testing.expect(census.noteServedInsert(50, 3, 14, &sup) == null);
    census.flipDone(50);
    try testing.expect(census.noteServedInsert(50, 3, 15, &sup) == null);
    // The bound: CENSUS_DIRS_MAX + 1 directories evict the oldest.
    var d: u64 = 1000;
    while (d < 1000 + CENSUS_DIRS_MAX + 1) : (d += 1) _ = census.noteServedInsert(d, 1, d, &sup);
    try testing.expect(census.dirs.count() <= CENSUS_DIRS_MAX);
    try testing.expect(!census.dirs.contains(50));
}

test "fsck C17 finds every planted shape and nothing else: multiply-mapped, missing-stripe, incomplete-map, misrouted-name, unmigrated-name" {
    const gpa = testing.allocator;
    const seed: key.NameKey = .init(0x5EED);
    const good = [_]Marker{ .{ .name = "\x00sqz.stripe:00", .child = 100 }, .{ .name = "\x00sqz.stripe:01", .child = 101 }, .{ .name = STRIPED, .child = 100 } };
    const incomplete = [_]Marker{ .{ .name = "\x00sqz.stripe:00", .child = 200 }, .{ .name = STRIPED, .child = 200 } };
    const shared = [_]Marker{ .{ .name = "\x00sqz.stripe:00", .child = 101 }, .{ .name = "\x00sqz.stripe:01", .child = 300 }, .{ .name = STRIPED, .child = 101 } };
    // A name routed to stripe 1 planted in stripe 0 (misrouted), and a
    // user name left in D after `migrating` cleared (unmigrated).
    var n: u8 = 'a';
    var misrouted: [1]u8 = undefined;
    while (n <= 'z') : (n += 1) {
        misrouted[0] = n;
        if (stripeOf(key.dentryNameHash54(&misrouted, &seed), 2) == 1) break;
    }
    const s0_names = [_]DirRec{.{ .cookie = 1, .child = 900, .file_type = 8, .name = &misrouted }};
    const d_names = [_]DirRec{.{ .cookie = 2, .child = 901, .file_type = 8, .name = "left" }};
    const views = [_]DirView{
        .{ .dir = 9, .markers = &good, .names = &d_names },
        .{ .dir = 100, .markers = &.{}, .names = &s0_names },
        .{ .dir = 101, .markers = &.{}, .names = &.{} },
        .{ .dir = 10, .markers = &incomplete, .names = &.{} },
        .{ .dir = 11, .markers = &shared, .names = &.{} },
    };
    const Has = struct {
        fn has(ino: u64) bool {
            return ino != 300;
        }
    };
    var out: std.ArrayList(Finding) = .empty;
    defer out.deinit(gpa);
    stats.fsck_stripe_findings.resetLogBudgetForTest();
    @import("../../../core/log.zig").testing.expectErrorLogs(1);
    try verifyStripes(gpa, &views, &Has.has, &seed, &out);
    try @import("../../../core/log.zig").testing.expectAllFired();
    var seen: [5]u32 = @splat(0);
    for (out.items) |f| seen[@backingInt(f.shape)] += 1;
    try testing.expectEqual(@as(u32, 1), seen[@backingInt(C17Shape.multiply_mapped)]);
    try testing.expectEqual(@as(u32, 1), seen[@backingInt(C17Shape.missing_stripe)]);
    try testing.expectEqual(@as(u32, 1), seen[@backingInt(C17Shape.incomplete_map)]);
    try testing.expectEqual(@as(u32, 1), seen[@backingInt(C17Shape.misrouted_name)]);
    try testing.expectEqual(@as(u32, 1), seen[@backingInt(C17Shape.unmigrated_name)]);
    try testing.expectEqual(@as(usize, 5), out.items.len);
    // A clean set: nothing.
    var clean: std.ArrayList(Finding) = .empty;
    defer clean.deinit(gpa);
    const cv = [_]DirView{ .{ .dir = 9, .markers = &.{ good[0], good[1], .{ .name = MIGRATING, .child = 100 }, good[2] }, .names = &d_names }, .{ .dir = 100, .markers = &.{}, .names = &.{} }, .{ .dir = 101, .markers = &.{}, .names = &.{} } };
    try verifyStripes(gpa, &cv, &Has.has, &seed, &clean);
    try testing.expectEqual(@as(usize, 0), clean.items.len);
}
