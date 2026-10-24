//! The READER side of GPFS-strict read tokens (charter §5.5; `docs/
//! porting/sym_tokens_intents_striping.md` §1.1 `TokenEntry`, §1.3 G/R/A/L,
//! §1.6 the cache, the serve gate, the budget, §1.7 the ack conditions,
//! §1.10 the re-point): one `Reader` per (volume, holder) — the cache keyed
//! by object (a `lookup` is one hash probe, a `readdir` page one binary
//! search on the cookie-ordered set; an entry is immutable once installed:
//! a serve that began before the recall serves the records it holds,
//! linearizable at its start), the paged fetch under the holder's budget,
//! the install-under-generation law (the recall bumps the generation
//! BEFORE it removes the entry; the install checks `gen == gen0` UNDER the
//! cache lock), single-flight per object, the R5 component
//! `dlm_token_records_bytes` with voluntary releases and the authority's
//! shed, and the ACK CONDITIONS: a recall is acked once its entries are
//! revoked, the kernel told and the scoped block-key purge ran (a DIRTY
//! local layout entry is KEPT: the writer's acked bytes). No drain of the
//! serves in flight: what such a serve hands on is ordered by the read it
//! feeds — a block deposited or served under a revoked binding meets the
//! read path's binding recheck AFTER its bytes are in hand (a revoked token
//! misses and re-grants; a lost binding purges the key and retries), and
//! the kernel orders its own caches behind the invalidations (an
//! `INVAL_ENTRY` takes the parent's `i_rwsem`, which a lookup or readdir in
//! flight holds; an `INVAL_INODE` bumps the inode's `attr_version`, which
//! drops a GETATTR reply already in flight, and waits out the locked folios
//! of reads in flight).
//!
//! THE PURGE LAW: every entry's exit purges exactly the keys its carried
//! `layout` names — a recall's revoke, a voluntary release, room-making's
//! kernel-pinned drop and the shed alike — so the read tiers hold a
//! foreign key only under an entry that names it (a serve in flight under
//! a gone entry re-grants at its binding recheck; the fresh entry names
//! what it deposited). A recall of an object this reader holds no entry
//! for (the holder's kernel pin, a drop without a release) owes the kernel
//! its invalidation and the tiers nothing. The whole-foreign census is
//! reserved for keys the reader cannot name: a held entry whose layout
//! does not enumerate them (a block-map tree head), or a revoke that could
//! keep no entries; it is counted, so a storm is visible.
//! PORTABLE: the wire is a `Link`, the data plane a `RecallSink`.
//!
//! The recall is a PUSH (decision 0002 row 257): the holder's frame
//! arrives on this reader's listener and the push's reply IS the ack.
//! The "fresh channel" law of the long-poll design becomes the posture
//! word `channel_ok`: a holder whose last round (grant or push) failed
//! serves NOTHING from this reader until a round completes (fail-closed).

const std = @import("std");
const Allocator = std.mem.Allocator;
const Io = std.Io;
const counters = @import("../../../core/counters.zig");
const errors = @import("../../../core/errors.zig");
const log = @import("../../../core/log.zig");
const clock = @import("../../../core/clock.zig");
const sync = @import("../../../sync/sync.zig");
const verbs = @import("../../../wire/verbs.zig");
const codec = @import("../../../wire/codec.zig");
const frame = @import("../../../wire/frame.zig");
const record = @import("../../record.zig");
const key = @import("../../key.zig");
const mem_budget = @import("../../../mem/budget.zig");
const table = @import("token_table.zig");

const assert = std.debug.assert;
const ns_per_ms: u64 = std.time.ns_per_ms;

pub const Object = table.Object;
pub const ClientId = table.ClientId;

// ---------------------------------------------------------------------------
// Constants — value and WHY.
// ---------------------------------------------------------------------------

/// `records_budget_bytes = max(mem_budget / 256, CONTROL_CAP)`: 64 MiB at
/// 16 GiB ≈ 1.6 M dentries or ≈ 1 000 full `layout` values — a reader's
/// live interest set; the floor is the physical minimum under which one
/// grant page could be resident.
pub fn recordsBudgetBytes(mem_budget_bytes: u64) u64 {
    return @max(mem_budget_bytes / 256, @as(u64, frame.CONTROL_CAP));
}

/// The dentry WINDOW a directory's entry holds resident (spec Q13, the
/// paged-resident set): a quarter of the records budget, so three other
/// objects' entries stay resident beside one huge directory's window; at
/// least one wire page, so a tiny budget still pages instead of refusing.
pub fn dirWindowBytes(records_budget_bytes: u64) u64 {
    return @max(records_budget_bytes / 4, @as(u64, frame.CONTROL_CAP));
}

comptime {
    assert(recordsBudgetBytes(16 << 30) == 64 << 20);
    assert(recordsBudgetBytes(0) == 1 << 20);
}

// ---------------------------------------------------------------------------
// Instruments — the reader-side `dlm_token_*` family and the serve ledger.
// ---------------------------------------------------------------------------

pub const stats = struct {
    /// Grants fetched (1 RPC per object first touch or after a recall).
    pub var dlm_token_grants: counters.ShardedCounter = .{};
    /// Live entries (gauge).
    pub var dlm_token_cached: counters.Gauge = .{};
    /// Serves answered from the cache: 0 RPCs, 0 device reads.
    pub var dlm_token_hits: counters.ShardedCounter = .{};
    pub var dlm_token_recalls_received: counters.ShardedCounter = .{};
    pub var dlm_token_recalls_acked: counters.ShardedCounter = .{};
    pub var dlm_token_reader_releases: counters.ShardedCounter = .{};
    /// The R5 component `dlm_token_records_bytes` (gauge).
    pub var dlm_token_cached_bytes: counters.Gauge = .{};
    /// Entries the authority's shed dropped WITHOUT a release (one needless
    /// recall per shed object, never a wrong answer).
    pub var dlm_token_sheds: counters.ShardedCounter = .{};
    /// Entries room-making dropped WITHOUT a release because the kernel may
    /// still cache an answer served under them (decision 0003 §AG.2): the
    /// holder keeps recalling this reader, so its change reaches the
    /// kernel by name — one needless recall per dropped object at worst.
    pub var dlm_token_kernel_pinned_drops: counters.ShardedCounter = .{};
    /// ONE entry over the whole records budget: refused loud.
    pub var dlm_token_oversize_refusals: counters.ShardedCounter = .{};
    /// Dentry fetches cut at the window bound (the set is larger than one
    /// resident window; the rest pages in on demand).
    pub var dlm_token_dir_windows: counters.ShardedCounter = .{};
    /// Block-map fetches cut at the window bound (the map is larger than
    /// one resident window; the rest pages in on demand).
    pub var dlm_token_map_windows: counters.ShardedCounter = .{};
    /// Serves refused by the gate (lease past T_self, channel not ok).
    pub var dlm_token_serve_refusals: counters.ShardedCounter = .{};
    /// Rounds completed on a holder's channel (grants + pushes).
    pub var dlm_token_channel_rounds: counters.ShardedCounter = .{};
    /// A fetch that spanned a recall retried.
    pub var dlm_token_fetch_retries: counters.ShardedCounter = .{};
    /// A single-flight loser re-read the winner's entry.
    pub var dlm_token_fetch_cohort_waits: counters.ShardedCounter = .{};
    /// Posture: 1 iff EVERY plane's channel is ok.
    pub var dlm_token_channel_ok: counters.Gauge = .{};
    pub var dlm_token_holder_repoints: counters.ShardedCounter = .{};
    pub var dlm_token_grant_rtt_ns: counters.ShardedLatencyHistogram = .{};
    /// Block keys purged by the scoped purge; scoped purges; whole-census
    /// fallbacks (an entry whose keys its layout does not enumerate — at
    /// its recall or its drop —, a revoke that kept no entries).
    pub var dlm_token_recall_purged_keys: counters.ShardedCounter = .{};
    pub var dlm_token_recall_scoped_purges: counters.ShardedCounter = .{};
    pub var dlm_token_recall_census_purges: counters.ShardedCounter = .{};
    /// Recalled objects this reader held no entry for (the holder's kernel
    /// pin after an earlier recall, a drop without a release): the kernel
    /// heard the invalidation; the read tiers owed nothing.
    pub var dlm_token_recall_unheld: counters.ShardedCounter = .{};
    /// Block keys purged at a drop WITHOUT a release (room-making's
    /// kernel-pinned drop, the shed): the entry's exit purges what it named.
    pub var dlm_token_drop_purged_keys: counters.ShardedCounter = .{};
    /// A DIRTY local layout entry KEPT across a recall (the writer's acked
    /// bytes; 0 on a `-o ro` reader).
    pub var dlm_token_recall_dirty_kept: counters.ShardedCounter = .{};
    /// A held (3.7) stripe skipped the discard (every clean entry is
    /// already pre-step).
    pub var dlm_token_recall_discard_skipped: counters.ShardedCounter = .{};
};

