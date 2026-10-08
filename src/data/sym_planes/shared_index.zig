//! The shared-block index (charter §5.5 "Pack blocks are scoped per
//! `(writer, slot, data volume)` so a block's references live in ONE slot
//! tree and `refcount(block)` is one range probe; a block referenced from
//! two slots arises only through clone/reflink → the clone protocol …
//! `ShareBlock` at the index home (the allocation holder) … a terminal
//! free of a SHARED block is never decided locally; a surviving sole owner
//! stays SHARED (the un-share protocol needs a share-in-flight witness)";
//! `docs/porting/sym_data_planes.md` §5.1–§5.2, §5.3 step 4, §5.5; decision
//! 0003 §E "Un-share witness"; 0002 row 239). PORTABLE.
//!
//! Home: the tree 0 of the volume the data volume's allocation holder is
//! HOMED on (else volume 0), served by THAT volume's manager — kind-9
//! records keyed by the 29-B ref key (one range probe per block like kind
//! 6), value `{version 1, reserved}`, written with a PARKING pre-admission
//! taken BEFORE the verb mutex (a park under the verb mutex deadlocks with
//! the checkpoint task's grant verbs; a `Try` fails the clone on a busy
//! ring 0). `ShareBlock` writes BOTH inos' entries (so C16 checks both
//! sides) and its served form is SCREENED: each named reference must
//! exist AND carry SHARED on its own volume — "MarkShared precedes
//! ShareBlock" — else the whole frame is `Rejected`.
//!
//! `ReleaseShared` deletes the releaser's ONE entry (the legal same-`off`
//! nested clone+clip class gives one ino two references to one block and
//! the other must stand), GCs every entry whose ino no longer holds a
//! reference (the "cloner died after step 2" window), and decides:
//! `NotShared` (the index never named it ⇒ the ordinary local verdict),
//! `Held{remaining}` (the block stays allocated), `Freed` (the releaser
//! runs the terminal free). A home that cannot decide keeps the reference
//! and the mark — `shared_release_failures` must-stay-0.
//!
//! The un-share WITNESS: a per-block share intent registered BEFORE the
//! clone's step 1 and retired at step 3's publish ack, so the home's
//! `Held{1}` verdict can issue `UnshareBlock{b, owner, share_seq}` to the
//! owner's holder only when `in_flight(b) == 0`; a `MarkShared` that lands
//! first bumps `share_seq` and the stale clear is refused at the re-check.

const std = @import("std");
const Atomic = std.atomic.Value;
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const counters = @import("../../core/counters.zig");
const log = @import("../../core/log.zig");
const jc = @import("../../meta/journal_core.zig");
const key = @import("../../meta/key.zig");
const record = @import("../../meta/record.zig");
const tree = @import("../../meta/tree.zig");
const block_refs = @import("../block_refs.zig");
const block_alloc = @import("../block_alloc.zig");
const records = @import("records.zig");
const custody = @import("custody.zig");

// ---------------------------------------------------------------------------
// Constants (every one with its WHY).
// ---------------------------------------------------------------------------

/// A clone storm on one block re-sizes the release's pre-admission at
/// most this often; past it a loud refusal, never a wait.
pub const RELEASE_RESIZE_ATTEMPTS: u32 = 8;
/// `ShareBlock` refs per call: the control entry's cap at ≈ 50 B per
/// record (29 B key + 4 B value + the 16 B frame).
pub const SHARE_BLOCK_CHUNK: usize = 512;
/// Share intents in flight at one home: a clone touches at most one block
/// per step and the cloner population is the appender ceiling.
pub const INTENTS_MAX: usize = 1024;

// ---------------------------------------------------------------------------
// Stats (§7.2).
// ---------------------------------------------------------------------------

pub const stats = struct {
    pub var share_block_calls: counters.ShardedCounter = .{};
    pub var share_block_entries: counters.ShardedCounter = .{};
    pub var share_block_rejected: counters.ShardedCounter = .{};
    pub var release_shared_calls: counters.ShardedCounter = .{};
    pub var release_shared_gc: counters.ShardedCounter = .{};
    pub var release_shared_resizes: counters.ShardedCounter = .{};
    pub var shared_index_population_probes: counters.ShardedCounter = .{};
    pub var share_intents_registered: counters.ShardedCounter = .{};
    pub var share_intents_retired: counters.ShardedCounter = .{};
    pub var unshare_issued: counters.ShardedCounter = .{};
    pub var unshare_withheld_in_flight: counters.ShardedCounter = .{};
    pub var unshare_stale_refused: counters.ShardedCounter = .{};
    pub var fsck_shared_index_drift: counters.ShardedCounter = .{};
    /// Levels.
    pub var shared_blocks: counters.Gauge = .{};
    pub var share_intents_in_flight: counters.Gauge = .{};
};

// ---------------------------------------------------------------------------
// §5.1 Home resolution and the probe.
// ---------------------------------------------------------------------------

