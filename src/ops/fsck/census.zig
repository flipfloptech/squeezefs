//! The inode census (`ops_fsck.md` §2): the ONE store face every pass
//! reads, the kind pagers over the ino-major slot trees, `InoBitmap` (one
//! bit per ino ever allocated, dense per keyspace), the derived budgets,
//! the start watermarks + the slot jump, forest scoping (`SlotScope`: the
//! judged / foreign-live / unleased slots, the era floor with tree 0 as its
//! witness, the foreign ring windows, the `-o ro` rule), the shard residue,
//! the inode pass (live + corpse bitmaps, `odd_nlink`, the per-slot raw
//! maxima), the layout pass (the C8 derived half — corpses INCLUDED — the
//! C12/C7 mapping list, the C11 (b) nominations) and the dentry pass (the
//! referenced bitmap carrying C10's `multi` counts and C17's markers).
//!
//! Portable over `Store` (the kv_fs store shape: `range`/`lookup` by kind +
//! `begin`/`commit` for the repair engine); the Linux harnesses bind the
//! `MetaSet`, the portable tests the RAM store.

const std = @import("std");
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const derive = @import("../../core/derive.zig");
const key = @import("../../meta/key.zig");
const log = @import("../../core/log.zig");
const record = @import("../../meta/record.zig");
const tree = @import("../../meta/tree.zig");
const tx = @import("../../meta/tx.zig");
const inos = @import("../../meta/inos.zig");
const route_table = @import("../../meta/routing/route_table.zig");
const economy = @import("../../meta/economy.zig");
const layout = @import("../../data/layout.zig");
const kvmap = @import("../../data/kvmap.zig");
const block_refs = @import("../../data/block_refs.zig");
const stripe = @import("../../meta/sym/xv/stripe.zig");
const fsck = @import("../fsck.zig");
const finding = @import("finding.zig");

pub const Error = error{ OutOfMemory, Io, Corrupt };

// ---------------------------------------------------------------------------
// The store face.
// ---------------------------------------------------------------------------

/// A run's walk accounting — what every store read the passes made cost
/// (the report's `nodes_walked`, `entries_visited`, `entries_read`,
/// `store_calls`; `ops_fsck.md` §6.1's bound).
pub const WalkTally = struct {
    /// Range and lookup calls.
    calls: u64 = 0,
    /// Entries the store delivered (a lookup's hit counts one).
    visited: u64 = 0,
    /// Entries the trees copied out of their leaves to deliver them.
    read: u64 = 0,
    /// Tree nodes the descents fetched.
    nodes: u64 = 0,
    /// Trees the calls consulted (a merged range consults one per slot
    /// tree it fetched from).
    sources: u64 = 0,
};

/// What fsck needs of a store (the `KvBackend`/`MetaSet`/`MemStore` shape):
/// the inclusive range scan and the point lookup of one kind, a tx for the
/// repair engine. The merged `range` of a set returns EVERY kind inside the
/// bounds (the slot tree's leaves are mixed), so every pager filters.
pub const Store = struct {
    ctx: *anyopaque,
    vtable: *const VTable,
    /// A range of one kind returns EVERY kind inside its bounds (the set's
    /// merged slot-tree walk): the C1 raw walk is then ONE pass over the
    /// ino-major space; a kind-partitioned store (the RAM store) needs one
    /// pass per kind.
    merged_kinds: bool = false,
    /// The run's accounting every read through this face adds to (null =
    /// uncounted).
    tally: ?*WalkTally = null,

    pub const VTable = struct {
        range: *const fn (*anyopaque, Allocator, kind: u8, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue), cost: *tree.WalkCost) Error!void,
        /// C1's raw view: a merged store's law-free walk (a record a tree
        /// holds outside its slot's window included); `range` elsewhere.
        range_raw: *const fn (*anyopaque, Allocator, kind: u8, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue), cost: *tree.WalkCost) Error!void,
        lookup: *const fn (*anyopaque, Allocator, kind: u8, k: []const u8, cost: *tree.WalkCost) Error!?[]u8,
        begin: *const fn (*anyopaque, site: tx.Site) Error!*tx.KvTx,
        commit: *const fn (*anyopaque, *tx.KvTx) Error!void,
        abandon: *const fn (*anyopaque, *tx.KvTx) void,
        nameKey: *const fn (*anyopaque) *const key.NameKey,
    };

    pub fn from(comptime S: type, s: *S) Store {
        const Adapter = struct {
            // A store with trees counts its descents; one without counts the
            // entries it delivered as read.
            fn rangeFn(ctx: *anyopaque, gpa: Allocator, kind: u8, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue), cost: *tree.WalkCost) Error!void {
                const st: *S = @ptrCast(@alignCast(ctx));
                if (@hasDecl(S, "rangeCounted")) {
                    st.rangeCounted(gpa, kind, start, end, max, out, cost) catch |e| return mapErr(e);
                } else {
                    const before = out.items.len;
                    st.range(gpa, kind, start, end, max, out) catch |e| return mapErr(e);
                    cost.entries += out.items.len - before;
                    cost.sources += 1;
                }
            }
            fn rangeRawFn(ctx: *anyopaque, gpa: Allocator, kind: u8, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue), cost: *tree.WalkCost) Error!void {
                if (!@hasDecl(S, "rangeRawCounted")) return rangeFn(ctx, gpa, kind, start, end, max, out, cost);
                const st: *S = @ptrCast(@alignCast(ctx));
                st.rangeRawCounted(gpa, start, end, max, out, cost) catch |e| return mapErr(e);
            }
            fn lookupFn(ctx: *anyopaque, gpa: Allocator, kind: u8, k: []const u8, cost: *tree.WalkCost) Error!?[]u8 {
                const st: *S = @ptrCast(@alignCast(ctx));
                if (@hasDecl(S, "lookupCounted")) return st.lookupCounted(gpa, kind, k, cost) catch |e| return mapErr(e);
                const v = st.lookup(gpa, kind, k) catch |e| return mapErr(e);
                if (v != null) cost.entries += 1;
                cost.sources += 1;
                return v;
            }
            fn beginFn(ctx: *anyopaque, site: tx.Site) Error!*tx.KvTx {
                const st: *S = @ptrCast(@alignCast(ctx));
                return st.begin(site) catch |e| return mapErr(e);
            }
            fn commitFn(ctx: *anyopaque, t: *tx.KvTx) Error!void {
                const st: *S = @ptrCast(@alignCast(ctx));
                st.commit(t) catch |e| return mapErr(e);
            }
            fn abandonFn(ctx: *anyopaque, t: *tx.KvTx) void {
                const st: *S = @ptrCast(@alignCast(ctx));
                st.abandon(t);
            }
            fn nameKeyFn(ctx: *anyopaque) *const key.NameKey {
                const st: *S = @ptrCast(@alignCast(ctx));
                return st.nameKey();
            }
            const vtable: VTable = .{ .range = &rangeFn, .range_raw = &rangeRawFn, .lookup = &lookupFn, .begin = &beginFn, .commit = &commitFn, .abandon = &abandonFn, .nameKey = &nameKeyFn };
        };
        return .{ .ctx = s, .vtable = &Adapter.vtable };
    }

    fn mapErr(e: anyerror) Error {
        return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Corrupt => error.Corrupt,
            else => error.Io,
        };
    }

    pub fn range(self: Store, gpa: Allocator, kind: u8, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue)) Error!void {
        return self.rangeVia(self.vtable.range, gpa, kind, start, end, max, out);
    }

    /// C1's raw view of `[start, end]` (`VTable.range_raw`).
    pub fn rangeRaw(self: Store, gpa: Allocator, kind: u8, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue)) Error!void {
        return self.rangeVia(self.vtable.range_raw, gpa, kind, start, end, max, out);
    }

    fn rangeVia(self: Store, f: @FieldType(VTable, "range"), gpa: Allocator, kind: u8, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue)) Error!void {
        var cost: tree.WalkCost = .{};
        const before = out.items.len;
        defer if (self.tally) |t| {
            t.calls += 1;
            t.visited += out.items.len - before;
            t.read += cost.entries;
            t.nodes += cost.nodes;
            t.sources += cost.sources;
        };
        return f(self.ctx, gpa, kind, start, end, max, out, &cost);
    }

    pub fn lookup(self: Store, gpa: Allocator, kind: u8, k: []const u8) Error!?[]u8 {
        var cost: tree.WalkCost = .{};
        const v = try self.vtable.lookup(self.ctx, gpa, kind, k, &cost);
        if (self.tally) |t| {
            t.calls += 1;
            if (v != null) t.visited += 1;
            t.read += cost.entries;
            t.nodes += cost.nodes;
            t.sources += cost.sources;
        }
        return v;
    }

    pub fn begin(self: Store, site: tx.Site) Error!*tx.KvTx {
        return self.vtable.begin(self.ctx, site);
    }

    pub fn commit(self: Store, t: *tx.KvTx) Error!void {
        return self.vtable.commit(self.ctx, t);
    }

    pub fn abandon(self: Store, t: *tx.KvTx) void {
        self.vtable.abandon(self.ctx, t);
    }

    pub fn nameKey(self: Store) *const key.NameKey {
        return self.vtable.nameKey(self.ctx);
    }

    /// The refs oracle's face over this store (kind 6).
    pub fn refStore(self: *const Store) block_refs.RefStore {
        return .{ .ctx = @constCast(self), .vtable = &ref_vtable };
    }

    /// The block-map oracle's face over this store (kind 7).
    pub fn mapStore(self: *const Store) kvmap.Store {
        return .{ .ctx = @constCast(self), .vtable = &map_vtable };
    }

    fn refRange(ctx: *anyopaque, gpa: Allocator, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue)) block_refs.Error!void {
        const self: *Store = @ptrCast(@alignCast(ctx));
        return self.range(gpa, key.TREE_BLOCK_REFS, start, end, max, out);
    }
    fn refLookup(ctx: *anyopaque, gpa: Allocator, k: []const u8) block_refs.Error!?[]u8 {
        const self: *Store = @ptrCast(@alignCast(ctx));
        return self.lookup(gpa, key.TREE_BLOCK_REFS, k);
    }
    const ref_vtable: block_refs.RefStore.VTable = .{ .range = &refRange, .lookup = &refLookup };

    fn mapRange(ctx: *anyopaque, gpa: Allocator, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue)) kvmap.Error!void {
        const self: *Store = @ptrCast(@alignCast(ctx));
        return self.range(gpa, key.TREE_BLOCK_MAP, start, end, max, out);
    }
    fn mapLookup(ctx: *anyopaque, gpa: Allocator, k: []const u8) kvmap.Error!?[]u8 {
        const self: *Store = @ptrCast(@alignCast(ctx));
        return self.lookup(gpa, key.TREE_BLOCK_MAP, k);
    }
    const map_vtable: kvmap.Store.VTable = .{ .range = &mapRange, .lookup = &mapLookup };
};