// ---------------------------------------------------------------------------
// Seams.
// ---------------------------------------------------------------------------

/// The dialed session to the holder: ONE call, the typed reply.
pub const Link = struct {
    ctx: *anyopaque,
    call: *const fn (ctx: *anyopaque, req: verbs.Request) errors.Error!verbs.Reply,
};

/// The data-plane half of the ack (spec §1.7 steps 2–3), decided under the
/// ino's (3.7) stripe in its NON-PARKING form.
pub const DiscardVerdict = union(enum) {
    /// The clean entry is gone and its block keys purged through the ONE
    /// purge helper: how many.
    discarded: u32,
    /// The entry IS a pending write (an inline write's `data_key` is the
    /// ONLY copy of the acked bytes until the fsync's save): kept.
    dirty_kept,
    /// The stripe is held: skipped — safe, the step made every clean entry
    /// pre-step already.
    held_skipped,
    /// No entry held for the object (recalled before and still pinned at
    /// its holder, dropped without a release, never granted): nothing to
    /// purge — the entry's exit purged what it named.
    no_entry,
    /// An entry is held but its layout does not enumerate its keys (a
    /// block-map tree head, a value that does not decode): the whole
    /// census, ONCE per call.
    unmapped,
};

pub const RecallSink = struct {
    ctx: *anyopaque,
    /// `entry` is the object's revoked entry (its carried records, the
    /// `layout` xattr among them — what the scoped purge enumerates), held
    /// pinned through the call; null when none was held.
    discardLayout: *const fn (ctx: *anyopaque, object: Object, entry: ?*const Entry) DiscardVerdict,
    purgeCensus: *const fn (ctx: *anyopaque) void,
    /// The FUSE-layer hook: the recalled object's kernel view is invalidated
    /// (`FUSE_NOTIFY_INVAL_INODE` + `PRUNE` through the session).
    noteRecalled: *const fn (ctx: *anyopaque, object: Object) void,
    /// A name the recalled batch changes under `parent` (decision 0003
    /// §AB.2): `FUSE_NOTIFY_INVAL_ENTRY` before the ack.
    noteName: *const fn (ctx: *anyopaque, parent: Object, name: []const u8) void,
    /// The batch changed a dentry it could not name: every dentry goes.
    noteUnnamed: *const fn (ctx: *anyopaque) void,

    /// A reader with no data plane bound (the contracts): nothing to purge.
    pub const none: RecallSink = .{ .ctx = @ptrFromInt(1), .discardLayout = &noDiscard, .purgeCensus = &noPurge, .noteRecalled = &noNote, .noteName = &noName, .noteUnnamed = &noUnnamed };

    fn noName(_: *anyopaque, _: Object, _: []const u8) void {}
    fn noUnnamed(_: *anyopaque) void {}

    fn noDiscard(_: *anyopaque, _: Object, _: ?*const Entry) DiscardVerdict {
        return .no_entry;
    }
    fn noPurge(_: *anyopaque) void {}
    fn noNote(_: *anyopaque, _: Object) void {}
};

/// The reader's own membership lease (`T_self`): a reader that cannot be
/// recalled serves NOTHING.
pub const SelfLease = struct {
    ctx: ?*anyopaque,
    live: *const fn (ctx: ?*anyopaque, now_ns: u64) bool,

    pub const always: SelfLease = .{ .ctx = null, .live = &alwaysLive };
    fn alwaysLive(_: ?*anyopaque, _: u64) bool {
        return true;
    }
};

// ---------------------------------------------------------------------------
// The entry.
// ---------------------------------------------------------------------------

pub const Xattr = struct { name: []const u8, value: []const u8 };
pub const DirRecord = struct { cookie: u64, child_ino: u64, file_type: u8, name: []const u8 };

pub const State = enum { live, revoked };

/// Immutable once installed; pinned by every serve that holds it.
pub const Entry = struct {
    arena: std.heap.ArenaAllocator,
    object: Object,
    attrs: record.InodeValue,
    /// Sorted by name (the holder pages them in name order).
    xattrs: []Xattr,
    xattrs_complete: bool,
    /// Cookie order; null when the fetch did not ask for dentries. The
    /// resident WINDOW of the set: the records STRICTLY after `dir_from`
    /// up to the set's end (`dir_complete`) or the window's byte bound.
    dir: ?[]DirRecord,
    dir_from: u64 = 0,
    dir_complete: bool = true,
    /// A tree-mapped file's block-map WINDOW (null when the fetch did not
    /// ask for it): every record resolving a block at or above `map_from`
    /// in index order (the floor's coverer first), up to the file's last
    /// record (`map_complete`) or the window's byte bound. The data plane
    /// resolves this mount's bindings of a foreign tree-mapped file off it
    /// — and only off it, so the keys a recall must purge are exactly the
    /// keys its records name (THE PURGE LAW).
    map: ?[]record.MapRecordView = null,
    map_from: u64 = 0,
    map_complete: bool = true,
    /// `128 + Σ xattr bytes + Σ (24 + name) per dentry`.
    bytes: u64,
    state: State = .live,
    /// The second-chance bit of `evictToBudget`.
    used: bool = false,
    pins: u32 = 0,
    /// Until this instant the kernel may cache an answer a serve built from
    /// this entry (decision 0003 §AG.2): the last serve + the mount's
    /// longest kernel TTL. A token is RELEASED only past it.
    kernel_pin_until_ns: u64 = 0,
    /// THIS mount's access stamp the holder applied after the grant (an
    /// access stamp recalls no reader — `token_table.ObjectUnion`): the
    /// records stay immutable; the toucher's own reads fold it so its
    /// atime never steps back. Monotone; 0 = none.
    atime_floor: std.atomic.Value(u64) = .init(0),

    /// The holder applied this mount's access stamp `stamp`.
    pub fn raiseAtime(self: *Entry, stamp: u64) void {
        _ = self.atime_floor.fetchMax(stamp, .monotonic);
    }

    /// The record's atime under this mount's own applied stamps.
    pub fn atime(self: *const Entry) u64 {
        return @max(self.attrs.atime, self.atime_floor.load(.monotonic));
    }

    pub fn xattr(self: *const Entry, name: []const u8) ?[]const u8 {
        for (self.xattrs) |x| if (std.mem.eql(u8, x.name, name)) return x.value;
        return null;
    }

    /// One hash-free probe over the cookie order would need an index; the
    /// dentry sets a page carries are bounded by the budget, so a name
    /// lookup is a linear scan here and a cookie page a binary search.
    pub fn lookupName(self: *const Entry, name: []const u8) ?DirRecord {
        const d = self.dir orelse return null;
        for (d) |r| if (std.mem.eql(u8, r.name, name)) return r;
        return null;
    }

    /// Does the resident window answer a page STRICTLY after `cookie`:
    /// the window starts at or before it and either reaches the set's end
    /// or holds at least one record past it.
    pub fn coversAfter(self: *const Entry, cookie: u64) bool {
        const d = self.dir orelse return false;
        if (cookie < self.dir_from) return false;
        return self.dir_complete or (d.len > 0 and cookie < d[d.len - 1].cookie);
    }

    /// Does the map window resolve `block`: it starts at or before it and
    /// either reaches the map's end or holds a record keyed at or past it
    /// (every record keyed at or below `block` is then resident).
    pub fn coversBlock(self: *const Entry, block: u64) bool {
        const m = self.map orelse return false;
        if (block < self.map_from) return false;
        return self.map_complete or (m.len > 0 and block <= m[m.len - 1].index);
    }

    /// The record `block` resolves to under the read law (the LAST record
    /// keyed at or below it that covers it — an exact record supersedes a
    /// covering run); null = a hole. The window must cover `block`.
    pub fn mapRecordFor(self: *const Entry, block: u32) ?record.MapRecordView {
        const m = self.map orelse return null;
        var found: ?record.MapRecordView = null;
        for (m) |r| {
            if (r.index > block) break;
            if (r.covers(block)) found = r;
        }
        return found;
    }

    /// Is a name's absence from the window DEFINITIVE: every cookie a
    /// record of `hash54` can carry (`coll_seq` 0…255) lies inside it.
    pub fn coversName(self: *const Entry, hash54: u64) bool {
        const d = self.dir orelse return false;
        const first = key.encodeReaddirCookie(hash54, 0);
        const last = key.encodeReaddirCookie(hash54, std.math.maxInt(u8));
        if (first <= self.dir_from) return false;
        return self.dir_complete or (d.len > 0 and d[d.len - 1].cookie >= last);
    }

    /// The last cookie the window holds (`dir_from` when empty).
    pub fn lastCookie(self: *const Entry) u64 {
        const d = self.dir orelse return self.dir_from;
        return if (d.len == 0) self.dir_from else d[d.len - 1].cookie;
    }

    /// The records STRICTLY after `cookie` (0 = start).
    pub fn pageAfter(self: *const Entry, cookie: u64) []const DirRecord {
        const d = self.dir orelse return &.{};
        var lo: usize = 0;
        var hi: usize = d.len;
        while (lo < hi) {
            const mid = lo + (hi - lo) / 2;
            if (d[mid].cookie <= cookie) lo = mid + 1 else hi = mid;
        }
        return d[lo..];
    }

    /// The arena AND the struct (the arena's child allocator created it).
    fn destroy(self: *Entry) void {
        var arena = self.arena;
        const gpa = arena.child_allocator;
        arena.deinit();
        gpa.destroy(self);
    }
};