/// The volume the data volume's allocation holder is HOMED on, else 0.
pub fn indexHomeVolumeFor(holder_home_vol: ?u16) u16 {
    return holder_home_vol orelse 0;
}

/// A reference naming a block: `(owner_ino, block_index)`.
pub const Ref = struct { owner_ino: u64, block_index: u32 };

/// `block_ref_probe_flags`: ONE range probe of the referencing ino's slot
/// tree — `{count, shared = OR of the records' SHARED bits}`; both halves
/// decoded for every record (a malformed accounting record is loud
/// corruption, never silently skipped or counted).
pub fn refProbe(store: block_refs.RefStore, gpa: Allocator, vol_tag: u64, block_idx: u64) block_refs.Error!custody.RefProbe {
    block_refs.stats.block_ref_probes.inc();
    var out: std.ArrayList(tree.KeyValue) = .empty;
    defer tree.deinitKeyValues(gpa, &out);
    const r = key.blockRange(vol_tag, block_idx);
    try store.vtable.range(store.ctx, gpa, r.lo.slice(), r.hi.slice(), std.math.maxInt(usize), &out);
    var count: u32 = 0;
    var shared = false;
    for (out.items) |kv| {
        if (!key.lessThan(kv.key, r.hi.slice())) continue;
        _ = key.BlockRefKey.decode(kv.key) catch return error.Corrupt;
        const v = record.BlockRefValue.decode(kv.value) catch return error.Corrupt;
        count += 1;
        shared = shared or v.shared;
    }
    return .{ .count = count, .shared = shared };
}

// ---------------------------------------------------------------------------
// The home's store seams.
// ---------------------------------------------------------------------------

pub const StoreError = error{ WindowFull, Io, OutOfMemory };

/// Kind-9 records in the home volume's tree 0: `put`/`delete` under ONE
/// control entry (PARKING pre-admission), `range` over the block prefix.
pub const IndexStore = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        put: *const fn (*anyopaque, k: []const u8, v: []const u8) StoreError!void,
        delete: *const fn (*anyopaque, k: []const u8) StoreError!void,
        /// Keys in `[lo, hi)`, ascending, copied into `out` (caller frees
        /// each key with `gpa`).
        range: *const fn (*anyopaque, Allocator, lo: []const u8, hi: []const u8, out: *std.ArrayList([]u8)) StoreError!void,
    };
};

pub const RefFlags = struct { exists: bool, shared: bool };

/// The routed probe of one reference on the OWNER's own volume (the
/// served `ShareBlock` screen) and the routed `still_referenced` arm of
/// the release's GC.
pub const RoutedRefs = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        refFlags: *const fn (*anyopaque, vol_tag: u64, block_idx: u64, ref: Ref) ?RefFlags,
        /// Does `owner_ino` still hold ANY reference to the block?
        stillReferenced: *const fn (*anyopaque, vol_tag: u64, block_idx: u64, owner_ino: u64) bool,
    };
};

// ---------------------------------------------------------------------------
// The share-intent witness (decision 0003 §E).
// ---------------------------------------------------------------------------

const Intent = struct { used: bool = false, vol_tag: u64 = 0, block_idx: u64 = 0, owner_ino: u64 = 0, seq: u64 = 0 };

// ---------------------------------------------------------------------------
// §5.3 The home.
// ---------------------------------------------------------------------------

pub const ShareVerdict = union(enum) {
    written: struct { written: u32, already: u32 },
    /// A named reference does not exist or does not carry SHARED.
    rejected,
    deferred,
    io_error,
};

pub const ReleaseVerdict = union(enum) {
    /// The index never named the block: the ordinary local verdict.
    not_shared,
    /// This owner's reference is released; `remaining` entries stand.
    held: u32,
    /// The releaser runs the terminal free.
    freed,
    /// The home could not decide: the reference and the mark are KEPT.
    undecided,
};

pub const UnshareDecision = union(enum) {
    /// `UnshareBlock{b, owner, share_seq}` may go to the owner's holder.
    issue: u64,
    /// A share of this very block is between step 1 and step 3.
    withheld_in_flight,
    /// More than one entry stands.
    not_sole,
};