// ---------------------------------------------------------------------------
// Ino arithmetic: a KEY ino and a GLOBAL ino decompose to the SAME
// `(slot, raw local)` (`route_table.effectiveIno` is the law), so a slot
// migration mid-scan cannot shift a bit.
// ---------------------------------------------------------------------------

pub const SlotLocal = struct { slot: u16, raw: u64 };

/// A record's KEY ino → `(slot, raw)`: a guest key splits; a native local
/// is slot 0's.
pub fn ofKeyIno(key_ino: u64) SlotLocal {
    if (key.splitGuestLocal(key_ino)) |g| return .{ .slot = g.slot, .raw = g.local };
    return .{ .slot = 0, .raw = key_ino };
}

/// A dentry's GLOBAL child ino → `(slot, raw)` (null = outside the
/// routable space — a corrupt value is never an index).
pub fn ofGlobalIno(global: u64) ?SlotLocal {
    if (!route_table.isRoutable(global)) return null;
    const r = inos.routeIno(global, inos.ROUTING_WIDTH);
    return .{ .slot = @intCast(r.slot), .raw = r.local };
}

pub fn globalOfKeyIno(key_ino: u64) u64 {
    return route_table.globalOfEffective(key_ino);
}

/// The global ino of a key ino that an INODE may own; null for a slot's
/// reserved locals below the mint base (a cross-owner intent's home is
/// local 0 of its slot — "guest cursors mint from 2, so no inode ever owns
/// it"; slot 0's local 0 likewise). The root pin (native local 1) is the
/// root's.
pub fn globalOfOwnedKeyIno(key_ino: u64) ?u64 {
    const sl = ofKeyIno(key_ino);
    if (sl.slot == 0 and sl.raw == inos.ROOT_INO) return inos.ROOT_INO;
    if (sl.raw < inos.LOCAL_INO_BASE) return null;
    return route_table.globalOfEffective(key_ino);
}

pub fn keyInoOf(sl: SlotLocal) u64 {
    if (sl.slot == 0) return sl.raw;
    return key.guestLocalIno(sl.slot, sl.raw);
}

// ---------------------------------------------------------------------------
// §2.6 Derived budgets (pure functions of the memory budget).
// ---------------------------------------------------------------------------

/// The scoped names the debug log spells out per pass (the rest are
/// the counter's).
const DEBUG_SCOPED_NAMES: u64 = 12;

/// `max(mem_budget / 64, 16 MiB)`: a scan holds two sets (≈ 3 % of the
/// budget transiently); the floor is the design cap's own requirement —
/// 100 M inodes / 8 = 12.5 MB of bits, 16 MiB with headroom for the
/// sparse tail (FSCK:L1430-1443).
pub fn inoSetByteBudget(mem_budget: u64) u64 {
    return @max(mem_budget / 64, 16 * derive.MiB);
}

/// The accounting factor converting the byte budget into C10 count
/// entries: a `(parent, name)` pair or an `(ino, nlink)` pair with hash
/// overhead — a bound, not a promise (FSCK:L1445-1465).
pub const C10_COUNT_ENTRY_BYTES: u64 = 64;

pub fn c10CountEntryBudget(mem_budget: u64) u64 {
    return inoSetByteBudget(mem_budget) / C10_COUNT_ENTRY_BYTES;
}

/// The candidate ceiling per class per run: `ino_set_byte_budget / 8`
/// entries; the rest is DEFERRED and said loudly (FSCK:L4076-4089).
pub fn candidateCeiling(mem_budget: u64) u64 {
    return inoSetByteBudget(mem_budget) / 8;
}

/// The C1 fan-out wave: `meta_lanes × 4` — hundreds of slot trees on a
/// two-lane pool wedged the loader's bucket lock (FSCK:L1820-1831).
pub fn c1Wave(meta_lanes: u32) u32 {
    return @max(meta_lanes, 1) * 4;
}

// ---------------------------------------------------------------------------
// §6.1 The walk's bound: what a run may read for what it visits.
// ---------------------------------------------------------------------------

/// Entries a run's store may copy out of its trees to deliver the tally's
/// `visited`: the merged walk's per-call bound (`meta/merged_walk.
/// readBound`: `3e + p` for a call that filled its `max` consulting `p`
/// trees, exactly `e` for one that reached its window's end) summed over
/// the calls — `3 × visited + sources`. Linear in what the run visits: a
/// walk that read `max` from every tree per page (T × the page) ran
/// quadratic up to `T × SCAN_PAGE` entries.
pub fn walkReadBound(t: WalkTally) u64 {
    return 3 * t.visited + t.sources;
}