/// A pinned serve: the entry (or `gone`), released by the caller.
pub const Serve = struct {
    entry: ?*Entry,
    cache: *Cache,

    pub fn gone(self: Serve) bool {
        return self.entry == null;
    }

    pub fn release(self: Serve, io: Io) void {
        const e = self.entry orelse return;
        self.cache.unpin(io, e);
    }
};

// ---------------------------------------------------------------------------
// The cache.
// ---------------------------------------------------------------------------

pub const Cache = struct {
    gpa: Allocator,
    lock: sync.Mutex = .init,
    /// The longest TTL the mount's kernel caches an answer for (decision
    /// 0003 §AG.2; `Divert.setKernelPin`): every serve pins its entry this
    /// long. Until the mount names it, never released (the coherent default:
    /// a needless recall, never a stale kernel entry).
    kernel_pin_ns: u64 = std.math.maxInt(u64),
    entries: std.AutoHashMapUnmanaged(Object, *Entry) = .empty,
    /// Per-recall increments, never merged (the monotone-word lesson).
    revoke_gens: std.AutoHashMapUnmanaged(Object, u64) = .empty,
    bytes: u64 = 0,
    budget_bytes: u64,
    /// The whole-budget census for every plane in the process rides the
    /// module gauge; this instance's share is `bytes`.
    gone_markers: u64 = 0,
    /// Entries dropped WITHOUT a release whose keys their dropper has not
    /// purged yet (`Reader.purgeDropped`): a recall that finds no entry
    /// while one is in flight cannot tell whether the tiers still hold its
    /// object's keys.
    drops_in_flight: u32 = 0,
    /// Entries an install displaced that carried a map window: they leave
    /// like a drop (pinned, counted in `drops_in_flight`) for the
    /// installer's purge (`Reader.purgeDisplaced`) — the window's keys may
    /// sit in the tiers and the new entry need not name them.
    displaced: std.ArrayListUnmanaged(*Entry) = .empty,

    pub fn init(gpa: Allocator, budget_bytes: u64) Cache {
        return .{ .gpa = gpa, .budget_bytes = budget_bytes };
    }

    pub fn deinit(self: *Cache) void {
        var it = self.entries.valueIterator();
        while (it.next()) |e| e.*.destroy();
        self.entries.deinit(self.gpa);
        self.revoke_gens.deinit(self.gpa);
        for (self.displaced.items) |e| e.destroy();
        self.displaced.deinit(self.gpa);
    }

    pub fn revokeGen(self: *Cache, io: Io, object: Object) u64 {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        return self.revoke_gens.get(object) orelse 0;
    }

    /// A hit: LIVE and carrying the dentries when asked; pinned.
    pub fn probe(self: *Cache, io: Io, object: Object, wants: Wants) ?*Entry {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        const e = self.entries.get(object) orelse return null;
        if (e.state != .live or (wants.dentries and !e.coversAfter(wants.after)) or (wants.map and !e.coversBlock(wants.after))) return null;
        e.pins += 1;
        e.used = true;
        self.pinForKernelLocked(e);
        return e;
    }

    /// A serve hands `e`'s records to the kernel's caches: the token stays
    /// this reader's (at its holder) for as long as they may live there.
    fn pinForKernelLocked(self: *const Cache, e: *Entry) void {
        e.kernel_pin_until_ns = @max(e.kernel_pin_until_ns, clock.monotonicNs() +| self.kernel_pin_ns);
    }

    fn kernelPinned(e: *const Entry, now_ns: u64) bool {
        return e.kernel_pin_until_ns > now_ns;
    }

    pub fn unpin(self: *Cache, io: Io, e: *Entry) void {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        e.pins -= 1;
        if (e.pins == 0 and e.state == .revoked) e.destroy();
    }

    /// The resident entry's words for a miss's diagnostic (null = none).
    pub fn describe(self: *Cache, io: Io, object: Object) ?struct { state: State, has_dir: bool } {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        const e = self.entries.get(object) orelse return null;
        return .{ .state = e.state, .has_dir = e.dir != null };
    }

    pub const InstallOutcome = union(enum) {
        /// The entry now served (pinned): `e`, or the richer entry it did
        /// not displace.
        installed: *Entry,
        recalled_mid_fetch,
        oversize,
    };

    /// Install under `gen0`, DECIDED UNDER THE CACHE LOCK: a bump the check
    /// sees aborts the install (the fetch read pre-commit pages); a bump
    /// after the check finds the installed entry. A dentry-bearing entry is
    /// never displaced by a bare one. Room is made by `evictToBudget`
    /// first (the caller's voluntary releases). `e` is consumed: pinned
    /// on `installed`, destroyed otherwise.
    pub fn install(self: *Cache, io: Io, e: *Entry, gen0: u64) InstallOutcome {
        if (e.bytes > self.budget_bytes) {
            stats.dlm_token_oversize_refusals.inc();
            log.err("tokens: ONE entry (object {d}, {d} B) exceeds the whole records budget ({d} B) — refused; the directory is too large for this reader's budget", .{ e.object, e.bytes, self.budget_bytes });
            e.destroy();
            return .oversize;
        }
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        if ((self.revoke_gens.get(e.object) orelse 0) != gen0) {
            e.destroy();
            return .recalled_mid_fetch;
        }
        if (self.entries.get(e.object)) |old| {
            if ((old.dir != null and e.dir == null) or (old.map != null and e.map == null)) {
                // Keep the richer entry; the caller serves it.
                e.destroy();
                old.pins += 1;
                old.used = true;
                self.pinForKernelLocked(old);
                return .{ .installed = old };
            }
            if (old.map != null) {
                self.displaced.ensureUnusedCapacity(self.gpa, 1) catch {
                    e.destroy();
                    return .oversize;
                };
                old.pins += 1;
                self.displaced.appendAssumeCapacity(old);
                self.drops_in_flight += 1;
            }
            self.removeLocked(old);
        }
        self.entries.put(self.gpa, e.object, e) catch {
            e.destroy();
            return .oversize;
        };
        self.bytes += e.bytes;
        stats.dlm_token_cached_bytes.add(e.bytes);
        stats.dlm_token_cached.add(1);
        e.pins += 1;
        self.pinForKernelLocked(e);
        return .{ .installed = e };
    }

    fn removeLocked(self: *Cache, e: *Entry) void {
        _ = self.entries.remove(e.object);
        self.bytes -= e.bytes;
        stats.dlm_token_cached_bytes.sub(e.bytes);
        stats.dlm_token_cached.sub(1);
        e.state = .revoked;
        if (e.pins == 0) e.destroy();
    }

    /// The recall's first half, per object: `revoke_gens[o] += 1` FIRST,
    /// then the entry leaves (REVOKED, bytes credited) — pinned into
    /// `kept[i]` (null when none was held), so the purge after it still
    /// reads its carried records; the caller unpins. Answers how many
    /// entries were held.
    pub fn revoke(self: *Cache, io: Io, objects: []align(1) const Object, kept: []?*Entry) Allocator.Error!u32 {
        assert(kept.len == objects.len);
        @memset(kept, null);
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        var held: u32 = 0;
        for (objects, kept) |o, *k| {
            const gop = try self.revoke_gens.getOrPut(self.gpa, o);
            if (!gop.found_existing) gop.value_ptr.* = 0;
            gop.value_ptr.* += 1;
            if (self.entries.get(o)) |e| {
                e.pins += 1;
                self.removeLocked(e);
                k.* = e;
                held += 1;
            }
        }
        return held;
    }

    /// Voluntary room: second-chance `used` bit, then any; the retired
    /// objects go to `out` and their entries, pinned, to `kept` (the
    /// caller's drain + purge + `Release`, then the unpin). An entry the
    /// kernel may still cache an answer from is dropped WITHOUT a release,
    /// pinned into `dropped` for the caller's purge (`dropOrRetireLocked`).
    pub fn evictToBudget(self: *Cache, io: Io, need: u64, out: *std.ArrayList(Object), kept: *std.ArrayList(?*Entry), dropped: *std.ArrayList(*Entry)) Allocator.Error!void {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        const now = clock.monotonicNs();
        var pass: u8 = 0;
        while (pass < 2 and self.bytes + need > self.budget_bytes) : (pass += 1) {
            var victims: std.ArrayListUnmanaged(*Entry) = .empty;
            defer victims.deinit(self.gpa);
            var it = self.entries.valueIterator();
            while (it.next()) |ep| {
                const e = ep.*;
                if (pass == 0 and e.used) {
                    e.used = false;
                    continue;
                }
                try victims.append(self.gpa, e);
            }
            for (victims.items) |e| {
                if (self.bytes + need <= self.budget_bytes) break;
                try self.dropOrRetireLocked(e, now, out, kept, dropped);
            }
        }
    }

    /// The room law for one victim (decision 0003 §AG.2): kernel-pinned ⇒
    /// dropped WITHOUT a release — the holder keeps this reader on the
    /// object, so a change still recalls it and its names reach the kernel
    /// — pinned into `dropped` (the caller purges its keys); else retired
    /// to `out`/`kept` for the caller's release.
    fn dropOrRetireLocked(self: *Cache, e: *Entry, now_ns: u64, out: *std.ArrayList(Object), kept: *std.ArrayList(?*Entry), dropped: *std.ArrayList(*Entry)) Allocator.Error!void {
        if (kernelPinned(e, now_ns)) {
            try dropped.ensureUnusedCapacity(self.gpa, 1);
            stats.dlm_token_kernel_pinned_drops.inc();
            e.pins += 1;
            dropped.appendAssumeCapacity(e);
            self.drops_in_flight += 1;
            self.removeLocked(e);
            return;
        }
        try out.ensureUnusedCapacity(self.gpa, 1);
        try kept.ensureUnusedCapacity(self.gpa, 1);
        out.appendAssumeCapacity(e.object);
        e.pins += 1;
        kept.appendAssumeCapacity(e);
        self.removeLocked(e);
    }

    /// One object out under the room law (`SQUEEZEFS_TEST_TOKEN_EVICT_AFTER_SERVE`:
    /// the budget's eviction of exactly the entry a serve just used).
    pub fn evictOne(self: *Cache, io: Io, object: Object, out: *std.ArrayList(Object), kept: *std.ArrayList(?*Entry), dropped: *std.ArrayList(*Entry)) Allocator.Error!void {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        const e = self.entries.get(object) orelse return;
        try self.dropOrRetireLocked(e, clock.monotonicNs(), out, kept, dropped);
    }

    /// The shed: every entry dropped WITHOUT a release, pinned into
    /// `dropped` for the caller's purge (an entry the list has no room for
    /// stays). Answers the entries dropped.
    pub fn shedAll(self: *Cache, io: Io, dropped: *std.ArrayList(*Entry)) u32 {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        const first = dropped.items.len;
        var it = self.entries.valueIterator();
        while (it.next()) |ep| dropped.append(self.gpa, ep.*) catch break;
        for (dropped.items[first..]) |e| {
            e.pins += 1;
            self.removeLocked(e);
        }
        const n: u32 = @intCast(dropped.items.len - first);
        self.drops_in_flight += n;
        stats.dlm_token_sheds.add(n);
        return n;
    }

    /// The entries installs displaced (moved into `out`, pinned).
    pub fn takeDisplaced(self: *Cache, io: Io, out: *std.ArrayList(*Entry)) Allocator.Error!void {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        try out.appendSlice(self.gpa, self.displaced.items);
        self.displaced.clearRetainingCapacity();
    }

    /// `n` dropped entries' keys are purged (`drops_in_flight`).
    pub fn dropsPurged(self: *Cache, io: Io, n: u32) void {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        self.drops_in_flight -= n;
    }

    pub fn dropsInFlight(self: *Cache, io: Io) u32 {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        return self.drops_in_flight;
    }

    pub fn count(self: *Cache, io: Io) usize {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        return self.entries.count();
    }

    pub fn bytesHeld(self: *Cache, io: Io) u64 {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        return self.bytes;
    }
};