pub const IndexHome = struct {
    gpa: Allocator,
    store: IndexStore,
    routed: ?RoutedRefs = null,
    lock: jc.SpinLock = .{},
    intents: [INTENTS_MAX]Intent = @splat(.{}),
    intent_seq: u64 = 0,
    /// Per-home monotone `share_seq`: bumped by every `MarkShared` the
    /// home learns of (its `ShareBlock`), checked by the un-share re-check.
    share_seq: Atomic(u64) = .init(0),

    fn put(self: *IndexHome, vol_tag: u64, block_idx: u64, ref: Ref) StoreError!void {
        const k = records.sharedIndexKey(vol_tag, block_idx, ref.owner_ino, ref.block_index);
        try self.store.vtable.put(self.store.ctx, &k, &records.encodeSharedIndexValue());
    }

    /// The block's entries (owner-sorted keys copied; caller frees).
    pub fn entries(self: *IndexHome, vol_tag: u64, block_idx: u64, out: *std.ArrayList([]u8)) StoreError!void {
        const r = records.sharedIndexBlockRange(vol_tag, block_idx);
        try self.store.vtable.range(self.store.ctx, self.gpa, r.lo.slice(), r.hi.slice(), out);
    }

    fn freeEntries(self: *IndexHome, out: *std.ArrayList([]u8)) void {
        for (out.items) |k| self.gpa.free(k);
        out.deinit(self.gpa);
    }

    /// `shared_index_population`: counted as ONE probe.
    pub fn population(self: *IndexHome, vol_tag: u64, block_idx: u64) StoreError!u32 {
        stats.shared_index_population_probes.inc();
        block_refs.stats.block_ref_probes.inc();
        var out: std.ArrayList([]u8) = .empty;
        defer self.freeEntries(&out);
        try self.entries(vol_tag, block_idx, &out);
        return @intCast(out.items.len);
    }

    fn hasEntry(self: *IndexHome, vol_tag: u64, block_idx: u64, ref: Ref) StoreError!bool {
        var out: std.ArrayList([]u8) = .empty;
        defer self.freeEntries(&out);
        try self.entries(vol_tag, block_idx, &out);
        const want = records.sharedIndexKey(vol_tag, block_idx, ref.owner_ino, ref.block_index);
        for (out.items) |k| if (std.mem.eql(u8, k, &want)) return true;
        return false;
    }

    /// `ShareBlock{vol_tag, block_idx, refs}`: BOTH inos' entries as one
    /// control entry, present ones skipped; the served form screens each
    /// reference against its owner's volume FIRST.
    pub fn shareBlock(self: *IndexHome, vol_tag: u64, block_idx: u64, refs: []const Ref, served: bool) ShareVerdict {
        stats.share_block_calls.inc();
        assert(refs.len <= SHARE_BLOCK_CHUNK);
        if (served) {
            const routed = self.routed orelse return self.rejectShare();
            for (refs) |r| {
                const f = routed.vtable.refFlags(routed.ctx, vol_tag, block_idx, r) orelse return self.rejectShare();
                if (!f.exists or !f.shared) return self.rejectShare();
            }
        }
        var written: u32 = 0;
        var already: u32 = 0;
        for (refs) |r| {
            const present = self.hasEntry(vol_tag, block_idx, r) catch |e| return mapShareStore(e);
            if (present) {
                already += 1;
                continue;
            }
            self.put(vol_tag, block_idx, r) catch |e| return mapShareStore(e);
            written += 1;
        }
        // A share landed: any un-share decided against an older seq is stale.
        _ = self.share_seq.fetchAdd(1, .acq_rel);
        stats.share_block_entries.add(written);
        if (written > 0) stats.shared_blocks.add(1);
        return .{ .written = .{ .written = written, .already = already } };
    }

    fn rejectShare(_: *IndexHome) ShareVerdict {
        stats.share_block_rejected.inc();
        return .rejected;
    }

    fn mapShareStore(e: StoreError) ShareVerdict {
        return switch (e) {
            error.WindowFull => .deferred,
            error.Io, error.OutOfMemory => .io_error,
        };
    }

    /// `ReleaseShared{vol_tag, block_idx, owner}`: the releaser's ONE
    /// entry deleted, the GC of entries whose ino no longer references
    /// (the routed arm only), then the verdict. The pre-admission is sized
    /// by the population; a racing `ShareBlock` that grew it re-sizes (≤
    /// `RELEASE_RESIZE_ATTEMPTS`) — past that, `undecided` loud.
    pub fn releaseShared(self: *IndexHome, vol_tag: u64, block_idx: u64, owner: ?Ref) ReleaseVerdict {
        stats.release_shared_calls.inc();
        var attempt: u32 = 0;
        while (attempt < RELEASE_RESIZE_ATTEMPTS) : (attempt += 1) {
            var out: std.ArrayList([]u8) = .empty;
            defer self.freeEntries(&out);
            self.entries(vol_tag, block_idx, &out) catch return .undecided;
            if (out.items.len == 0) return .not_shared;
            const sized = out.items.len;
            // The releaser's own entry.
            if (owner) |o| {
                const want = records.sharedIndexKey(vol_tag, block_idx, o.owner_ino, o.block_index);
                for (out.items) |k| if (std.mem.eql(u8, k, &want)) {
                    self.store.vtable.delete(self.store.ctx, k) catch return .undecided;
                    break;
                };
            }
            // GC: every entry whose ino no longer holds a reference.
            if (self.routed) |routed| {
                for (out.items) |k| {
                    const dk = records.decodeSharedIndexKey(k) catch return .undecided;
                    if (owner != null and dk.ownerIno() == owner.?.owner_ino and dk.blockIndex() == owner.?.block_index) continue;
                    if (!routed.vtable.stillReferenced(routed.ctx, vol_tag, block_idx, dk.ownerIno())) {
                        self.store.vtable.delete(self.store.ctx, k) catch return .undecided;
                        stats.release_shared_gc.inc();
                    }
                }
            }
            var after: std.ArrayList([]u8) = .empty;
            defer self.freeEntries(&after);
            self.entries(vol_tag, block_idx, &after) catch return .undecided;
            // A racing ShareBlock grew the population past the sized
            // admission: re-size and run again.
            if (after.items.len + 1 > sized and after.items.len >= sized) {
                stats.release_shared_resizes.inc();
                continue;
            }
            if (after.items.len == 0) {
                stats.shared_blocks.sub(1);
                return .freed;
            }
            return .{ .held = @intCast(after.items.len) };
        }
        log.err("shared index: ReleaseShared of block {d} on vol {x} could not be sized in {d} attempts (a clone storm on one block) — the reference and the mark are KEPT", .{ block_idx, vol_tag, RELEASE_RESIZE_ATTEMPTS });
        return .undecided;
    }

    // ---- the witness ----

    /// Registered BEFORE the clone's step 1; the seq retires it.
    pub fn registerIntent(self: *IndexHome, vol_tag: u64, block_idx: u64, owner_ino: u64) ?u64 {
        self.lock.lock();
        defer self.lock.unlock();
        for (&self.intents) |*i| if (!i.used) {
            self.intent_seq += 1;
            i.* = .{ .used = true, .vol_tag = vol_tag, .block_idx = block_idx, .owner_ino = owner_ino, .seq = self.intent_seq };
            stats.share_intents_registered.inc();
            stats.share_intents_in_flight.add(1);
            return i.seq;
        };
        return null;
    }

    /// Retired at step 3's publish ack (or the cloner's abort).
    pub fn retireIntent(self: *IndexHome, seq: u64) bool {
        self.lock.lock();
        defer self.lock.unlock();
        for (&self.intents) |*i| if (i.used and i.seq == seq) {
            i.used = false;
            stats.share_intents_retired.inc();
            stats.share_intents_in_flight.sub(1);
            return true;
        };
        return false;
    }

    pub fn inFlight(self: *IndexHome, vol_tag: u64, block_idx: u64) u32 {
        self.lock.lock();
        defer self.lock.unlock();
        var n: u32 = 0;
        for (self.intents) |i| if (i.used and i.vol_tag == vol_tag and i.block_idx == block_idx) {
            n += 1;
        };
        return n;
    }

    /// At a `Held{1}` verdict: issue the un-share iff no share of the
    /// block is in flight.
    pub fn decideUnshare(self: *IndexHome, vol_tag: u64, block_idx: u64, remaining: u32) UnshareDecision {
        if (remaining != 1) return .not_sole;
        if (self.inFlight(vol_tag, block_idx) != 0) {
            stats.unshare_withheld_in_flight.inc();
            return .withheld_in_flight;
        }
        stats.unshare_issued.inc();
        return .{ .issue = self.share_seq.load(.acquire) };
    }

    /// The home's re-check when the owner's holder is about to clear the
    /// bit under the 4a guard: a `MarkShared` that landed first bumped the
    /// seq ⇒ the stale clear is refused.
    pub fn confirmUnshare(self: *IndexHome, share_seq: u64) bool {
        if (self.share_seq.load(.acquire) != share_seq) {
            stats.unshare_stale_refused.inc();
            return false;
        }
        return true;
    }

    /// The allocator's `SharedFreeGate`: a terminal free of a SHARED block
    /// asks the home with the releasing owner.
    pub fn sharedFreeGate(self: *IndexHome, owner: *const Ref) block_alloc.SharedFreeGate {
        _ = owner;
        const G = struct {
            fn ask(ctx: *anyopaque, vol_tag: u64, idx: u64) block_alloc.SharedVerdict {
                const h: *IndexHome = @ptrCast(@alignCast(ctx));
                return switch (h.releaseShared(vol_tag, idx, h.releasingOwnerFor(vol_tag, idx))) {
                    .not_shared => .not_shared,
                    .held => |n| .{ .held = n },
                    .freed => .freed,
                    .undecided => .unreachable_home,
                };
            }
        };
        return .{ .ctx = self, .ask = &G.ask };
    }

    /// The releaser the gate names: the allocator's seam carries no owner,
    /// so the home releases the FIRST entry whose ino no longer references
    /// the block (the routed arm) or, unrouted, defers to the GC-less
    /// population verdict.
    fn releasingOwnerFor(self: *IndexHome, vol_tag: u64, block_idx: u64) ?Ref {
        const routed = self.routed orelse return null;
        var out: std.ArrayList([]u8) = .empty;
        defer self.freeEntries(&out);
        self.entries(vol_tag, block_idx, &out) catch return null;
        for (out.items) |k| {
            const dk = records.decodeSharedIndexKey(k) catch continue;
            if (!routed.vtable.stillReferenced(routed.ctx, vol_tag, block_idx, dk.ownerIno())) return .{ .owner_ino = dk.ownerIno(), .block_index = dk.blockIndex() };
        }
        return null;
    }
};