/// Tree nodes a run may fetch: `height` per entry it visits and per call
/// (a call that delivers nothing still descends once) — node reads per
/// entry within the tree height.
pub fn walkNodeBound(t: WalkTally, height: u64) u64 {
    return height * (t.visited + t.calls);
}

// ---------------------------------------------------------------------------
// §2.2 InoBitmap.
// ---------------------------------------------------------------------------

pub const Mark = enum { newly_set, already_set, refused };

/// One bit per ino ever allocated, dense per keyspace: raw locals are
/// monotonic from 2 and never reused. `raw < 2` never indexes (raw 1 =
/// the root pin / a control record — the root exemption falls out of the
/// encoding). The CEILING is an allocation authority (the inode pass's
/// per-set raw maximum, the watermarks): a dentry's `child_ino` is never
/// one — a corrupt value naming `u64::MAX` would otherwise size a vector
/// from it. Over the byte budget ⇒ `truncated` and the set's class records
/// NO verdict.
pub const InoBitmap = struct {
    gpa: Allocator,
    slots: std.AutoHashMapUnmanaged(u16, []u64) = .empty,
    ceiling: u64,
    budget: u64,
    bytes: u64 = 0,
    population: u64 = 0,
    truncated: bool = false,

    pub fn init(gpa: Allocator, ceiling: u64, budget: u64) InoBitmap {
        return .{ .gpa = gpa, .ceiling = ceiling, .budget = budget };
    }

    pub fn deinit(self: *InoBitmap) void {
        var it = self.slots.valueIterator();
        while (it.next()) |w| self.gpa.free(w.*);
        self.slots.deinit(self.gpa);
        self.* = undefined;
    }

    fn wordsFor(self: *const InoBitmap) usize {
        return @intCast((self.ceiling + 64) / 64);
    }

    fn words(self: *InoBitmap, slot: u16) Allocator.Error!?[]u64 {
        if (self.slots.get(slot)) |w| return w;
        const n = self.wordsFor();
        const cost = @as(u64, n) * @sizeOf(u64);
        if (self.bytes + cost > self.budget) {
            self.truncated = true;
            return null;
        }
        const w = try self.gpa.alloc(u64, n);
        @memset(w, 0);
        try self.slots.put(self.gpa, slot, w);
        self.bytes += cost;
        return w;
    }

    /// Set the bit; `newly_set` is what makes C10's counting free for the
    /// single-name majority.
    pub fn mark(self: *InoBitmap, sl: SlotLocal) Allocator.Error!Mark {
        if (sl.raw < inos.LOCAL_INO_BASE or sl.raw > self.ceiling) return .refused;
        const w = (try self.words(sl.slot)) orelse return .refused;
        const bit = @as(u64, 1) << @intCast(sl.raw % 64);
        const i: usize = @intCast(sl.raw / 64);
        if (w[i] & bit != 0) return .already_set;
        w[i] |= bit;
        self.population += 1;
        return .newly_set;
    }

    pub fn isSet(self: *const InoBitmap, sl: SlotLocal) bool {
        if (sl.raw < inos.LOCAL_INO_BASE or sl.raw > self.ceiling) return false;
        const w = self.slots.get(sl.slot) orelse return false;
        return w[@intCast(sl.raw / 64)] & (@as(u64, 1) << @intCast(sl.raw % 64)) != 0;
    }

    /// Word-parallel `self AND NOT other`: a healthy volume's whole
    /// difference is `population / 64` words with no per-inode work.
    /// `visit` returns false to stop (the candidate ceiling).
    pub fn eachAbsentFrom(self: *const InoBitmap, other: *const InoBitmap, ctx: anytype, comptime visit: fn (@TypeOf(ctx), SlotLocal) bool) void {
        var it = self.slots.iterator();
        while (it.next()) |e| {
            const slot = e.key_ptr.*;
            const mine = e.value_ptr.*;
            const theirs = other.slots.get(slot);
            for (mine, 0..) |word, wi| {
                var bits = word & ~(if (theirs) |t| (if (wi < t.len) t[wi] else 0) else 0);
                while (bits != 0) : (bits &= bits - 1) {
                    if (!visit(ctx, .{ .slot = slot, .raw = @as(u64, wi) * 64 + @ctz(bits) })) return;
                }
            }
        }
    }
};

// ---------------------------------------------------------------------------
// §2.4–§2.5 Forest scoping: the start watermarks, the judged slots, the
// era floor with tree 0 as its witness, the foreign windows, the `-o ro`
// rule. ONE snapshot at pass start, shared by every walker (0002 Q13).
// ---------------------------------------------------------------------------

pub const Era = enum { prior, current, unknown };

pub const Posture = enum {
    /// The volume's manager (the offline D0-guarded open is this too).
    manager,
    /// A joined appender: every slot NOT leased here is the manager's
    /// projection (its rotor roots ride its page) — unjudged.
    member,
    /// A `-o ro` reader / a probe: leases nothing, has nothing in flight.
    probe,
};

pub const SlotScope = struct {
    gpa: Allocator,
    posture: Posture = .manager,
    /// Per slot the START watermark (first raw local NOT minted at the
    /// walk's start) of a keyspace this mount holds the LIVE cursor for;
    /// absent = walked whole (a joiner's slot bounded at grant-time words
    /// read live blocks as leaked — §4.4ba).
    watermarks: std.AutoHashMapUnmanaged(u16, u64) = .empty,
    /// Slots this mount leases (native included).
    leased: std.AutoHashMapUnmanaged(u16, void) = .empty,
    /// Slots a lessee the membership owner lists LIVE holds: their trees
    /// CHANGE under a census here — never judged, counted.
    foreign_live: std.AutoHashMapUnmanaged(u16, void) = .empty,
    /// Slots NOBODY leases (tree 0 `Unleased`): no minter, so every record
    /// in them is prior-era whatever the floor says.
    unleased: std.AutoHashMapUnmanaged(u16, void) = .empty,
    /// The era floor per slot: the watermark recovered at OPEN (exact
    /// provenance, not a heuristic); absent = fail-closed `unknown`.
    era_floor: std.AutoHashMapUnmanaged(u16, u64) = .empty,
    /// GLOBAL inos a foreign ring window names (incl. a DELETE's child
    /// resolved through the tree): scoped out of C9/C10.
    window_inos: std.AutoHashMapUnmanaged(u64, void) = .empty,
    /// A ring that cannot be read ⇒ the WHOLE inode plane takes no verdict.
    window_unknown: bool = false,
    /// `-o ro` / probe: a `Live` appender page at open is an UNCOVERED
    /// window (acked records replayed by a writer's own-residue open and
    /// nothing else) ⇒ the inode plane records no verdict (Issue 21b).
    reader_uncovered: bool = false,

    pub fn init(gpa: Allocator) SlotScope {
        return .{ .gpa = gpa };
    }

    pub fn deinit(self: *SlotScope) void {
        self.watermarks.deinit(self.gpa);
        self.leased.deinit(self.gpa);
        self.foreign_live.deinit(self.gpa);
        self.unleased.deinit(self.gpa);
        self.era_floor.deinit(self.gpa);
        self.window_inos.deinit(self.gpa);
        self.* = undefined;
    }

    /// The ONE predicate the C1 raw walk, the dentry pass and the inode
    /// plane read: on the MANAGER the foreign-live slots; on a MEMBER
    /// every slot not leased here; a PROBE judges everything but a live
    /// foreign lessee's tree.
    pub fn unjudged(self: *const SlotScope, slot: u16) bool {
        return switch (self.posture) {
            .manager, .probe => self.foreign_live.contains(slot),
            .member => !self.leased.contains(slot),
        };
    }

    /// `minted_in_prior_era` + the tree-0 witness, three-valued and
    /// fail-closed (`BE:L12412-12454`).
    pub fn era(self: *const SlotScope, sl: SlotLocal) Era {
        if (self.foreign_live.contains(sl.slot)) return .unknown;
        if (self.unleased.contains(sl.slot)) return .prior;
        if (self.posture == .probe) return .prior;
        const floor = self.era_floor.get(sl.slot) orelse return .unknown;
        return if (sl.raw < floor) .prior else .current;
    }

    /// Does the inode plane take a verdict at all this run?
    pub fn inodePlaneJudges(self: *const SlotScope) bool {
        return !self.window_unknown and !self.reader_uncovered;
    }

    /// The coverage gauge: `leased ∪ unleased`.
    pub fn slotsCovered(self: *const SlotScope) u64 {
        var n: u64 = 0;
        var it = self.unleased.keyIterator();
        while (it.next()) |s| if (!self.leased.contains(s.*)) {
            n += 1;
        };
        return n + self.leased.count();
    }
};