// ---------------------------------------------------------------------------
// The reader plane.
// ---------------------------------------------------------------------------

pub const Error = error{
    /// The reader's own lease is past `T_self`, or the holder's channel is
    /// not ok: the serve is refused (retryable) — the bounded-staleness
    /// projection is never served in its place.
    TokenChannelStale,
    MembershipPending,
    /// The holder answered `NotHolder{holder}` (the caller re-resolves once).
    StaleHolderView,
    /// The holder answered `HolderDead{holder}`.
    HolderUnreachable,
    /// A grant paged past the records budget (loud).
    Oversize,
    /// The wire failed (a poisoned session, a refused frame).
    Wire,
    OutOfMemory,
};

pub fn coreError(e: Error) errors.Error {
    return switch (e) {
        error.TokenChannelStale => error.TokenChannelStale,
        error.MembershipPending => error.MembershipPending,
        error.StaleHolderView => error.StaleHolderView,
        error.HolderUnreachable => error.HolderUnreachable,
        error.Oversize => error.ValueTooLarge,
        error.Wire => error.ConnectionLost,
        error.OutOfMemory => error.OutOfMemory,
    };
}

pub const Wants = struct {
    dentries: bool = false,
    records: bool = true,
    /// The block-map window (never with `dentries`): `after` is the block
    /// the resident window must resolve, else the fetch starts there.
    map: bool = false,
    /// With `dentries`: the cookie the caller's page or lookup starts
    /// STRICTLY after (0 = the set's start); the resident window must
    /// cover it, else the fetch starts there.
    after: u64 = 0,

    /// The window a lookup of a name hashing to `hash54` needs: from just
    /// before its first possible cookie.
    pub fn forName(hash54: u64) Wants {
        return .{ .dentries = true, .after = key.encodeReaddirCookie(hash54, 0) - 1 };
    }
};