// ---------------------------------------------------------------------------
// §5.5 C16: shared-index drift (report-only).
// ---------------------------------------------------------------------------

pub const Drift = struct {
    flag_without_entry: u64,
    entry_without_flag: u64,
    pub fn clean(self: Drift) bool {
        return self.flag_without_entry == 0 and self.entry_without_flag == 0;
    }
};

/// Over one data volume: the SHARED-flagged references (global inos) vs
/// the home's entries, both SORTED ascending by `(block_idx, owner_ino,
/// block_index)` as encoded ref keys. Verify-before-report is the
/// caller's second call.
pub fn sharedIndexDrift(flagged_ref_keys: []const []const u8, index_keys: []const []const u8) Drift {
    var i: usize = 0;
    var j: usize = 0;
    var d: Drift = .{ .flag_without_entry = 0, .entry_without_flag = 0 };
    while (i < flagged_ref_keys.len or j < index_keys.len) {
        if (j >= index_keys.len) {
            d.flag_without_entry += 1;
            i += 1;
            continue;
        }
        if (i >= flagged_ref_keys.len) {
            d.entry_without_flag += 1;
            j += 1;
            continue;
        }
        // Compare the bodies (the kind bytes differ: 6 vs 9).
        const a = flagged_ref_keys[i][1..];
        const b = index_keys[j][1..];
        switch (std.mem.order(u8, a, b)) {
            .eq => {
                i += 1;
                j += 1;
            },
            .lt => {
                d.flag_without_entry += 1;
                i += 1;
            },
            .gt => {
                d.entry_without_flag += 1;
                j += 1;
            },
        }
    }
    stats.fsck_shared_index_drift.add(d.flag_without_entry + d.entry_without_flag);
    return d;
}