// ---------------------------------------------------------------------------
// The kind pagers over the ino-major space.
// ---------------------------------------------------------------------------

/// The first key of `kind` for `ino` (a resume cursor), the last of the space.
fn firstKeyOf(kind: u8, ino: u64) key.KeyBuf {
    return switch (kind) {
        key.TREE_INODES => key.KeyBuf.fromSlice(&key.inodeKey(ino)),
        key.TREE_DENTRIES => key.KeyBuf.fromSlice(&key.dentryKey(ino, 0, 0)),
        key.TREE_XATTRS => key.KeyBuf.fromSlice(&key.xattrKey(ino, 0, 0)),
        key.TREE_BLOCK_MAP => key.KeyBuf.fromSlice(&key.indexRangeFrom(ino, 0).lo),
        else => unreachable,
    };
}

fn lastKeyOf(kind: u8) key.KeyBuf {
    const max = std.math.maxInt(u64);
    return switch (kind) {
        key.TREE_INODES => key.KeyBuf.fromSlice(&key.inodeKey(max)),
        key.TREE_DENTRIES => key.KeyBuf.fromSlice(&key.dentryKey(max, key.HASH54_MAX, std.math.maxInt(u8))),
        key.TREE_XATTRS => key.KeyBuf.fromSlice(&key.xattrKey(max, key.HASH56_MAX, std.math.maxInt(u8))),
        key.TREE_BLOCK_MAP => key.KeyBuf.fromSlice(&key.indexRangeFrom(max, 0).hi),
        else => unreachable,
    };
}

/// Pages one ino-major kind from `from_ino` to the end of its space,
/// `SCAN_PAGE` at a time; a merged page holds OTHER kinds too (the leaves
/// are mixed), so the pager filters by kind byte + length and resumes at
/// the NEXT key of its kind after the page's last key, whatever its kind
/// (0002 row 176: never `key ‖ 0x00`). `jumpTo(ino)` re-aims the cursor
/// (the start-watermark slot jump).
pub const KindPager = struct {
    store: Store,
    gpa: Allocator,
    kind: u8,
    want_len: usize,
    cursor: key.KeyBuf,
    end: key.KeyBuf,
    page: std.ArrayList(tree.KeyValue) = .empty,
    pos: usize = 0,
    done: bool = false,
    pages: u64 = 0,
    /// The test seam: park after the FIRST page until released (the
    /// census-hold contract: a creator mints past the start watermarks).
    hold: ?*const std.atomic.Value(bool) = null,
    /// The C1 raw walk: the store's law-free view, yielding EVERY record
    /// the bounds return, whatever its shape (a key the codec refuses is
    /// exactly what it must see).
    raw: bool = false,

    pub fn init(store: Store, gpa: Allocator, kind: u8, from_ino: u64) KindPager {
        return .{ .store = store, .gpa = gpa, .kind = kind, .want_len = key.slotTreeKeyLen(kind).?, .cursor = firstKeyOf(kind, from_ino), .end = lastKeyOf(kind) };
    }

    pub fn deinit(self: *KindPager) void {
        tree.deinitKeyValues(self.gpa, &self.page);
        self.* = undefined;
    }

    /// Drop the page's copies and keep the list usable.
    fn clearPage(self: *KindPager) void {
        for (self.page.items) |*kv| kv.deinit(self.gpa);
        self.page.clearRetainingCapacity();
        self.pos = 0;
    }

    pub fn jumpTo(self: *KindPager, ino: u64) void {
        self.clearPage();
        self.done = false;
        self.cursor = firstKeyOf(self.kind, ino);
    }

    fn fetch(self: *KindPager) Error!void {
        self.clearPage();
        if (self.done) return;
        if (self.pages == 1) if (self.hold) |h| while (h.load(.acquire)) std.Thread.yield() catch std.atomic.spinLoopHint();
        if (self.raw) {
            try self.store.rangeRaw(self.gpa, self.kind, self.cursor.slice(), self.end.slice(), fsck.SCAN_PAGE, &self.page);
        } else try self.store.range(self.gpa, self.kind, self.cursor.slice(), self.end.slice(), fsck.SCAN_PAGE, &self.page);
        self.pages += 1;
        if (self.page.items.len == 0) {
            self.done = true;
            return;
        }
        if (self.page.items.len < fsck.SCAN_PAGE) {
            self.done = true;
            return;
        }
        const last = self.page.items[self.page.items.len - 1].key;
        if (last.len < 8) {
            // A key too short to carry an ino (a raw walk's truncated
            // record): the next page starts just past it.
            self.cursor = key.keySuccessor(last);
            return;
        }
        const last_kind: u8 = if (last.len > key.INO_MAJOR_KIND_OFF) last[key.INO_MAJOR_KIND_OFF] else 0;
        const last_ino = std.mem.readInt(u64, last[0..8], .big);
        if (last.len == self.want_len and last_kind == self.kind) {
            self.cursor = key.sameKindSuccessor(last) orelse {
                self.done = true;
                return;
            };
        } else if (last_kind < self.kind) {
            self.cursor = firstKeyOf(self.kind, last_ino);
        } else {
            if (last_ino == std.math.maxInt(u64)) {
                self.done = true;
                return;
            }
            self.cursor = firstKeyOf(self.kind, last_ino + 1);
        }
    }

    /// The next record of this kind (borrowed until the next call).
    pub fn next(self: *KindPager) Error!?tree.KeyValue {
        while (true) {
            if (self.pos >= self.page.items.len) {
                if (self.done and self.pages != 0) return null;
                try self.fetch();
                if (self.page.items.len == 0) return null;
            }
            const kv = self.page.items[self.pos];
            self.pos += 1;
            if (!self.raw and (kv.key.len != self.want_len or kv.key[key.INO_MAJOR_KIND_OFF] != self.kind)) continue;
            return kv;
        }
    }
};

// ---------------------------------------------------------------------------
// §2.1 The census.
// ---------------------------------------------------------------------------

/// One block reference a layout names (C12's window arithmetic, C7's read
/// list, C2's lost arm): the binding + the owner.
pub const MappingRef = struct {
    ino: u64,
    block_index: u32,
    mapping: layout.Mapping,
};

pub const Options = struct {
    scope: *const SlotScope,
    shard: ?finding.Shard = null,
    /// The memory budget the derived budgets read.
    mem_budget: u64,
    geo: layout.Geometry,
    /// Volume tags the set knows (a binding naming another is C2-lost by
    /// construction).
    known_vol_tags: []const u64,
    /// The census-hold test seam.
    hold: ?*const std.atomic.Value(bool) = null,
    /// A fleet member's residue: the inos of the slots THIS mount leases
    /// (a live lessee's slots are readable only on the lessee — its
    /// coordinator cannot scrub them), so the block census takes rows of
    /// its own custody alone and the merge at the coordinator lands every
    /// block exactly once.
    custody_only: bool = false,
};