pub const Reader = struct {
    gpa: Allocator,
    io: Io,
    /// This member's identity word (the `client_id` of every call).
    client_id: ClientId,
    /// The holder's volume ordinal (the frame's `volume`).
    volume: u16,
    link: Link,
    sink: RecallSink,
    lease: SelfLease,
    cache: Cache,
    /// The channel posture: false after a failed round until one
    /// completes (fail-closed).
    channel_ok: std.atomic.Value(bool) = .init(true),
    /// Bumped by every re-point; a session dialed under an older
    /// generation drops before its next call (the binding's business).
    endpoint_gen: std.atomic.Value(u64) = .init(1),
    /// Single-flight per object.
    fetch_lock: sync.Mutex = .init,
    fetching: std.AutoHashMapUnmanaged(Object, void) = .empty,
    fetch_done: sync.Notify = .init,
    /// The last holder redirect the wire named (`NotHolder` / `HolderDead`).
    last_redirect_holder: std.atomic.Value(u32) = .init(0),
    /// The dentry window's byte bound (`dirWindowBytes`; a test lowers it).
    dir_window_bytes: u64,

    pub fn init(gpa: Allocator, io: Io, client_id: ClientId, volume: u16, link: Link, sink: RecallSink, lease: SelfLease, budget_bytes: u64) Reader {
        return .{ .gpa = gpa, .io = io, .client_id = client_id, .volume = volume, .link = link, .sink = sink, .lease = lease, .cache = Cache.init(gpa, budget_bytes), .dir_window_bytes = dirWindowBytes(budget_bytes) };
    }

    pub fn deinit(self: *Reader) void {
        self.cache.deinit();
        self.fetching.deinit(self.gpa);
    }

    /// The R5 component: floor 0, weight 1, the shed drops everything.
    pub fn budgetComponent(self: *Reader) mem_budget.Component {
        return mem_budget.Component.init("dlm_token_records_bytes", 0, 1, .{ .gauge = &stats.dlm_token_cached_bytes }, .{ .ctx = self, .call = &shedFn });
    }

    fn shedFn(ctx: *anyopaque, _: u64) void {
        const self: *Reader = @ptrCast(@alignCast(ctx));
        _ = self.shed(self.io);
    }

    /// The shed (the authority's, the lease gate's): every entry dropped
    /// WITHOUT a release, each one's keys purged at the drop.
    pub fn shed(self: *Reader, io: Io) u32 {
        var dropped: std.ArrayList(*Entry) = .empty;
        defer dropped.deinit(self.gpa);
        const n = self.cache.shedAll(io, &dropped);
        self.purgeDropped(io, dropped.items);
        return n;
    }

    // ---- the serve ----

    /// `serve(object, wants)`: the gate, the hit, else the single-flight
    /// fetch; `Serve.entry == null` = gone at its holder.
    pub fn serve(self: *Reader, io: Io, object: Object, wants: Wants) Error!Serve {
        if (!self.lease.live(self.lease.ctx, clock.monotonicNs())) {
            stats.dlm_token_serve_refusals.inc();
            _ = self.shed(io);
            return error.MembershipPending;
        }
        // A cached token serves whatever the channel's posture (charter
        // §5.5: "0 RPCs and 0 device reads while cached"): the recall is a
        // PUSH on this reader's own listener, not a round on this channel,
        // and a holder that cannot reach this reader proceeds only on its
        // membership lease's expiry — the gate above. The fail-closed
        // channel gates FETCHES: a holder whose last round failed grants
        // nothing new until a round completes (0002 row 355).
        if (self.cache.probe(io, object, wants)) |e| {
            stats.dlm_token_hits.inc();
            return .{ .entry = e, .cache = &self.cache };
        }
        // A miss that finds a resident entry names what it lacked (the
        // fetch path, never the hit's).
        if (self.cache.describe(io, object)) |d| log.debug("tokens: object {d} missed the cache (entry {t}, dentries {}, wanted dentries {}, channel ok {})", .{ object, d.state, d.has_dir, wants.dentries, self.channel_ok.load(.acquire) });
        if (!self.channel_ok.load(.acquire)) {
            stats.dlm_token_serve_refusals.inc();
            return error.TokenChannelStale;
        }
        while (true) {
            if (self.cache.probe(io, object, wants)) |e| {
                stats.dlm_token_hits.inc();
                return .{ .entry = e, .cache = &self.cache };
            }
            // Single-flight: the loser registers on `fetch_done` BEFORE it
            // re-checks the winner (the register-then-check law).
            var n = self.fetch_done.notified();
            const pre = n.enable(io);
            const winner = blk: {
                self.fetch_lock.lockUncancelable(io);
                defer self.fetch_lock.unlock(io);
                const gop = self.fetching.getOrPut(self.gpa, object) catch return error.OutOfMemory;
                break :blk !gop.found_existing;
            };
            if (!winner) {
                stats.dlm_token_fetch_cohort_waits.inc();
                if (!pre) n.waitUncancelable(io) else n.cancel(io);
                continue;
            }
            n.cancel(io);
            defer {
                self.fetch_lock.lockUncancelable(io);
                _ = self.fetching.remove(object);
                self.fetch_lock.unlock(io);
                self.fetch_done.notifyWaiters(io);
            }
            while (true) {
                switch (try self.fetchOnce(io, object, wants)) {
                    .installed => |e| return .{ .entry = e, .cache = &self.cache },
                    .gone => return .{ .entry = null, .cache = &self.cache },
                    .recalled_mid_fetch => {
                        stats.dlm_token_fetch_retries.inc();
                        continue;
                    },
                }
            }
        }
    }

    const FetchOutcome = union(enum) { installed: *Entry, gone, recalled_mid_fetch };

    /// G1–G4: `gen0`, the page loop (xattr pages first by `xattr_after`, an
    /// empty incomplete page refused never spun on, then dentry pages by
    /// cookie until complete), the install under `gen0`.
    fn fetchOnce(self: *Reader, io: Io, object: Object, wants: Wants) Error!FetchOutcome {
        const gen0 = self.cache.revokeGen(io, object);
        const t0 = clock.monotonicNs();
        const e = self.gpa.create(Entry) catch return error.OutOfMemory;
        e.* = .{ .arena = std.heap.ArenaAllocator.init(self.gpa), .object = object, .attrs = .{}, .xattrs = &.{}, .xattrs_complete = false, .dir = null, .bytes = table.TOKEN_ENTRY_BYTES };
        var consumed = false;
        defer if (!consumed) e.destroy();
        const arena = e.arena.allocator();
        var xattrs: std.ArrayList(Xattr) = .empty;
        var dir: std.ArrayList(DirRecord) = .empty;
        var xattr_after: []const u8 = "";
        var records_done = false;
        var dir_after: u64 = wants.after;
        var dir_done = !wants.dentries;
        var dir_complete = true;
        var map: std.ArrayList(record.MapRecordView) = .empty;
        var map_after: u64 = wants.after;
        var map_done = !wants.map;
        var map_complete = true;
        var first = true;
        while (!(records_done and dir_done and map_done)) {
            var want_bits: u8 = 0;
            if (!records_done) want_bits |= verbs.WANT_RECORDS;
            if (wants.dentries) want_bits |= verbs.WANT_DENTRIES;
            if (wants.map) want_bits |= verbs.WANT_MAP;
            const after = if (wants.map) map_after else dir_after;
            const rep = self.link.call(self.link.ctx, .{ .token_grant = .{ .head = .{ .volume = self.volume, .wants = want_bits, .mode = 0, .xattr_after_len = @intCast(xattr_after.len), .client_id = self.client_id, .object = object, .after = after }, .bytes = xattr_after } }) catch |err| {
                self.noteRoundFailed();
                return switch (err) {
                    error.StaleHolderView => error.StaleHolderView,
                    error.HolderUnreachable => error.HolderUnreachable,
                    error.MembershipPending, error.Deferred => error.MembershipPending,
                    error.OutOfMemory => error.OutOfMemory,
                    else => error.Wire,
                };
            };
            const page = rep.token_grant;
            switch (@as(verbs.GrantPageKind, @fromBackingInt(page.head.kind))) {
                .granted => {},
                .not_holder => {
                    self.last_redirect_holder.store(page.head.holder, .release);
                    return error.StaleHolderView;
                },
                .holder_dead => {
                    self.last_redirect_holder.store(page.head.holder, .release);
                    return error.HolderUnreachable;
                },
                .gone => {
                    self.noteRound();
                    return .gone;
                },
            }
            self.noteRound();
            stats.dlm_token_grants.inc();
            if (first) {
                e.attrs = record.InodeValue.decode(&std.mem.toBytes(page.head.attrs)) catch return error.Wire;
                first = false;
            }
            var carried: usize = 0;
            var xs = page.xattrs();
            while (xs.next()) |x| {
                const name = arena.dupe(u8, x.name) catch return error.OutOfMemory;
                const value = arena.dupe(u8, x.value) catch return error.OutOfMemory;
                xattrs.append(arena, .{ .name = name, .value = value }) catch return error.OutOfMemory;
                e.bytes += name.len + value.len;
                xattr_after = name;
                carried += 1;
            }
            var ds = page.dir();
            var cut = false;
            while (ds.next()) |d| {
                // The window's bound (spec Q13): a record that would carry
                // the entry past it ends the window here — at least one
                // record per window, so a walk always advances; the rest
                // of the set pages in on demand from the window's end.
                if (dir.items.len > 0 and e.bytes + table.DIR_RECORD_BYTES + d.name.len > self.dir_window_bytes) {
                    cut = true;
                    break;
                }
                const name = arena.dupe(u8, d.name) catch return error.OutOfMemory;
                dir.append(arena, .{ .cookie = d.head.cookie, .child_ino = d.head.child_ino, .file_type = d.head.file_type, .name = name }) catch return error.OutOfMemory;
                e.bytes += table.DIR_RECORD_BYTES + name.len;
                dir_after = d.head.cookie;
                carried += 1;
            }
            var ms = page.map();
            var map_cut = false;
            while (ms.next()) |m| {
                // A later page's floor is a record this window carries.
                if (map.items.len > 0 and m.head.index <= map.items[map.items.len - 1].index) continue;
                // The window's bound, as the dentries' (at least one
                // record per window; the rest pages in from its end).
                if (map.items.len > 0 and e.bytes + table.MAP_RECORD_BYTES + m.value.len > self.dir_window_bytes) {
                    map_cut = true;
                    break;
                }
                const value = arena.dupe(u8, m.value) catch return error.OutOfMemory;
                map.append(arena, .{ .index = m.head.index, .len = m.head.len, .value = value }) catch return error.OutOfMemory;
                e.bytes += table.MAP_RECORD_BYTES + value.len;
                carried += 1;
            }
            records_done = page.head.flags & verbs.PAGE_XATTRS_COMPLETE != 0;
            if (page.head.flags & verbs.PAGE_HAS_DIR != 0) dir_done = page.head.flags & verbs.PAGE_DIR_COMPLETE != 0;
            if (page.head.flags & verbs.PAGE_HAS_MAP != 0) {
                map_done = page.head.flags & verbs.PAGE_MAP_COMPLETE != 0;
                if (!map_done and map.items.len > 0) map_after = @as(u64, map.items[map.items.len - 1].index) + 1;
            }
            if (cut) {
                dir_done = true;
                dir_complete = false;
                stats.dlm_token_dir_windows.inc();
            }
            if (map_cut) {
                map_done = true;
                map_complete = false;
                stats.dlm_token_map_windows.inc();
            }
            if (e.bytes > self.cache.budget_bytes) {
                stats.dlm_token_oversize_refusals.inc();
                return error.Oversize;
            }
            // An EMPTY incomplete page: never spun on.
            if (carried == 0 and !(records_done and dir_done and map_done)) return error.Wire;
        }
        stats.dlm_token_grant_rtt_ns.record(clock.monotonicNs() -| t0);
        e.xattrs = xattrs.items;
        e.xattrs_complete = true;
        if (wants.dentries) {
            e.dir = dir.items;
            e.dir_from = wants.after;
            e.dir_complete = dir_complete;
        }
        if (wants.map) {
            e.map = map.items;
            e.map_from = wants.after;
            e.map_complete = map_complete;
        }
        // Room by voluntary releases, drained + purged THEN released.
        var retired: std.ArrayList(Object) = .empty;
        defer retired.deinit(self.gpa);
        var retired_kept: std.ArrayList(?*Entry) = .empty;
        defer {
            for (retired_kept.items) |k| if (k) |x| self.cache.unpin(io, x);
            retired_kept.deinit(self.gpa);
        }
        var dropped: std.ArrayList(*Entry) = .empty;
        defer dropped.deinit(self.gpa);
        const room = self.cache.evictToBudget(io, e.bytes, &retired, &retired_kept, &dropped);
        self.purgeDropped(io, dropped.items);
        room catch return error.OutOfMemory;
        if (retired.items.len != 0) self.releaseRetired(retired.items, retired_kept.items);
        consumed = true;
        const outcome = self.cache.install(io, e, gen0);
        self.purgeDisplaced(io);
        return switch (outcome) {
            .installed => |served| .{ .installed = served },
            .recalled_mid_fetch => .recalled_mid_fetch,
            .oversize => error.Oversize,
        };
    }

    /// The map-bearing entries installs displaced leave as drops do: their
    /// window's keys purged (THE PURGE LAW). One that cannot be listed now
    /// stays counted in flight (a recall meanwhile censuses) for the next.
    fn purgeDisplaced(self: *Reader, io: Io) void {
        var displaced: std.ArrayList(*Entry) = .empty;
        defer displaced.deinit(self.gpa);
        self.cache.takeDisplaced(io, &displaced) catch return;
        self.purgeDropped(io, displaced.items);
    }

    fn noteRound(self: *Reader) void {
        stats.dlm_token_channel_rounds.inc();
        self.channel_ok.store(true, .release);
    }

    fn noteRoundFailed(self: *Reader) void {
        self.channel_ok.store(false, .release);
    }

    pub fn channelOk(self: *const Reader) bool {
        return self.channel_ok.load(.acquire);
    }

    // ---- the recall (push) ----

    /// The pushed frame: per object the generation bump THEN the removal
    /// (`revokeKeeping`), then the purge — the kernel told, the scoped
    /// block-key purge run; the reply is the ack.
    pub fn handleRecall(self: *Reader, io: Io, objects: []align(1) const Object) Allocator.Error!void {
        const kept = try self.revokeKeeping(io, objects);
        defer self.releaseKept(io, kept);
        stats.dlm_token_recalls_received.add(@intCast(objects.len));
        self.noteRound();
        self.purge(objects, kept);
        stats.dlm_token_recalls_acked.add(@intCast(objects.len));
    }

    /// The revoke that keeps each revoked entry pinned for the purge
    /// (`Cache.revoke`); the caller hands the result to `releaseKept`.
    fn revokeKeeping(self: *Reader, io: Io, objects: []align(1) const Object) Allocator.Error![]?*Entry {
        const kept = try self.gpa.alloc(?*Entry, objects.len);
        errdefer self.gpa.free(kept);
        _ = self.cache.revoke(io, objects, kept) catch |e| {
            for (kept) |k| if (k) |x| self.cache.unpin(io, x);
            return e;
        };
        return kept;
    }

    fn releaseKept(self: *Reader, io: Io, kept: []?*Entry) void {
        for (kept) |k| if (k) |x| self.cache.unpin(io, x);
        self.gpa.free(kept);
    }

    /// The scoped purge (spec §1.7 step 2): per recalled object the
    /// kernel's view invalidated, the clean layout entry discarded with its
    /// keys — the sink reads them off the revoked entry (`kept[i]`) —
    /// DIRTY kept, a held stripe skipped. An object with no entry owes the
    /// tiers nothing (THE PURGE LAW); one whose entry names its keys
    /// elsewhere, one with no entry while a drop's purge is in flight (it
    /// may be that object's), or a call with no `kept` (the revoke could
    /// keep nothing, so what was held is unknown), falls back to the census
    /// ONCE per call.
    fn purge(self: *Reader, objects: []align(1) const Object, kept: ?[]const ?*Entry) void {
        if (kept) |ks| assert(ks.len == objects.len);
        var census_done = false;
        for (objects, 0..) |o, i| {
            self.sink.noteRecalled(self.sink.ctx, o);
            const k: ?*const Entry = if (kept) |ks| ks[i] else null;
            switch (self.sink.discardLayout(self.sink.ctx, o, k)) {
                .discarded => |n| {
                    stats.dlm_token_recall_purged_keys.add(n);
                    stats.dlm_token_recall_scoped_purges.inc();
                },
                .dirty_kept => stats.dlm_token_recall_dirty_kept.inc(),
                .held_skipped => stats.dlm_token_recall_discard_skipped.inc(),
                .no_entry => if (kept == null or self.cache.dropsInFlight(self.io) != 0) {
                    self.censusOnce(&census_done);
                } else stats.dlm_token_recall_unheld.inc(),
                .unmapped => self.censusOnce(&census_done),
            }
        }
    }

    fn censusOnce(self: *Reader, done: *bool) void {
        if (done.*) return;
        done.* = true;
        stats.dlm_token_recall_census_purges.inc();
        self.sink.purgeCensus(self.sink.ctx);
    }

    /// The keys of entries that left WITHOUT a release (room-making's
    /// kernel-pinned drop, the shed), purged at the drop (THE PURGE LAW),
    /// then unpinned. No kernel invalidation: nothing changed, and the
    /// holder still lists this reader for the change that will.
    fn purgeDropped(self: *Reader, io: Io, dropped: []const *Entry) void {
        if (dropped.len == 0) return;
        var census_done = false;
        for (dropped) |e| {
            switch (self.sink.discardLayout(self.sink.ctx, e.object, e)) {
                .discarded => |n| stats.dlm_token_drop_purged_keys.add(n),
                .unmapped => self.censusOnce(&census_done),
                .dirty_kept, .held_skipped, .no_entry => {},
            }
        }
        self.cache.dropsPurged(io, @intCast(dropped.len));
        for (dropped) |e| self.cache.unpin(io, e);
    }

    // ---- release, stop, re-point ----

    /// Voluntary: purged first, then `Release` (best effort — a lost
    /// release costs the holder one needless recall).
    fn releaseRetired(self: *Reader, objects: []const Object, kept: []const ?*Entry) void {
        self.purge(objects, kept);
        var off: usize = 0;
        while (off < objects.len) {
            const n = @min(objects.len - off, std.math.maxInt(u16));
            _ = self.link.call(self.link.ctx, .{ .token_release = .{ .head = .{ .volume = self.volume, .n = @intCast(n), .reserved = 0, .client_id = self.client_id }, .items = objects[off .. off + n] } }) catch {};
            off += n;
        }
        stats.dlm_token_reader_releases.add(@intCast(objects.len));
    }

    /// Evict ONE object as room-making would (a kernel-pinned entry drops
    /// without a release, a cold one is released): the
    /// `SQUEEZEFS_TEST_TOKEN_EVICT_AFTER_SERVE` seam's step.
    pub fn evict(self: *Reader, io: Io, object: Object) void {
        var retired: std.ArrayList(Object) = .empty;
        defer retired.deinit(self.gpa);
        var kept: std.ArrayList(?*Entry) = .empty;
        defer {
            for (kept.items) |k| if (k) |x| self.cache.unpin(io, x);
            kept.deinit(self.gpa);
        }
        var dropped: std.ArrayList(*Entry) = .empty;
        defer dropped.deinit(self.gpa);
        const room = self.cache.evictOne(io, object, &retired, &kept, &dropped);
        self.purgeDropped(io, dropped.items);
        room catch return;
        if (retired.items.len != 0) self.releaseRetired(retired.items, kept.items);
    }

    /// Release ONE object the caller is done with (a test's voluntary
    /// release; the eviction path is `evictToBudget`'s).
    pub fn release(self: *Reader, io: Io, object: Object) void {
        const objs = [_]Object{object};
        const kept = self.revokeKeeping(io, &objs) catch return;
        defer self.releaseKept(io, kept);
        self.releaseRetired(&objs, kept);
    }

    /// The clean leave: every held token released when the channel is ok.
    pub fn stop(self: *Reader, io: Io) void {
        var held: std.ArrayList(Object) = .empty;
        defer held.deinit(self.gpa);
        {
            self.cache.lock.lockUncancelable(io);
            defer self.cache.lock.unlock(io);
            var it = self.cache.entries.keyIterator();
            while (it.next()) |k| held.append(self.gpa, k.*) catch break;
        }
        if (held.items.len == 0 or !self.channel_ok.load(.acquire)) return;
        const kept = self.revokeKeeping(io, held.items) catch return;
        defer self.releaseKept(io, kept);
        self.releaseRetired(held.items, kept);
    }

    /// A holder that MOVED its listener: the link re-pointed in place, the
    /// endpoint generation bumped, every cached token dropped with its purge
    /// ("a holder that moved may have re-granted"), the channel fail-closed
    /// until a round completes — the first round is the probe on the new
    /// session (`ping`), so a resolve after the move parks on no dial.
    pub fn repoint(self: *Reader, io: Io, link: Link) void {
        self.link = link;
        _ = self.endpoint_gen.fetchAdd(1, .acq_rel);
        var held: std.ArrayList(Object) = .empty;
        defer held.deinit(self.gpa);
        {
            self.cache.lock.lockUncancelable(io);
            defer self.cache.lock.unlock(io);
            var it = self.cache.entries.keyIterator();
            while (it.next()) |k| held.append(self.gpa, k.*) catch break;
        }
        // A revoke that could not allocate kept nothing: the purge runs
        // over no entries — the census, the fail-safe direction.
        const kept = self.revokeKeeping(io, held.items) catch null;
        defer if (kept) |k| self.releaseKept(io, k);
        self.purge(held.items, kept);
        self.channel_ok.store(false, .release);
        stats.dlm_token_holder_repoints.inc();
        if (self.link.call(self.link.ctx, .{ .ping = .{} })) |_| self.noteRound() else |_| self.noteRoundFailed();
    }

    /// The in-op RE-PROBE of a fail-closed channel (the divert's, ONCE per
    /// serve that needs a fetch): the link re-dialled through the
    /// resolver, the probe round the `ping`. A round that completes proves
    /// a LIVE holder this reader lost a round with — it may have
    /// re-granted (or restarted on a fixed port), so every cached token is
    /// dropped as at a move and the channel re-opens; a round that fails
    /// proves nothing moved: the cache stays (its tokens serve hits until
    /// the membership gate) and the channel stays closed. Returns whether
    /// the channel re-opened.
    pub fn redial(self: *Reader, io: Io, link: Link) bool {
        self.link = link;
        _ = self.endpoint_gen.fetchAdd(1, .acq_rel);
        if (self.link.call(self.link.ctx, .{ .ping = .{} })) |_| {} else |_| {
            self.noteRoundFailed();
            return false;
        }
        var held: std.ArrayList(Object) = .empty;
        defer held.deinit(self.gpa);
        {
            self.cache.lock.lockUncancelable(io);
            defer self.cache.lock.unlock(io);
            var it = self.cache.entries.keyIterator();
            while (it.next()) |k| held.append(self.gpa, k.*) catch break;
        }
        // A revoke that could not allocate kept nothing: the purge runs
        // over no entries — the census, the fail-safe direction.
        const kept = self.revokeKeeping(io, held.items) catch null;
        defer if (kept) |k| self.releaseKept(io, k);
        self.purge(held.items, kept);
        self.noteRound();
        return true;
    }

    pub fn endpointGen(self: *const Reader) u64 {
        return self.endpoint_gen.load(.acquire);
    }
};