// ---------------------------------------------------------------------------
// A RAM home store (the portable contracts).
// ---------------------------------------------------------------------------

pub const RamIndexStore = struct {
    gpa: Allocator,
    keys: std.ArrayList([]u8) = .empty,
    refuse_next: u32 = 0,

    pub fn deinit(self: *RamIndexStore) void {
        for (self.keys.items) |k| self.gpa.free(k);
        self.keys.deinit(self.gpa);
    }

    pub fn count(self: *const RamIndexStore) usize {
        return self.keys.items.len;
    }

    fn put(ctx: *anyopaque, k: []const u8, v: []const u8) StoreError!void {
        const self: *RamIndexStore = @ptrCast(@alignCast(ctx));
        records.decodeSharedIndexValue(v) catch return error.Io;
        if (self.refuse_next > 0) {
            self.refuse_next -= 1;
            return error.WindowFull;
        }
        for (self.keys.items) |e| if (std.mem.eql(u8, e, k)) return;
        const d = try self.gpa.dupe(u8, k);
        errdefer self.gpa.free(d);
        try self.keys.append(self.gpa, d);
        std.mem.sort([]u8, self.keys.items, {}, struct {
            fn lt(_: void, a: []u8, b: []u8) bool {
                return key.lessThan(a, b);
            }
        }.lt);
    }

    fn delete(ctx: *anyopaque, k: []const u8) StoreError!void {
        const self: *RamIndexStore = @ptrCast(@alignCast(ctx));
        for (self.keys.items, 0..) |e, i| if (std.mem.eql(u8, e, k)) {
            self.gpa.free(self.keys.orderedRemove(i));
            return;
        };
    }

    fn range(ctx: *anyopaque, gpa: Allocator, lo: []const u8, hi: []const u8, out: *std.ArrayList([]u8)) StoreError!void {
        const self: *RamIndexStore = @ptrCast(@alignCast(ctx));
        for (self.keys.items) |e| {
            if (key.lessThan(e, lo) or !key.lessThan(e, hi)) continue;
            try out.append(gpa, try gpa.dupe(u8, e));
        }
    }

    const vtable: IndexStore.VTable = .{ .put = &put, .delete = &delete, .range = &range };

    pub fn store(self: *RamIndexStore) IndexStore {
        return .{ .ctx = self, .vtable = &vtable };
    }
};