pub const Census = struct {
    gpa: Allocator,
    /// `nlink > 0`, judged slots, this shard's residue.
    live: InoBitmap,
    /// `nlink == 0` (unlinked, FORGET not arrived): LEGAL live state that
    /// still owns its blocks — out of every inode-plane set, IN the block
    /// census (the corpse-census correction: 152 FPs on a healthy fleet).
    corpses: InoBitmap,
    /// Non-directory live inodes with `nlink != 1` (global ino → nlink).
    odd_nlink: std.AutoHashMapUnmanaged(u64, u32) = .empty,
    odd_nlink_complete: bool = true,
    /// Directories (global inos) — never a C10 NAME-count finding.
    dirs: InoBitmap,
    /// Live directories' `nlink` (global ino → nlink): C10's directory
    /// arm, judged against `2 + subdirectories` (`DentryPass.subdirs`).
    dir_nlink: std.AutoHashMapUnmanaged(u64, u32) = .empty,
    dir_nlink_complete: bool = true,
    /// Per slot the maximum raw local an inode record carries (the
    /// referenced set's ceiling).
    max_raw: std.AutoHashMapUnmanaged(u16, u64) = .empty,
    /// The C8 derived half (corpses included), per owner.
    owners: std.ArrayList(block_refs.OwnerBindings) = .empty,
    /// Every mapping with its owner (C12, C7, C2-lost's re-verify).
    mappings: std.ArrayList(MappingRef) = .empty,
    /// Sweep-less kvmap heads with `size > 0` (C11 (b) nominees).
    kvmap_nominees: std.ArrayList(u64) = .empty,
    /// Bindings naming a volume the set does not know (C2-lost by construction).
    unresolvable: std.ArrayList(MappingRef) = .empty,
    inodes_scanned: u64 = 0,
    corpses_seen: u64 = 0,
    /// An unreadable page / an undecodable layout: the census is incomplete
    /// (the classes that need completeness record no verdict).
    complete: bool = true,

    pub fn deinit(self: *Census) void {
        const gpa = self.gpa;
        self.live.deinit();
        self.corpses.deinit();
        self.dirs.deinit();
        self.odd_nlink.deinit(gpa);
        self.dir_nlink.deinit(gpa);
        self.max_raw.deinit(gpa);
        for (self.owners.items) |o| gpa.free(o.bindings);
        self.owners.deinit(gpa);
        self.mappings.deinit(gpa);
        self.kvmap_nominees.deinit(gpa);
        self.unresolvable.deinit(gpa);
        self.* = undefined;
    }

    /// The set-wide raw ceiling (the allocation authority the referenced
    /// bitmap is sized by).
    pub fn rawCeiling(self: *const Census) u64 {
        var c: u64 = inos.LOCAL_INO_BASE;
        var it = self.max_raw.valueIterator();
        while (it.next()) |v| c = @max(c, v.*);
        return c;
    }

    pub fn hasInode(self: *const Census, global: u64) bool {
        const sl = ofGlobalIno(global) orelse return false;
        return self.live.isSet(sl) or self.corpses.isSet(sl);
    }

    pub fn isLive(self: *const Census, global: u64) bool {
        const sl = ofGlobalIno(global) orelse return false;
        return self.live.isSet(sl);
    }
};

/// The inode pass + the layout pass: bounded at the start watermarks (a
/// record minted past its slot's ceiling is skipped and the cursor JUMPS to
/// the next slot — a walk paged at 512 never caught a creator that outpaced
/// it); a slot with no watermark is walked whole; a slot the scope does not
/// judge is skipped and counted; under `--shards` only the residue's
/// inodes feed the sets (the layout half filters by the OWNER's residue).
pub fn run(gpa: Allocator, store: Store, opts: Options) Error!Census {
    const budget = inoSetByteBudget(opts.mem_budget);
    // Pass A: the inode kind — the raw maxima first, so the bitmaps' ceiling
    // is an allocation authority (the records themselves) + the watermarks.
    var inode_rows: std.ArrayList(InodeRow) = .empty;
    defer inode_rows.deinit(gpa);
    var max_raw: std.AutoHashMapUnmanaged(u16, u64) = .empty;
    errdefer max_raw.deinit(gpa);
    var ceiling: u64 = inos.LOCAL_INO_BASE;
    {
        var wit = opts.scope.watermarks.valueIterator();
        while (wit.next()) |w| ceiling = @max(ceiling, w.* -| 1);
    }
    var complete = true;
    var scanned: u64 = 0;
    {
        var pager = KindPager.init(store, gpa, key.TREE_INODES, 0);
        pager.hold = opts.hold;
        defer pager.deinit();
        while (true) {
            const kv = (pager.next() catch |e| switch (e) {
                error.OutOfMemory => return error.OutOfMemory,
                else => {
                    complete = false;
                    break;
                },
            }) orelse break;
            const key_ino = std.mem.readInt(u64, kv.key[0..8], .big);
            const sl = ofKeyIno(key_ino);
            if (opts.scope.watermarks.get(sl.slot)) |w| if (sl.raw >= w) {
                // Past the start watermark: the next census's record; the
                // whole rest of this slot is too.
                pager.jumpTo(nextSlotKeyIno(sl.slot) orelse break);
                continue;
            };
            const v = record.InodeValue.decode(kv.value) catch continue; // C1's object
            const e = try max_raw.getOrPutValue(gpa, sl.slot, 0);
            e.value_ptr.* = @max(e.value_ptr.*, sl.raw);
            ceiling = @max(ceiling, sl.raw);
            const global = globalOfKeyIno(key_ino);
            if (opts.shard) |s| if (!s.owns(global)) continue;
            if (opts.custody_only and !opts.scope.leased.contains(sl.slot)) continue;
            scanned += 1;
            // The block census reads EVERY slot (a foreign lessee's blocks
            // are real references; the ladder shields its churn); the
            // inode-plane classes scope the slot out themselves.
            try inode_rows.append(gpa, .{ .sl = sl, .global = global, .nlink = v.nlink, .dir = economy.isDirMode(v.mode) });
        }
    }
    var c: Census = .{
        .gpa = gpa,
        .live = InoBitmap.init(gpa, ceiling, budget),
        .corpses = InoBitmap.init(gpa, ceiling, budget),
        .dirs = InoBitmap.init(gpa, ceiling, budget),
        .max_raw = max_raw,
        .inodes_scanned = scanned,
        .complete = complete,
    };
    errdefer c.deinit();
    const entry_budget = c10CountEntryBudget(opts.mem_budget);
    for (inode_rows.items) |r| {
        if (r.nlink == 0) {
            _ = try c.corpses.mark(r.sl);
            c.corpses_seen += 1;
            continue;
        }
        _ = try c.live.mark(r.sl);
        if (r.dir) {
            _ = try c.dirs.mark(r.sl);
            if (c.dir_nlink.count() < entry_budget) {
                try c.dir_nlink.put(gpa, r.global, r.nlink);
            } else c.dir_nlink_complete = false;
        } else if (r.nlink != 1) {
            if (c.odd_nlink.count() < entry_budget) {
                try c.odd_nlink.put(gpa, r.global, r.nlink);
            } else c.odd_nlink_complete = false;
        }
    }
    if (c.live.truncated or c.corpses.truncated) c.complete = false;
    // Pass B: the layout kind (every live inode's layout, corpses included).
    try layoutPass(gpa, store, opts, &c);
    return c;
}