// ---------------------------------------------------------------------------
// Tests: the pure laws (the protocol contracts ride `tokens.zig`).
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the cache: install under gen0 is refused by a bump the check sees and found by a bump after it; a dentry-bearing entry is never displaced by a bare one; pins keep a revoked entry alive until the last release" {
    const io = testing.io;
    var c = Cache.init(testing.allocator, 1 << 20);
    defer c.deinit();
    const mk = struct {
        fn mk(gpa: Allocator, object: Object, with_dir: bool) !*Entry {
            const e = try gpa.create(Entry);
            e.* = .{ .arena = std.heap.ArenaAllocator.init(gpa), .object = object, .attrs = .{ .mode = 0o100644 }, .xattrs = &.{}, .xattrs_complete = true, .dir = if (with_dir) &.{} else null, .bytes = table.TOKEN_ENTRY_BYTES };
            return e;
        }
    }.mk;
    const gpa = testing.allocator;
    // A bump the check sees: aborted.
    const gen0 = c.revokeGen(io, 7);
    var none: [1]?*Entry = undefined;
    try testing.expectEqual(@as(u32, 0), try c.revoke(io, &.{7}, &none));
    try testing.expectEqual(@as(?*Entry, null), none[0]);
    const e1 = try mk(gpa, 7, false);
    try testing.expect(c.install(io, e1, gen0) == .recalled_mid_fetch);
    // Installed under the current generation; a later bump finds it.
    const gen1 = c.revokeGen(io, 7);
    const e2 = try mk(gpa, 7, true);
    try testing.expectEqual(e2, c.install(io, e2, gen1).installed);
    try testing.expectEqual(@as(usize, 1), c.count(io));
    try testing.expect(c.probe(io, 7, .{ .dentries = true }) != null);
    c.unpin(io, e2);
    // A bare entry never displaces the dentry-bearing one: the richer one
    // is served (pinned) in its place.
    const e3 = try mk(gpa, 7, false);
    try testing.expectEqual(e2, c.install(io, e3, gen1).installed);
    c.unpin(io, e2);
    try testing.expect(c.probe(io, 7, .{ .dentries = true }) != null);
    // Revoked while pinned (twice): alive until the last unpin — the
    // revoke's own pin (the purge's read of the carried records) included.
    c.unpin(io, e2);
    var kept: [1]?*Entry = undefined;
    try testing.expectEqual(@as(u32, 1), try c.revoke(io, &.{7}, &kept));
    try testing.expectEqual(e2, kept[0].?);
    try testing.expectEqual(@as(usize, 0), c.count(io));
    try testing.expectEqual(State.revoked, e2.state);
    c.unpin(io, e2);
    c.unpin(io, kept[0].?);
}