/// A RAM routed-refs table: `(owner_ino, block_idx) → {exists, shared}`.
pub const RamRoutedRefs = struct {
    rows: [32]struct { used: bool = false, owner_ino: u64 = 0, block_idx: u64 = 0, block_index: u32 = 0, shared: bool = false } = @splat(.{}),

    pub fn set(self: *RamRoutedRefs, owner_ino: u64, block_idx: u64, block_index: u32, shared: bool) void {
        for (&self.rows) |*r| if (r.used and r.owner_ino == owner_ino and r.block_idx == block_idx and r.block_index == block_index) {
            r.shared = shared;
            return;
        };
        for (&self.rows) |*r| if (!r.used) {
            r.* = .{ .used = true, .owner_ino = owner_ino, .block_idx = block_idx, .block_index = block_index, .shared = shared };
            return;
        };
        unreachable;
    }

    pub fn remove(self: *RamRoutedRefs, owner_ino: u64, block_idx: u64) void {
        for (&self.rows) |*r| if (r.used and r.owner_ino == owner_ino and r.block_idx == block_idx) {
            r.used = false;
        };
    }

    fn refFlags(ctx: *anyopaque, _: u64, block_idx: u64, ref: Ref) ?RefFlags {
        const self: *RamRoutedRefs = @ptrCast(@alignCast(ctx));
        for (self.rows) |r| if (r.used and r.owner_ino == ref.owner_ino and r.block_idx == block_idx and r.block_index == ref.block_index) return .{ .exists = true, .shared = r.shared };
        return .{ .exists = false, .shared = false };
    }

    fn stillReferenced(ctx: *anyopaque, _: u64, block_idx: u64, owner_ino: u64) bool {
        const self: *RamRoutedRefs = @ptrCast(@alignCast(ctx));
        for (self.rows) |r| if (r.used and r.owner_ino == owner_ino and r.block_idx == block_idx) return true;
        return false;
    }

    const vtable: RoutedRefs.VTable = .{ .refFlags = &refFlags, .stillReferenced = &stillReferenced };

    pub fn routed(self: *RamRoutedRefs) RoutedRefs {
        return .{ .ctx = self, .vtable = &vtable };
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the_shared_index_home_follows_the_allocation_holder; the_index_lives_in_tree0_and_never_pollutes_a_slot_trees_probe: kind 9 under the holder's home, else 0; the kind-6 probe never sees it" {
    try testing.expectEqual(@as(u16, 3), indexHomeVolumeFor(3));
    try testing.expectEqual(@as(u16, 0), indexHomeVolumeFor(null));
    // A kind-9 key sorts outside every kind-6 block range.
    const r = key.blockRange(0xAB, 12);
    const k9 = records.sharedIndexKey(0xAB, 12, 5, 0);
    try testing.expect(!(key.lessThan(&k9, r.hi.slice()) and !key.lessThan(&k9, r.lo.slice())));
}

/// A RAM kind-6 store with the kv_fs `range`/`lookup` shape (the probe's
/// fixture; the Linux binding is `meta/backend.KvBackend`).
pub const RamRefStore = struct {
    gpa: Allocator,
    rows: std.ArrayList(Row) = .empty,
    const Row = struct { k: []u8, v: []u8 };
    const Err = error{ OutOfMemory, Io };

    pub fn deinit(self: *RamRefStore) void {
        for (self.rows.items) |r| {
            self.gpa.free(r.k);
            self.gpa.free(r.v);
        }
        self.rows.deinit(self.gpa);
    }

    pub fn put(self: *RamRefStore, k: []const u8, v: []const u8) !void {
        for (self.rows.items) |*r| if (std.mem.eql(u8, r.k, k)) {
            self.gpa.free(r.v);
            r.v = try self.gpa.dupe(u8, v);
            return;
        };
        const kd = try self.gpa.dupe(u8, k);
        errdefer self.gpa.free(kd);
        const vd = try self.gpa.dupe(u8, v);
        errdefer self.gpa.free(vd);
        try self.rows.append(self.gpa, .{ .k = kd, .v = vd });
        std.mem.sort(Row, self.rows.items, {}, struct {
            fn lt(_: void, a: Row, b: Row) bool {
                return key.lessThan(a.k, b.k);
            }
        }.lt);
    }

    pub fn lookup(self: *RamRefStore, gpa: Allocator, _: u8, k: []const u8) Err!?[]u8 {
        for (self.rows.items) |r| if (std.mem.eql(u8, r.k, k)) return try gpa.dupe(u8, r.v);
        return null;
    }

    pub fn range(self: *RamRefStore, gpa: Allocator, _: u8, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue)) Err!void {
        var n: usize = 0;
        for (self.rows.items) |r| {
            if (key.lessThan(r.k, start) or key.lessThan(end, r.k)) continue;
            if (n >= max) break;
            try out.append(gpa, .{ .key = try gpa.dupe(u8, r.k), .value = try gpa.dupe(u8, r.v) });
            n += 1;
        }
    }

    pub fn store(self: *RamRefStore) block_refs.RefStore {
        return block_refs.RefStore.from(RamRefStore, self);
    }
};