const InodeRow = struct { sl: SlotLocal, global: u64, nlink: u32, dir: bool };

/// The first key ino of the slot after `slot` (null past the last).
fn nextSlotKeyIno(slot: u16) ?u64 {
    if (slot == std.math.maxInt(u16)) return null;
    return key.guestLocalIno(slot + 1, 0);
}

fn layoutPass(gpa: Allocator, store: Store, opts: Options, c: *Census) Error!void {
    var pager = KindPager.init(store, gpa, key.TREE_XATTRS, 0);
    defer pager.deinit();
    const map_store = store.mapStore();
    while (true) {
        const kv = (pager.next() catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            else => {
                c.complete = false;
                return;
            },
        }) orelse break;
        const xv = record.XattrValue.decode(kv.value) catch continue;
        if (!std.mem.eql(u8, xv.name, layout.LAYOUT_XATTR_NAME)) continue;
        const key_ino = std.mem.readInt(u64, kv.key[0..8], .big);
        const sl = ofKeyIno(key_ino);
        if (opts.scope.watermarks.get(sl.slot)) |w| if (sl.raw >= w) continue;
        if (opts.shard) |s| if (!s.owns(globalOfKeyIno(key_ino))) continue;
        if (opts.custody_only and !opts.scope.leased.contains(sl.slot)) continue;
        var binds: std.ArrayList(block_refs.Binding) = .empty;
        errdefer binds.deinit(gpa);
        const l = layout.Layout.decode(xv.value, opts.geo) catch |e| {
            if (e != error.MappingRefused) {
                c.complete = false;
                continue;
            }
            // A window the law refuses (C12's Overrun object): the census
            // still resolves the BASE and counts the reference — the
            // block is healthy for C2/C3/C8 and unreadable for every
            // reader; without this the Overrun arm would read as C2 lost.
            try rawBaseBindings(gpa, xv.value, opts.geo.chunk, &binds);
            try c.owners.append(gpa, .{ .ino = key_ino, .bindings = try binds.toOwnedSlice(gpa) });
            continue;
        };
        if (l.isKvmap()) {
            kvmap.deriveBindings(map_store, gpa, key_ino, opts.geo.chunk, &binds) catch |e| switch (e) {
                error.OutOfMemory => return error.OutOfMemory,
                else => {
                    c.complete = false;
                    continue;
                },
            };
            for (binds.items) |b| try c.mappings.append(gpa, .{ .ino = key_ino, .block_index = b.block_index, .mapping = layout.Mapping.wholeBlock(b.vol_tag, b.block_idx * opts.geo.chunk, 0) });
            // C11 (b): a sweep-less head claiming data with no records;
            // corpses never nominate (a truncate/unlink sweep legitimately
            // leaves the shape).
            if (l.kvmapHead().?.sweep == null and l.size > 0 and binds.items.len == 0 and c.live.isSet(sl)) try c.kvmap_nominees.append(gpa, key_ino);
        } else {
            var it = l.named(opts.geo.chunk);
            while (it.next()) |e| {
                try binds.append(gpa, .{ .block_index = e.block, .vol_tag = e.mapping.vol_tag, .block_idx = e.mapping.blockIdx(opts.geo.chunk) });
                try c.mappings.append(gpa, .{ .ino = key_ino, .block_index = e.block, .mapping = e.mapping });
            }
        }
        for (binds.items, 0..) |b, i| {
            if (!known(opts.known_vol_tags, b.vol_tag)) try c.unresolvable.append(gpa, c.mappings.items[c.mappings.items.len - binds.items.len + i]);
        }
        try c.owners.append(gpa, .{ .ino = key_ino, .bindings = try binds.toOwnedSlice(gpa) });
    }
}

/// The base blocks of a layout whose windows the codec refuses, read raw.
fn rawBaseBindings(gpa: Allocator, bytes: []const u8, chunk: u64, out: *std.ArrayList(block_refs.Binding)) Error!void {
    if (bytes.len < layout.LAYOUT_HEADER_LEN) return;
    const h = std.mem.bytesToValue(layout.LayoutHeaderWire, bytes[0..layout.LAYOUT_HEADER_LEN]);
    const flags = std.mem.readInt(u16, &h.flags_le, .little);
    const body = bytes[layout.LAYOUT_HEADER_LEN..];
    switch (h.layout_kind) {
        1, 2 => {
            const with_copy = h.layout_kind == 2 or flags & layout.F_HAS_DURABLE_COPY != 0;
            if (!with_copy or body.len < 16 + layout.MAPPING_WIRE_LEN) return;
            const w = std.mem.bytesToValue(layout.MappingWire, body[16..][0..layout.MAPPING_WIRE_LEN]);
            try out.append(gpa, .{ .block_index = 0, .vol_tag = std.mem.readInt(u64, &w.vol_tag_le, .little), .block_idx = std.mem.readInt(u64, &w.offset_le, .little) / chunk });
        },
        3 => {
            if (flags & layout.F_MAP_INLINE == 0 or body.len < 4) return;
            const n: usize = std.mem.readInt(u32, body[0..4], .little);
            if (body.len - 4 != n * layout.MAP_ENTRY_WIRE_LEN) return;
            for (std.mem.bytesAsSlice(layout.MapEntryWire, body[4..])) |e| {
                try out.append(gpa, .{ .block_index = e.block(), .vol_tag = std.mem.readInt(u64, &e.mapping.vol_tag_le, .little), .block_idx = std.mem.readInt(u64, &e.mapping.offset_le, .little) / chunk });
            }
        },
        else => {},
    }
}

fn known(tags: []const u64, t: u64) bool {
    for (tags) |k| if (k == t) return true;
    return false;
}

// ---------------------------------------------------------------------------
// §2.3 The dentry pass.
// ---------------------------------------------------------------------------

/// A collected dentry identity (C10's deduped names, the dangling arm).
pub const NameRec = struct {
    /// The parent's KEY ino and the record's key suffix.
    parent: u64,
    suffix: u64,
    child: u64,
    file_type: u8,
};

/// A directory's stripe markers + user names (C17's view), per directory
/// that carries a NUL-led name; the stripes' own names ride the same map.
pub const DirNames = struct {
    markers: std.ArrayList(stripe.Marker) = .empty,
    names: std.ArrayList(stripe.DirRec) = .empty,
};

pub const DentryPass = struct {
    gpa: Allocator,
    arena: std.heap.ArenaAllocator,
    /// Every GLOBAL child a dentry names (the union across volumes).
    referenced: InoBitmap,
    /// Dentry RECORDS naming each ino with more than one — counted only for
    /// inos whose FIRST name marked a bit, so a hostile child is never counted.
    multi: std.AutoHashMapUnmanaged(u64, u32) = .empty,
    multi_complete: bool = true,
    /// Children above the ceiling / outside the routable space: dangling
    /// nominees the bitmap could not index (bounded by the candidate ceiling).
    unindexed: std.ArrayList(NameRec) = .empty,
    /// The identities collected for `collect` (deduped by `(parent, name)`).
    names: std.ArrayList(NameRec) = .empty,
    /// C17: per directory (GLOBAL ino → its markers + user names).
    dirs: std.AutoHashMapUnmanaged(u64, *DirNames) = .empty,
    /// C10's directory arm: per parent (its GLOBAL ino) the names it holds
    /// of a directory, stripe markers excluded (a striped directory's
    /// stripes count their own subdirectories). Counted for EVERY parent
    /// a judged slot holds, before the child shard filter: a directory's
    /// count needs all of its names.
    subdirs: std.AutoHashMapUnmanaged(u64, u32) = .empty,
    subdirs_complete: bool = true,
    refs_indexed: u64 = 0,
    foreign_dentry_scoped: u64 = 0,
    window_scoped: u64 = 0,
    /// Every name of the set is in this pass: no slot is a live lessee's.
    /// A lessee's slot tree (the names its directories hold — a
    /// predecessor's directories included, the slot being the container)
    /// left this forest at the grant, so the referenced set is incomplete
    /// by construction while any slot is foreign-live: the reverse
    /// difference (C9) and the name counts (C10) take no verdict then.
    names_complete: bool = true,
    complete: bool = true,

    pub fn deinit(self: *DentryPass) void {
        self.referenced.deinit();
        self.multi.deinit(self.gpa);
        self.unindexed.deinit(self.gpa);
        self.names.deinit(self.gpa);
        self.dirs.deinit(self.gpa);
        self.subdirs.deinit(self.gpa);
        self.arena.deinit();
        self.* = undefined;
    }

    /// Record names of `global` the pass saw: the multi count, else 1 iff
    /// the bit is set.
    pub fn namesOf(self: *const DentryPass, global: u64) u32 {
        if (self.multi.get(global)) |n| return n;
        const sl = ofGlobalIno(global) orelse return 0;
        return if (self.referenced.isSet(sl)) 1 else 0;
    }

    /// Directory names the pass saw under `dir` (its GLOBAL ino).
    pub fn subdirsOf(self: *const DentryPass, dir: u64) u32 {
        return self.subdirs.get(dir) orelse 0;
    }
};