test "the budget: ONE oversize entry is refused loud; eviction gives the used bit a second chance; the shed drops everything, each entry pinned for its purge" {
    const io = testing.io;
    const gpa = testing.allocator;
    var c = Cache.init(gpa, 3 * table.TOKEN_ENTRY_BYTES);
    defer c.deinit();
    // No kernel caches these answers: room-making releases (§AG.2's pin is
    // the next row's).
    c.kernel_pin_ns = 0;
    const mk = struct {
        fn mk(a: Allocator, object: Object, bytes: u64) !*Entry {
            const e = try a.create(Entry);
            e.* = .{ .arena = std.heap.ArenaAllocator.init(a), .object = object, .attrs = .{}, .xattrs = &.{}, .xattrs_complete = true, .dir = null, .bytes = bytes };
            return e;
        }
    }.mk;
    const big = try mk(gpa, 1, 4 * table.TOKEN_ENTRY_BYTES);
    log.testing.expectErrorLogs(1);
    try testing.expect(c.install(io, big, 0) == .oversize);
    try log.testing.expectAllFired();
    var es: [3]*Entry = undefined;
    for (&es, 1..) |*e, i| {
        e.* = try mk(gpa, @intCast(i), table.TOKEN_ENTRY_BYTES);
        try testing.expectEqual(e.*, c.install(io, e.*, 0).installed);
        c.unpin(io, e.*);
    }
    // Object 2 was used (probed): the first pass clears its bit and takes
    // another; it survives.
    _ = c.probe(io, 2, .{}).?;
    c.unpin(io, es[1]);
    var out: std.ArrayList(Object) = .empty;
    defer out.deinit(gpa);
    var kept: std.ArrayList(?*Entry) = .empty;
    defer kept.deinit(gpa);
    var dropped: std.ArrayList(*Entry) = .empty;
    defer dropped.deinit(gpa);
    try c.evictToBudget(io, table.TOKEN_ENTRY_BYTES, &out, &kept, &dropped);
    try testing.expectEqual(@as(usize, 0), dropped.items.len);
    try testing.expectEqual(@as(usize, 1), out.items.len);
    try testing.expect(out.items[0] != 2);
    // The victim's entry stays pinned (REVOKED) for the purge; the unpin
    // destroys it.
    try testing.expectEqual(@as(usize, 1), kept.items.len);
    try testing.expectEqual(out.items[0], kept.items[0].?.object);
    try testing.expectEqual(State.revoked, kept.items[0].?.state);
    c.unpin(io, kept.items[0].?);
    try testing.expectEqual(@as(usize, 2), c.count(io));
    const before = stats.dlm_token_sheds.load();
    try testing.expectEqual(@as(u32, 2), c.shedAll(io, &dropped));
    try testing.expectEqual(before + 2, stats.dlm_token_sheds.load());
    try testing.expectEqual(@as(u64, 0), c.bytesHeld(io));
    try testing.expectEqual(@as(usize, 2), dropped.items.len);
    for (dropped.items) |e| {
        try testing.expectEqual(State.revoked, e.state);
        c.unpin(io, e);
    }
}