test "the_one_slot_probe_agrees_with_the_derived_census_on_every_published_block: ref_probe folds count and SHARED over one range probe and refuses a malformed record" {
    var ram: RamRefStore = .{ .gpa = testing.allocator };
    defer ram.deinit();
    const store = ram.store();
    const p0 = try refProbe(store, testing.allocator, 7, 3);
    try testing.expectEqual(@as(u32, 0), p0.count);
    try ram.put(&key.blockRefKey(7, 3, 100, 0), &(record.BlockRefValue{ .map_blob = false, .shared = false }).encode());
    try ram.put(&key.blockRefKey(7, 3, 200, 4), &(record.BlockRefValue{ .map_blob = false, .shared = true }).encode());
    try ram.put(&key.blockRefKey(7, 4, 100, 1), &(record.BlockRefValue{ .map_blob = false, .shared = false }).encode());
    const probes0 = block_refs.stats.block_ref_probes.load();
    const p = try refProbe(store, testing.allocator, 7, 3);
    try testing.expectEqual(@as(u32, 2), p.count);
    try testing.expect(p.shared);
    try testing.expectEqual(probes0 + 1, block_refs.stats.block_ref_probes.load());
    const p4 = try refProbe(store, testing.allocator, 7, 4);
    try testing.expectEqual(@as(u32, 1), p4.count);
    try testing.expect(!p4.shared);
    try ram.put(&key.blockRefKey(7, 5, 1, 0), &.{ 1, 0xFF, 0, 0 });
    try testing.expectError(error.Corrupt, refProbe(store, testing.allocator, 7, 5));
}

test "a_clone_on_an_armed_forest_marks_the_source_shared_and_indexes_both_inos: ShareBlock writes both entries once (idempotent), the served form screens 'MarkShared precedes ShareBlock'; a full ring is Deferred" {
    var rs: RamIndexStore = .{ .gpa = testing.allocator };
    defer rs.deinit();
    var routed: RamRoutedRefs = .{};
    var home: IndexHome = .{ .gpa = testing.allocator, .store = rs.store(), .routed = routed.routed() };
    const src: Ref = .{ .owner_ino = 100, .block_index = 2 };
    const dst: Ref = .{ .owner_ino = 200, .block_index = 0 };
    // Served without the marks: rejected, nothing written.
    routed.set(100, 9, 2, false);
    try testing.expectEqual(ShareVerdict.rejected, home.shareBlock(5, 9, &.{ src, dst }, true));
    try testing.expectEqual(@as(usize, 0), rs.count());
    // Marked source, dest not yet published WITH the bit: rejected (the
    // dest's reference does not exist yet) — the cloner's own call is
    // unscreened (it publishes the dest refs after step 2).
    routed.set(100, 9, 2, true);
    try testing.expectEqual(ShareVerdict.rejected, home.shareBlock(5, 9, &.{ src, dst }, true));
    const v = home.shareBlock(5, 9, &.{ src, dst }, false).written;
    try testing.expectEqual(@as(u32, 2), v.written);
    try testing.expectEqual(@as(usize, 2), rs.count());
    const again = home.shareBlock(5, 9, &.{ src, dst }, false).written;
    try testing.expectEqual(@as(u32, 0), again.written);
    try testing.expectEqual(@as(u32, 2), again.already);
    try testing.expectEqual(@as(u32, 2), try home.population(5, 9));
    try testing.expectEqual(@as(u32, 0), try home.population(5, 10));
    rs.refuse_next = 1;
    try testing.expectEqual(ShareVerdict.deferred, home.shareBlock(5, 11, &.{src}, false));
}