pub const DentryOptions = struct {
    scope: *const SlotScope,
    shard: ?finding.Shard = null,
    mem_budget: u64,
    /// The referenced bitmap's ceiling (the inode census's raw maximum).
    ceiling: u64,
    /// GLOBAL inos whose name identities to collect (C10's verdict
    /// numerator, the dangling arm) — null = none.
    collect: ?*const std.AutoHashMapUnmanaged(u64, void) = null,
    /// Collect every directory's C17 view (markers + names).
    stripes: bool = true,
    hold: ?*const std.atomic.Value(bool) = null,
};

/// ONE sequential pass over the dentry kind marking every child (a dentry
/// lives on its PARENT's volume and may name a child on another, so the
/// union is the referenced set). Sharded by the CHILD the value carries,
/// never by the key's parent (the shard rule). A projected (unjudged)
/// parent slot is scoped out and counted; a child a foreign window names
/// is scoped out of the sets. Unreadable ⇒ `complete = false`: a truncated
/// referenced set would make every inode beyond the tear look unreferenced.
pub fn dentryPass(gpa: Allocator, store: Store, opts: DentryOptions) Error!DentryPass {
    var p: DentryPass = .{ .gpa = gpa, .arena = std.heap.ArenaAllocator.init(gpa), .referenced = InoBitmap.init(gpa, opts.ceiling, inoSetByteBudget(opts.mem_budget)) };
    errdefer p.deinit();
    const entry_budget = c10CountEntryBudget(opts.mem_budget);
    const ceiling = candidateCeiling(opts.mem_budget);
    const arena = p.arena.allocator();
    var pager = KindPager.init(store, gpa, key.TREE_DENTRIES, 0);
    pager.hold = opts.hold;
    defer pager.deinit();
    while (true) {
        const kv = (pager.next() catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            else => {
                p.complete = false;
                break;
            },
        }) orelse break;
        const dk = key.DentryKey.decode(kv.key) catch continue;
        const parent = dk.parentIno();
        const psl = ofKeyIno(parent);
        if (opts.scope.unjudged(psl.slot)) {
            if (p.foreign_dentry_scoped < DEBUG_SCOPED_NAMES) {
                const dvs = record.DentryValue.decode(kv.value) catch null;
                log.debug("fsck census: dentry of parent {d} (slot {d}) → child {?d} scoped (the parent's slot is a live lessee's)", .{ globalOfKeyIno(parent), psl.slot, if (dvs) |x| x.child_ino else null });
            }
            p.foreign_dentry_scoped += 1;
            continue;
        }
        const dv = record.DentryValue.decode(kv.value) catch continue; // C1's object
        // C17's census rides along: every NUL-led name, every name of a
        // directory that carries one (the shapes need the whole view).
        if (opts.stripes) {
            // Keyed by the directory's GLOBAL ino: a stripe map names its
            // stripes by their globals.
            const d = try dirView(&p, arena, globalOfKeyIno(parent));
            const name = try arena.dupe(u8, dv.name);
            if (stripe.isMarkerName(dv.name)) {
                try d.markers.append(arena, .{ .name = name, .child = dv.child_ino });
            } else {
                try d.names.append(arena, .{ .cookie = key.encodeReaddirCookie(dk.hash54(), dk.collSeq()), .child = dv.child_ino, .file_type = dv.file_type, .name = name });
            }
        }
        if (dv.file_type == economy.ftByte(economy.S.IFDIR) and !stripe.isMarkerName(dv.name)) {
            const pg = globalOfKeyIno(parent);
            if (p.subdirs.count() < entry_budget or p.subdirs.contains(pg)) {
                (try p.subdirs.getOrPutValue(gpa, pg, 0)).value_ptr.* += 1;
            } else p.subdirs_complete = false;
        }
        if (opts.shard) |s| if (!s.owns(dv.child_ino)) continue;
        if (opts.scope.window_inos.contains(dv.child_ino)) {
            p.window_scoped += 1;
            continue;
        }
        const rec: NameRec = .{ .parent = parent, .suffix = dk.suffix(), .child = dv.child_ino, .file_type = dv.file_type };
        const csl = ofGlobalIno(dv.child_ino) orelse {
            if (p.unindexed.items.len < ceiling) try p.unindexed.append(gpa, rec);
            continue;
        };
        // A cross-owner name: the child's slot is a live lessee's (a
        // joiner's file under this mount's directory), so its record is
        // the lessee's to judge — scoped like a dentry of its own tree.
        if (opts.scope.unjudged(csl.slot)) {
            if (p.foreign_dentry_scoped < DEBUG_SCOPED_NAMES) log.debug("fsck census: dentry of parent {d} → child {d} (slot {d}) scoped (the child's slot is a live lessee's)", .{ globalOfKeyIno(parent), dv.child_ino, csl.slot });
            p.foreign_dentry_scoped += 1;
            continue;
        }
        switch (try p.referenced.mark(csl)) {
            .newly_set => p.refs_indexed += 1,
            .already_set => {
                p.refs_indexed += 1;
                if (p.multi.count() < entry_budget or p.multi.contains(dv.child_ino)) {
                    const e = try p.multi.getOrPutValue(gpa, dv.child_ino, 1);
                    e.value_ptr.* += 1;
                } else p.multi_complete = false;
            },
            .refused => {
                if (p.referenced.truncated) {
                    p.complete = false;
                } else if (p.unindexed.items.len < ceiling) try p.unindexed.append(gpa, rec);
            },
        }
        if (opts.collect) |set| if (set.contains(dv.child_ino)) {
            try p.names.append(gpa, rec);
        };
    }
    if (p.referenced.truncated) p.complete = false;
    p.names_complete = opts.scope.foreign_live.count() == 0 and p.foreign_dentry_scoped == 0;
    return p;
}

fn dirView(p: *DentryPass, arena: Allocator, dir: u64) Allocator.Error!*DirNames {
    const gop = try p.dirs.getOrPut(p.gpa, dir);
    if (!gop.found_existing) {
        const d = try arena.create(DirNames);
        d.* = .{};
        gop.value_ptr.* = d;
    }
    return gop.value_ptr.*;
}