test "the kernel pin (decision 0003 §AG.2): room-making drops an entry the kernel may still cache an answer from WITHOUT a release (counted), and retires one past the pin for its release" {
    const io = testing.io;
    const gpa = testing.allocator;
    var c = Cache.init(gpa, 2 * table.TOKEN_ENTRY_BYTES);
    defer c.deinit();
    for ([_]Object{ 1, 2 }) |o| {
        const e = try gpa.create(Entry);
        e.* = .{ .arena = std.heap.ArenaAllocator.init(gpa), .object = o, .attrs = .{}, .xattrs = &.{}, .xattrs_complete = true, .dir = null, .bytes = table.TOKEN_ENTRY_BYTES };
        c.kernel_pin_ns = if (o == 1) std.math.maxInt(u64) else 0;
        c.unpin(io, c.install(io, e, 0).installed);
    }
    var out: std.ArrayList(Object) = .empty;
    defer out.deinit(gpa);
    var kept: std.ArrayList(?*Entry) = .empty;
    defer kept.deinit(gpa);
    var dropped: std.ArrayList(*Entry) = .empty;
    defer dropped.deinit(gpa);
    const drops0 = stats.dlm_token_kernel_pinned_drops.load();
    // Room for both: object 1 (served under the pin) drops — pinned for
    // the caller's purge, never released —, object 2 (past its pin) is
    // retired for the caller's release.
    try c.evictToBudget(io, 2 * table.TOKEN_ENTRY_BYTES, &out, &kept, &dropped);
    try testing.expectEqual(drops0 + 1, stats.dlm_token_kernel_pinned_drops.load());
    try testing.expectEqualSlices(Object, &.{2}, out.items);
    try testing.expectEqual(@as(usize, 1), dropped.items.len);
    try testing.expectEqual(@as(Object, 1), dropped.items[0].object);
    c.unpin(io, kept.items[0].?);
    c.unpin(io, dropped.items[0]);
    try testing.expectEqual(@as(usize, 0), c.count(io));
}

test "Entry: pageAfter is the strict cookie successor; lookupName finds by full name" {
    const recs = [_]DirRecord{ .{ .cookie = 10, .child_ino = 1, .file_type = 8, .name = "a" }, .{ .cookie = 20, .child_ino = 2, .file_type = 8, .name = "b" }, .{ .cookie = 30, .child_ino = 3, .file_type = 4, .name = "c" } };
    var d = recs;
    var e: Entry = .{ .arena = std.heap.ArenaAllocator.init(testing.allocator), .object = 1, .attrs = .{}, .xattrs = &.{}, .xattrs_complete = true, .dir = &d, .bytes = 0 };
    defer e.arena.deinit();
    try testing.expectEqual(@as(usize, 3), e.pageAfter(0).len);
    try testing.expectEqual(@as(usize, 2), e.pageAfter(10).len);
    try testing.expectEqual(@as(usize, 1), e.pageAfter(25).len);
    try testing.expectEqual(@as(usize, 0), e.pageAfter(30).len);
    try testing.expectEqual(@as(u64, 2), e.lookupName("b").?.child_ino);
    try testing.expectEqual(@as(?DirRecord, null), e.lookupName("zz"));
}