test "a terminal free of a SHARED block is never decided locally: ReleaseShared deletes the releaser's ONE entry, GCs a dead cloner's entry, decides Held/Freed/NotShared; the allocator's gate rides it" {
    var rs: RamIndexStore = .{ .gpa = testing.allocator };
    defer rs.deinit();
    var routed: RamRoutedRefs = .{};
    var home: IndexHome = .{ .gpa = testing.allocator, .store = rs.store(), .routed = routed.routed() };
    const src: Ref = .{ .owner_ino = 100, .block_index = 2 };
    const dst: Ref = .{ .owner_ino = 200, .block_index = 0 };
    routed.set(100, 9, 2, true);
    routed.set(200, 9, 0, true);
    _ = home.shareBlock(5, 9, &.{ src, dst }, true).written;
    try testing.expectEqual(ReleaseVerdict.not_shared, home.releaseShared(5, 10, src));
    // The source releases: Held{1} — the dest stands, the block stays allocated.
    routed.remove(100, 9);
    try testing.expectEqual(ReleaseVerdict{ .held = 1 }, home.releaseShared(5, 9, src));
    try testing.expectEqual(@as(usize, 1), rs.count());
    // The dest releases: Freed — the releaser runs the terminal free.
    routed.remove(200, 9);
    try testing.expectEqual(ReleaseVerdict.freed, home.releaseShared(5, 9, dst));
    try testing.expectEqual(@as(usize, 0), rs.count());
    // a_cloner_dying_after_share_block_is_reported_by_c16_and_gcd_at_the_release:
    // the dest's entry stands with no reference; the source's release GCs it.
    routed.set(100, 12, 0, true);
    _ = home.shareBlock(5, 12, &.{ .{ .owner_ino = 100, .block_index = 0 }, .{ .owner_ino = 300, .block_index = 0 } }, false).written;
    const gc0 = stats.release_shared_gc.load();
    routed.remove(100, 12);
    try testing.expectEqual(ReleaseVerdict.freed, home.releaseShared(5, 12, .{ .owner_ino = 100, .block_index = 0 }));
    try testing.expectEqual(gc0 + 1, stats.release_shared_gc.load());
    // The allocator gate: a SHARED block's begin_free asks the home.
    const a = try block_alloc.BlockAllocator.init(testing.allocator, .{ .vol_tag = 5, .vol = 0, .block_size = 4096, .capacity_bytes = 32 * 4096 });
    defer a.deinit();
    var i: u64 = 0;
    while (i < 16) : (i += 1) _ = try a.allocateBlock();
    routed.set(100, 14, 0, true);
    routed.set(200, 14, 0, true);
    _ = home.shareBlock(5, 14, &.{ .{ .owner_ino = 100, .block_index = 0 }, .{ .owner_ino = 200, .block_index = 0 } }, true).written;
    a.markShared(14);
    const owner: Ref = .{ .owner_ino = 100, .block_index = 0 };
    a.shared_gate = home.sharedFreeGate(&owner);
    routed.remove(100, 14); // ino 100's durable release rode its publish
    try testing.expectEqual(block_alloc.BeginFree.nonterminal, try a.freeBlock(14)); // Held{1}: never freed locally
    try testing.expect(a.isShared(14));
    try testing.expectEqual(block_alloc.State.allocated, a.stateOf(14));
    routed.remove(200, 14);
    try testing.expectEqual(block_alloc.BeginFree.terminal, try a.freeBlock(14)); // Freed: the releaser runs the terminal free
    try testing.expect(!a.isShared(14));
    // An unreachable home keeps the reference and the mark.
    a.markShared(15);
    _ = try a.allocateBlock(); // 16
    const Unreachable = struct {
        fn ask(_: *anyopaque, _: u64, _: u64) block_alloc.SharedVerdict {
            return .unreachable_home;
        }
    };
    a.shared_gate = .{ .ctx = &home, .ask = &Unreachable.ask };
    const fail0 = block_alloc.stats.shared_release_failures.load();
    try testing.expectEqual(block_alloc.BeginFree.refused, try a.freeBlock(15));
    try testing.expectEqual(fail0 + 1, block_alloc.stats.shared_release_failures.load());
    try testing.expect(a.isShared(15));
}

test "the un-share witness: an intent registered before step 1 withholds the Held{1} un-share; retired at step 3 it issues with the share_seq; a MarkShared landing first makes the clear stale at the re-check" {
    var rs: RamIndexStore = .{ .gpa = testing.allocator };
    defer rs.deinit();
    var home: IndexHome = .{ .gpa = testing.allocator, .store = rs.store() };
    try testing.expectEqual(UnshareDecision.not_sole, home.decideUnshare(5, 9, 2));
    const seq = home.registerIntent(5, 9, 100).?;
    try testing.expectEqual(@as(u32, 1), home.inFlight(5, 9));
    try testing.expectEqual(@as(u32, 0), home.inFlight(5, 10));
    try testing.expectEqual(UnshareDecision.withheld_in_flight, home.decideUnshare(5, 9, 1));
    try testing.expect(home.retireIntent(seq));
    try testing.expect(!home.retireIntent(seq));
    try testing.expectEqual(@as(u32, 0), home.inFlight(5, 9));
    const d = home.decideUnshare(5, 9, 1);
    try testing.expect(home.confirmUnshare(d.issue));
    // A new share of the block lands before the owner's holder clears: stale.
    _ = home.shareBlock(5, 9, &.{.{ .owner_ino = 300, .block_index = 0 }}, false).written;
    const stale0 = stats.unshare_stale_refused.load();
    try testing.expect(!home.confirmUnshare(d.issue));
    try testing.expectEqual(stale0 + 1, stats.unshare_stale_refused.load());
}

test "C16 shared-index drift: a flag without an entry and an entry without a flag are each counted; aligned sets are clean" {
    const f1 = key.blockRefKey(5, 9, 100, 2);
    const f2 = key.blockRefKey(5, 9, 200, 0);
    const f3 = key.blockRefKey(5, 11, 100, 0);
    const e1 = records.sharedIndexKey(5, 9, 100, 2);
    const e2 = records.sharedIndexKey(5, 9, 200, 0);
    const e4 = records.sharedIndexKey(5, 12, 100, 0);
    try testing.expect(sharedIndexDrift(&.{ &f1, &f2 }, &.{ &e1, &e2 }).clean());
    const d = sharedIndexDrift(&.{ &f1, &f2, &f3 }, &.{ &e1, &e2, &e4 });
    try testing.expectEqual(@as(u64, 1), d.flag_without_entry);
    try testing.expectEqual(@as(u64, 1), d.entry_without_flag);
    try testing.expectEqual(@as(u64, 2), sharedIndexDrift(&.{}, &.{ &e1, &e2 }).entry_without_flag);
}