// ---------------------------------------------------------------------------
// Tests: the bitmap ties, the budgets, the pagers over a RAM store, the
// scoping law. The census itself is exercised by `classes.zig`'s fixture.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "InoBitmap: one bit per ino, the root never indexes, a ceiling-crossing ino marks nothing, newly_set once, the word-parallel difference and the budget truncation" {
    const gpa = testing.allocator;
    var a = InoBitmap.init(gpa, 1000, 1 << 20);
    defer a.deinit();
    try testing.expectEqual(Mark.refused, try a.mark(.{ .slot = 0, .raw = 1 }));
    try testing.expectEqual(Mark.refused, try a.mark(.{ .slot = 0, .raw = 1001 }));
    try testing.expectEqual(Mark.refused, try a.mark(.{ .slot = 3, .raw = std.math.maxInt(u64) }));
    try testing.expectEqual(Mark.newly_set, try a.mark(.{ .slot = 0, .raw = 2 }));
    try testing.expectEqual(Mark.already_set, try a.mark(.{ .slot = 0, .raw = 2 }));
    try testing.expectEqual(Mark.newly_set, try a.mark(.{ .slot = 7, .raw = 999 }));
    try testing.expectEqual(Mark.newly_set, try a.mark(.{ .slot = 7, .raw = 70 }));
    try testing.expectEqual(@as(u64, 3), a.population);
    try testing.expect(a.isSet(.{ .slot = 7, .raw = 70 }) and !a.isSet(.{ .slot = 7, .raw = 71 }) and !a.isSet(.{ .slot = 9, .raw = 2 }));
    var b = InoBitmap.init(gpa, 1000, 1 << 20);
    defer b.deinit();
    _ = try b.mark(.{ .slot = 7, .raw = 70 });
    var seen: std.ArrayList(SlotLocal) = .empty;
    defer seen.deinit(gpa);
    const V = struct {
        fn visit(list: *std.ArrayList(SlotLocal), sl: SlotLocal) bool {
            list.append(testing.allocator, sl) catch return false;
            return true;
        }
    };
    a.eachAbsentFrom(&b, &seen, V.visit);
    try testing.expectEqual(@as(usize, 2), seen.items.len);
    // Only one slot's words fit a 16-word budget: the second slot truncates.
    var t = InoBitmap.init(gpa, 1000, 16 * 8);
    defer t.deinit();
    try testing.expectEqual(Mark.newly_set, try t.mark(.{ .slot = 0, .raw = 2 }));
    try testing.expectEqual(Mark.refused, try t.mark(.{ .slot = 1, .raw = 2 }));
    try testing.expect(t.truncated);
}

test "the derived budgets restate their formulas (drift is red)" {
    try testing.expectEqual(@as(u64, 16 * derive.MiB), inoSetByteBudget(64 * derive.MiB));
    try testing.expectEqual(@as(u64, 1 * derive.GiB), inoSetByteBudget(64 * derive.GiB));
    try testing.expectEqual(inoSetByteBudget(8 * derive.GiB) / 64, c10CountEntryBudget(8 * derive.GiB));
    try testing.expectEqual(inoSetByteBudget(8 * derive.GiB) / 8, candidateCeiling(8 * derive.GiB));
    try testing.expectEqual(@as(u32, 8), c1Wave(2));
    try testing.expectEqual(@as(u32, 4), c1Wave(0));
    // 100 M inodes / 8 = 12.5 MB of bits fit the 16 MiB floor.
    try testing.expect(100_000_000 / 8 < inoSetByteBudget(0));
}

test "the walk bounds restate their formulas (drift is red): reads within 3 × visited + the trees consulted, node reads within the height per entry visited and per call" {
    const t: WalkTally = .{ .calls = 10, .visited = 5_000, .read = 0, .nodes = 0, .sources = 70 };
    try testing.expectEqual(@as(u64, 3 * 5_000 + 70), walkReadBound(t));
    try testing.expectEqual(@as(u64, 3 * (5_000 + 10)), walkNodeBound(t, 3));
    // A store without trees reads what it delivers: inside the bound.
    try testing.expect(t.visited <= walkReadBound(t));
}

test "ino arithmetic: a key ino and the global ino it encodes decompose to the SAME (slot, raw); the root is slot 0 raw 1; an unroutable global is no index" {
    const w = inos.ROUTING_WIDTH;
    const global = inos.makeGlobalIno(77, 5, w);
    const eff = route_table.effectiveIno(global);
    try testing.expectEqual(ofKeyIno(eff), ofGlobalIno(global).?);
    try testing.expectEqual(SlotLocal{ .slot = 5, .raw = 77 }, ofKeyIno(eff));
    try testing.expectEqual(global, globalOfKeyIno(eff));
    try testing.expectEqual(eff, keyInoOf(.{ .slot = 5, .raw = 77 }));
    try testing.expectEqual(SlotLocal{ .slot = 0, .raw = 1 }, ofGlobalIno(1).?);
    try testing.expect(ofGlobalIno(0) == null);
    try testing.expect(ofGlobalIno(std.math.maxInt(u64)) == null);
}

test "an inode-owned key ino has a global ino; a slot's reserved locals below the mint base — the cross-owner intent HOME (local 0) and the root pin's slot-0 siblings — own none (0003 §X.6; found by the online fsck row: the live window scan of a joiner's ring met an intent record and A's daemon died on the arithmetic's assertion — the admin lane's `HolderUnreachable, AGAIN`)" {
    const w = inos.ROUTING_WIDTH;
    const eff = route_table.effectiveIno(inos.makeGlobalIno(77, 5, w));
    try testing.expectEqual(inos.makeGlobalIno(77, 5, w), globalOfOwnedKeyIno(eff).?);
    try testing.expectEqual(@as(?u64, 1), globalOfOwnedKeyIno(1));
    try testing.expectEqual(@as(?u64, null), globalOfOwnedKeyIno(key.guestLocalIno(5, 0)));
    try testing.expectEqual(@as(?u64, null), globalOfOwnedKeyIno(key.guestLocalIno(5, 1)));
    try testing.expectEqual(@as(?u64, null), globalOfOwnedKeyIno(0));
}

test "SlotScope: the unjudged predicate per posture; the era floor is three-valued with tree 0 as the witness; the inode plane abstains on an unreadable window or a reader's uncovered ring" {
    const gpa = testing.allocator;
    var s = SlotScope.init(gpa);
    defer s.deinit();
    try s.leased.put(gpa, 0, {});
    try s.leased.put(gpa, 3, {});
    try s.foreign_live.put(gpa, 9, {});
    try s.unleased.put(gpa, 12, {});
    try s.era_floor.put(gpa, 3, 100);
    try testing.expect(!s.unjudged(3) and s.unjudged(9) and !s.unjudged(12));
    s.posture = .member;
    try testing.expect(!s.unjudged(3) and s.unjudged(12) and s.unjudged(9));
    s.posture = .manager;
    try testing.expectEqual(Era.prior, s.era(.{ .slot = 3, .raw = 99 }));
    try testing.expectEqual(Era.current, s.era(.{ .slot = 3, .raw = 100 }));
    try testing.expectEqual(Era.unknown, s.era(.{ .slot = 4, .raw = 2 }));
    try testing.expectEqual(Era.prior, s.era(.{ .slot = 12, .raw = 1 << 30 }));
    try testing.expectEqual(Era.unknown, s.era(.{ .slot = 9, .raw = 2 }));
    s.posture = .probe;
    try testing.expectEqual(Era.prior, s.era(.{ .slot = 4, .raw = 2 }));
    try testing.expectEqual(Era.unknown, s.era(.{ .slot = 9, .raw = 2 }));
    try testing.expect(s.inodePlaneJudges());
    s.window_unknown = true;
    try testing.expect(!s.inodePlaneJudges());
    s.window_unknown = false;
    s.reader_uncovered = true;
    try testing.expect(!s.inodePlaneJudges());
    try testing.expectEqual(@as(u64, 3), s.slotsCovered());
}
