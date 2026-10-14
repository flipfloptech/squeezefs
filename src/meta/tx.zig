//! Transaction staging (charter §5.2 "Operations stage records in a
//! task-local transaction (read-your-own-writes overlay over the node
//! cache)"; survey `docs/porting/kv_tree_cache_commit_survey.md` §2.1, §7
//! row 10). PORTABLE: nothing here touches a device, a lock or a clock.
//!
//! A `KvTx` is one op's staged records in stage order — `(tree_id, key,
//! kind, value)`, seqs UNASSIGNED (the conveyor pass stamps them from its
//! reservation inside the leaf-lock window). Ops thread one `KvTx`
//! explicitly; kv ops never nest transactions. `buildQueuedTx` is the
//! pre-queue admission: it checks every slot-tree record's key against its
//! kind through the ONE key codec (a key that does not decode as its kind
//! would route to a foreign slot or fail at every later read), enforces the
//! per-volume record-value cap at the ONE commit-side choke point every tx
//! passes (an over-cap record fails ITS OWN commit, never the checkpoint's
//! freeze — the 2026-08-19 AlreadyFreezing wedge's third leg), fixes the
//! exact entry length (the Σ-admission size, taken BEFORE any lock) and
//! yields the queue entry.
//!
//! The entry the pass writes for a `QueuedTx` is, by construction, the entry
//! `journal.buildEntryWrite` writes for the same frames — `frames()` is the
//! ONE path from staged records to `journal.RecordFrame`s, and the batch
//! writer shares `entryOps` with the solo writer (the batch-of-1
//! equivalence; `batch_of_one_keeps_todays_entry_shape`). The test below
//! pins encode∘decode against `journal.zig`'s own framing.
//!
//! Guards (charter §5.2 "Each queue entry co-owns its tx's guards until
//! terminal outcome") travel as an OPAQUE handle: the 4a lock manager is its
//! own module; this one depends only on the release seam.

const std = @import("std");
const Allocator = std.mem.Allocator;
const counters = @import("../core/counters.zig");
const key = @import("key.zig");
const record = @import("record.zig");
const journal = @import("journal.zig");
const jc = @import("journal_core.zig");

const assert = std.debug.assert;

pub const RecordKind = record.RecordKind;
pub const AdmissionClass = jc.AdmissionClass;
pub const SeqSpan = jc.SeqSpan;

pub const stats = struct {
    /// `meta_kv_forest_key_violations`: a staged `(kind, key)` the key codec
    /// refused at admission — the key does not decode as its kind, or its
    /// routing ino is above the slot namespace — so it fails its commit.
    pub var meta_kv_forest_key_violations: counters.ShardedCounter = .{};
};

// ---------------------------------------------------------------------------
// The guard seam.
// ---------------------------------------------------------------------------

/// The tx's 4a guard set, co-owned by the queue entry from enqueue to the
/// tx's TERMINAL outcome (post-ack on success, post-rollback on failure): a
/// dropped committer drops only its own frame — the entry's hold keeps
/// same-key exclusion alive structurally. Opaque: `ctx` is whatever the
/// lock manager hands out (its owned guard slice); `release` drops it. The
/// empty set is for commits whose exclusion is architectural rather than
/// guard-borne (the pre-arm writer-claim tx; compensation records).
pub const GuardSet = struct {
    ctx: ?*anyopaque,
    release_fn: ?*const fn (*anyopaque) void,

    pub const none: GuardSet = .{ .ctx = null, .release_fn = null };

    pub fn init(ctx: *anyopaque, release_fn: *const fn (*anyopaque) void) GuardSet {
        return .{ .ctx = ctx, .release_fn = release_fn };
    }

    pub fn isHeld(self: GuardSet) bool {
        return self.ctx != null;
    }

    /// Drop the guards. Idempotent: the terminal outcome releases exactly
    /// once whichever path (fan-out, rollback, abandon) reaches it first.
    pub fn release(self: *GuardSet) void {
        const ctx = self.ctx orelse return;
        const f = self.release_fn.?;
        self.* = .none;
        f(ctx);
    }
};

/// The commit door's tokens (charter §5.5 "every commit takes a token per
/// slot before queueing (held to terminal outcome)"): one per distinct
/// slot of the entry, inline (`MAX_DOOR_SLOTS`, no allocation), co-owned by
/// the queue entry beside the guards and returned at the terminal outcome
/// AFTER them (the reference's fan-out order). Opaque like `GuardSet`: the
/// door (`sym/slots/door.zig`) hands out `ctx` and the release.
pub const DoorTokens = struct {
    ctx: ?*anyopaque = null,
    release_fn: ?*const fn (*anyopaque, *DoorTokens) void = null,
    slots: [MAX_DOOR_SLOTS]u16 = undefined,
    len: u8 = 0,

    /// A namespace op names ≤ 6 slots (rename's two parents, two inodes
    /// and a whiteout mint, plus a mint); the reclaim's destroy batch
    /// (`kv_fs.RECLAIM_BATCH` = 64 corpses, each in its own slot) is the
    /// widest entry the door sees — 64 × 2 B inline, still no allocation.
    pub const MAX_DOOR_SLOTS: usize = 64;
    pub const none: DoorTokens = .{};

    pub fn isHeld(self: DoorTokens) bool {
        return self.ctx != null;
    }

    /// Return the tokens. Idempotent (the terminal outcome, whichever path).
    pub fn release(self: *DoorTokens) void {
        const ctx = self.ctx orelse return;
        const f = self.release_fn.?;
        self.ctx = null;
        self.release_fn = null;
        f(ctx, self);
        self.len = 0;
    }
};

// ---------------------------------------------------------------------------
// KvTx — staging.
// ---------------------------------------------------------------------------

/// One staged record: the key as the op built it (the charter's key, kind
/// byte included), bytes owned by the tx's arena.
pub const Staged = struct {
    tree_id: u8,
    key: []const u8,
    kind: RecordKind,
    /// Empty for `Delete`; the delta payload for `Delta`.
    value: []const u8,
};

/// The D4.a attribution site: a successful commit counts one journal
/// entry against the `KvTx` construction site, so per-op entry ratios
/// decompose into NAMED committers (`meta_kv_commit_sites`).
pub const Site = std.builtin.SourceLocation;

/// A name a tx changes (decision 0003 §AB.2): the parent's key ino and the
/// name (tx-arena bytes). The conveyor names it to every kernel that may
/// cache it — the token readers' through the recall, this mount's own when
/// the tx is not its kernel's.
pub const NameNote = record.NameRef;

/// The names one namespace op changes: a rename's two (a create, link,
/// unlink, rmdir name one; an exchange or a whiteout re-names the same two).
pub const NAME_NOTES_MAX = 2;

/// The notes a tx carries into its queue entry. `overflow`: a dentry the
/// notes do not name (a third name, an arena refusal) — its readers drop
/// every dentry instead (FUSE_NOTIFY_INC_EPOCH).
pub const NameNotes = struct {
    buf: [NAME_NOTES_MAX]NameNote = undefined,
    len: u8 = 0,
    overflow: bool = false,

    pub fn slice(self: *const NameNotes) []const NameNote {
        return self.buf[0..self.len];
    }
};

pub const KvTx = struct {
    arena: std.heap.ArenaAllocator,
    staged: std.ArrayList(Staged) = .empty,
    site: Site,
    /// A structural commit whose exclusion is the engine's — the migration
    /// engine's delta copy into the target (the slot is CLOSED at the
    /// source, the target's tree unpublished) — takes no door tokens.
    door_bypass: bool = false,
    /// The ring class the entry admits in: every user op is `.user`; the
    /// rollback's compensation records and the structural plane's control
    /// entries ride the checkpoint class (they never park behind user
    /// commits — the liveness argument).
    class: AdmissionClass = .user,
    guards: GuardSet = .none,
    /// The tx mutates no record a read token carries (a control xattr on
    /// its own), so the conveyor pass recalls nothing for it.
    token_quiet: bool = false,
    /// This mount's kernel asked for the change (a FUSE request) — or holds
    /// nothing it changes (a forgotten corpse): its own dentry and attr
    /// caches follow the reply. Any other origin — a peer's served step or
    /// ship, a job, a tool — is invalidated in it (§AB.2).
    kernel_origin: bool = false,
    names: NameNotes = .{},

    /// `site` is the committer's `@src()`.
    pub fn init(gpa: Allocator, site: Site) KvTx {
        return .{ .arena = std.heap.ArenaAllocator.init(gpa), .site = site };
    }

    /// Releases every staged byte and the guards (if the tx never became a
    /// queue entry).
    pub fn deinit(self: *KvTx) void {
        self.guards.release();
        self.arena.deinit();
        self.* = undefined;
    }

    pub fn isEmpty(self: *const KvTx) bool {
        return self.staged.items.len == 0;
    }

    /// Note a name this tx changes (deduplicated: a rename's re-insert
    /// names what its delete did). A NUL-led name — the stripe map's
    /// reserved markers (`xv/stripe.zig`) — is no kernel's: never noted
    /// (and never on the wire: a recalled name carries no NUL).
    pub fn noteName(self: *KvTx, parent: u64, name: []const u8) void {
        if (name.len == 0 or name[0] == 0) return;
        for (self.names.slice()) |n| if (n.parent == parent and std.mem.eql(u8, n.name, name)) return;
        if (self.names.len == NAME_NOTES_MAX) {
            self.names.overflow = true;
            return;
        }
        const copy = self.arena.allocator().dupe(u8, name) catch {
            self.names.overflow = true;
            return;
        };
        self.names.buf[self.names.len] = .{ .parent = parent, .name = copy };
        self.names.len += 1;
    }

    /// Attach the op's held guard set; multi-commit ops hand the same set
    /// to each sequential commit.
    pub fn holdGuards(self: *KvTx, guards: GuardSet) void {
        self.guards = guards;
    }

    pub fn stagePut(self: *KvTx, tree_id: u8, k: []const u8, value: []const u8) Allocator.Error!void {
        try self.stage(tree_id, k, .put, value);
    }

    pub fn stageDelta(self: *KvTx, tree_id: u8, k: []const u8, delta: record.InodeDelta) Allocator.Error!void {
        const enc = delta.encode();
        try self.stage(tree_id, k, .delta, enc.slice());
    }

    /// A pre-encoded `Delta` payload (the layout-delta class).
    pub fn stageDeltaRaw(self: *KvTx, tree_id: u8, k: []const u8, payload: []const u8) Allocator.Error!void {
        try self.stage(tree_id, k, .delta, payload);
    }

    pub fn stageDelete(self: *KvTx, tree_id: u8, k: []const u8) Allocator.Error!void {
        try self.stage(tree_id, k, .delete, &.{});
    }

    fn stage(self: *KvTx, tree_id: u8, k: []const u8, kind: RecordKind, value: []const u8) Allocator.Error!void {
        assert(k.len != 0);
        assert(kind != .delete or value.len == 0);
        const a = self.arena.allocator();
        const kd = try a.dupe(u8, k);
        const vd = try a.dupe(u8, value);
        try self.staged.append(a, .{ .tree_id = tree_id, .key = kd, .kind = kind, .value = vd });
    }
};

// ---------------------------------------------------------------------------
// Read-your-own-writes: the dentry/xattr chain probe folded through the
// overlay (`chain_occupancy` / `dentry_insert_key`).
// ---------------------------------------------------------------------------

/// The occupied `coll_seq`s of one hash chain `[start, end]` (same-kind
/// keys whose LAST byte is the `coll_seq`): the live range scan's
/// keys (`live_chain_keys`, the caller's scan of exactly that range) folded
/// through the tx overlay — a slot this tx staged a `Delete` for (rename
/// EXCHANGE) is free for its re-insert; a slot it staged a `Put` for is
/// occupied; a `Delta` changes nothing. Ascending into `out`.
pub fn chainOccupancy(tx: *const KvTx, chain_kind: u8, live_chain_keys: []const []const u8, start: []const u8, end: []const u8, out: *[256]u8) []u8 {
    var bits: [4]u64 = @splat(0);
    for (live_chain_keys) |k| setBit(&bits, k[k.len - 1]);
    for (tx.staged.items) |s| {
        if (s.tree_id != chain_kind) continue;
        if (key.lessThan(s.key, start) or key.lessThan(end, s.key)) continue;
        switch (s.kind) {
            .delete => clearBit(&bits, s.key[s.key.len - 1]),
            .put => setBit(&bits, s.key[s.key.len - 1]),
            .delta => {},
        }
    }
    var n: usize = 0;
    for (bits, 0..) |word, w| {
        var rest = word;
        while (rest != 0) : (rest &= rest - 1) {
            out[n] = @intCast(w * 64 + @ctz(rest));
            n += 1;
        }
    }
    return out[0..n];
}

fn setBit(bits: *[4]u64, c: u8) void {
    bits[c >> 6] |= @as(u64, 1) << @truncate(c);
}

fn clearBit(bits: *[4]u64, c: u8) void {
    bits[c >> 6] &= ~(@as(u64, 1) << @truncate(c));
}

/// The inclusive key range of `(parent, hash54)`'s chain — what the caller
/// scans for `dentryInsertKey`'s `live_chain_keys`.
pub const ChainRange = struct { start: [key.DENTRY_KEY_LEN]u8, end: [key.DENTRY_KEY_LEN]u8 };

pub fn dentryChainRange(parent: u64, hash54: u64) ChainRange {
    return .{ .start = key.dentryKey(parent, hash54, 0), .end = key.dentryKey(parent, hash54, std.math.maxInt(u8)) };
}

pub const DentryInsertError = error{
    /// A name the u8 `name_len` cannot frame.
    NameTooLong,
    /// The 257th same-hash name: counted, clean, never a panic.
    DentryChainOverflow,
};

/// The chain key a NEW dentry for `(parent, name)` occupies: the first free
/// `coll_seq` in the seeded-hash chain, overlay-aware (a rename EXCHANGE
/// re-inserts into the slot it just deleted). `live_chain_keys` is the live
/// scan of `dentryChainRange(parent, hash54(name))`.
pub fn dentryInsertKey(tx: *const KvTx, name_key: *const key.NameKey, parent: u64, name: []const u8, live_chain_keys: []const []const u8) DentryInsertError![key.DENTRY_KEY_LEN]u8 {
    if (name.len > record.NAME_MAX) return error.NameTooLong;
    const hash = key.dentryNameHash54(name, name_key);
    const range = dentryChainRange(parent, hash);
    var buf: [256]u8 = undefined;
    const occupied = chainOccupancy(tx, key.TREE_DENTRIES, live_chain_keys, &range.start, &range.end, &buf);
    const coll = try key.assignDentryCollSeq(occupied);
    return key.dentryKey(parent, hash, coll);
}

// ---------------------------------------------------------------------------
// UndoKey — a key's pre-image captured at RAM apply (§5.2 "Failure": the
// seq-conditional rollback).
// ---------------------------------------------------------------------------

/// What the key folded to in its leaf BEFORE this tx's records were applied
/// (first touch wins across a batch, so the batch-range rollback restores
/// the pre-batch state exactly).
pub const PreImage = union(enum) {
    /// A live value.
    live: []const u8,
    /// A tombstone shadows the key.
    tombstone,
    /// No records for the key.
    absent,
};

pub const Compensation = struct { kind: RecordKind, value: []const u8 };

pub const UndoKey = struct {
    tree_id: u8,
    /// The staged key.
    key: []const u8,
    pre: PreImage,

    /// The compensating record for a record of this tx that ESCAPED the open
    /// overlay into a frozen or durable bset before the write failed (a
    /// freeze raced the failed write): re-Put the pre-image, or Delete what
    /// was tombstoned/absent — committed through the checkpoint-class
    /// reserve so a crash after the escaped records reach a durable bset
    /// still replays to the rolled-back state.
    pub fn compensation(self: UndoKey) Compensation {
        return switch (self.pre) {
            .live => |v| .{ .kind = .put, .value = v },
            .tombstone, .absent => .{ .kind = .delete, .value = &.{} },
        };
    }

    /// The seq-conditional probe: a record of this tx escaped iff the key's
    /// newest record seq (after the open-overlay removal) lies in the tx's
    /// stamped span — a NEWER committed record stands.
    pub fn escaped(span: SeqSpan, newest_seq: ?u64) bool {
        const s = newest_seq orelse return false;
        return span.contains(s);
    }
};

// ---------------------------------------------------------------------------
// buildQueuedTx — pre-queue admission.
// ---------------------------------------------------------------------------

/// One record of the queue entry: the tag's tree id and the record, seq 0
/// until the pass stamps it.
pub const Rec = struct {
    tree_id: u8,
    key: []const u8,
    kind: RecordKind,
    value: []const u8,
    seq: u64 = 0,
};

/// Why `buildQueuedTx` refused — with the evidence the committer's error
/// reports (the typed classes of survey §2.19: `E2BIG`/`EINVAL`-class
/// clean refusals, the op fails ALONE).
pub const BuildRefusal = union(enum) {
    value_too_large: struct { len: usize, cap: usize },
    entry_too_large: struct { len: u64, cap: u64 },
    forest_key: key.ForestKeyError,
};

pub const BuildError = error{
    ValueTooLarge,
    EntryTooLarge,
    ForestKeyRefused,
    OutOfMemory,
};

/// One transaction on the commit conveyor: the encoded records (seqs
/// stamped by the pass inside the lock window), the exact entry length
/// (the Σ-admission and the drain byte cap read it), the attribution site,
/// the co-owned guard set. The completion channel is the conveyor's (it
/// owns the wait primitive); the three charter items are `recs`, `guards`
/// and that channel.
pub const QueuedTx = struct {
    arena: std.heap.ArenaAllocator,
    recs: []Rec,
    /// Exact journal entry length (`entryLenFor`).
    len: u64,
    class: AdmissionClass,
    token_quiet: bool,
    kernel_origin: bool = false,
    names: NameNotes = .{},
    site: Site,
    /// Never read, only owned: the hold IS the same-key exclusion. Released
    /// at the terminal outcome BEFORE the ack (a committer re-entering on
    /// the same key never self-waits on its own tx).
    guards: GuardSet,
    /// The commit door's tokens, taken BEFORE the enqueue and returned at
    /// the terminal outcome after the guards.
    door: DoorTokens = .none,
    door_bypass: bool = false,

    /// The terminal outcome: drop the guards, the door tokens, free the
    /// records.
    pub fn deinit(self: *QueuedTx) void {
        self.guards.release();
        self.door.release();
        self.arena.deinit();
        self.* = undefined;
    }

    /// Stamp this member's record seqs back-to-back from `seq_base`
    /// (`seq = seq_base + i`); answers the member's stamped span, the
    /// rollback's seq-conditional domain. The pass advances its cursor by
    /// `len` per member so members never share a stamp.
    pub fn stampSeqs(self: *QueuedTx, seq_base: u64) SeqSpan {
        for (self.recs, 0..) |*r, i| r.seq = seq_base + @as(u64, @intCast(i));
        return SeqSpan.stamped(seq_base, self.len);
    }

    /// The entry's record frames — the ONE path to the journal wire; the
    /// pass hands them to `journal.buildBatchWrite` (or a solo
    /// `buildEntryWrite`: byte-identical). Leaf content is level 0.
    pub fn frames(self: *const QueuedTx, out: []journal.RecordFrame) []journal.RecordFrame {
        assert(out.len >= self.recs.len);
        for (self.recs, 0..) |r, i| out[i] = frameOf(r);
        return out[0..self.recs.len];
    }

    /// The frames into a fresh arena-owned slice.
    pub fn framesAlloc(self: *QueuedTx) Allocator.Error![]journal.RecordFrame {
        const out = try self.arena.allocator().alloc(journal.RecordFrame, self.recs.len);
        return self.frames(out);
    }
};

fn frameOf(r: Rec) journal.RecordFrame {
    return .{
        .tag = journal.tagFor(r.tree_id, 0),
        .key = r.key,
        .kind = journalKindOf(r.kind),
        .seq = r.seq,
        .value = r.value,
    };
}

/// A record kind as the journal frame's (one encoding, asserted below).
pub fn journalKindOf(k: RecordKind) journal.RecordKind {
    return @fromBackingInt(@intCast(@backingInt(k)));
}

comptime {
    // The journal's frame kind enum mirrors the record kind byte for byte.
    assert(@backingInt(journal.RecordKind.put) == @backingInt(RecordKind.put));
    assert(@backingInt(journal.RecordKind.delta) == @backingInt(RecordKind.delta));
    assert(@backingInt(journal.RecordKind.delete) == @backingInt(RecordKind.delete));
}

/// Exact whole-entry size for staged records (header + Σ `1 + 16 + key +
/// value`), refusing past the 128 KiB whole-entry cap — `journal.entryLenFor`
/// over the entry's frames, so a planner sizing a tx ahead of staging it
/// (the destroy chunker) derives from the arithmetic the admission enforces.
pub fn entryLenFor(recs: []const Rec) error{EntryTooLarge}!u64 {
    var payload: u64 = 0;
    for (recs) |r| payload += journal.recordFrameLen(r.key.len, r.value.len);
    const len = journal.ENTRY_HDR_LEN + payload;
    if (len > journal.MAX_ENTRY_LEN) return error.EntryTooLarge;
    return len;
}

/// Steps (1)–(2a) of the commit pipeline for ONE tx. CONSUMES `tx` (its
/// arena moves into the entry — nothing is copied; the tx must not be
/// deinit'ed afterwards; on a refusal its guards and bytes are released
/// here). `node_size` is the volume's: the record-value cap is
/// `min(64 KiB, node_size/4)` plus the xattr envelope, the very check the
/// freeze runs. Tree 0's control records are keyed by their own codec and
/// pass the key check untouched.
pub fn buildQueuedTx(tx: *KvTx, node_size: usize, diag: ?*BuildRefusal) BuildError!QueuedTx {
    var arena = tx.arena;
    var guards = tx.guards;
    const staged = tx.staged.items;
    const class = tx.class;
    const token_quiet = tx.token_quiet;
    const kernel_origin = tx.kernel_origin;
    const names = tx.names;
    const site = tx.site;
    const door_bypass = tx.door_bypass;
    tx.* = undefined;
    errdefer {
        guards.release();
        arena.deinit();
    }
    const a = arena.allocator();
    const recs = try a.alloc(Rec, staged.len);
    const cap = record.recordValueCap(node_size);
    for (staged, 0..) |s, i| {
        if (s.value.len > cap) {
            if (diag) |d| d.* = .{ .value_too_large = .{ .len = s.value.len, .cap = cap } };
            return error.ValueTooLarge;
        }
        if (key.isSlotTreeKind(s.tree_id)) {
            _ = key.checkSlotTreeKey(s.tree_id, s.key) catch |e| {
                stats.meta_kv_forest_key_violations.inc();
                if (diag) |d| d.* = .{ .forest_key = e };
                return error.ForestKeyRefused;
            };
        }
        recs[i] = .{ .tree_id = s.tree_id, .key = s.key, .kind = s.kind, .value = s.value };
    }
    const len = entryLenFor(recs) catch {
        if (diag) |d| d.* = .{ .entry_too_large = .{ .len = journal.ENTRY_HDR_LEN + rawPayloadLen(recs), .cap = journal.MAX_ENTRY_LEN } };
        return error.EntryTooLarge;
    };
    return .{
        .arena = arena,
        .recs = recs,
        .len = len,
        .class = class,
        .token_quiet = token_quiet,
        .kernel_origin = kernel_origin,
        .names = names,
        .site = site,
        .guards = guards,
        .door_bypass = door_bypass,
    };
}

fn rawPayloadLen(recs: []const Rec) u64 {
    var n: u64 = 0;
    for (recs) |r| n += journal.recordFrameLen(r.key.len, r.value.len);
    return n;
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

const TestGuards = struct {
    released: u32 = 0,

    fn release(ctx: *anyopaque) void {
        const self: *TestGuards = @ptrCast(@alignCast(ctx));
        self.released += 1;
    }

    fn set(self: *TestGuards) GuardSet {
        return GuardSet.init(self, release);
    }
};

fn stageCreate(tx: *KvTx, parent: u64, child: u64, name: []const u8, seed: u64) !void {
    const name_key: key.NameKey = .init(seed);
    const iv = record.InodeValue{ .mode = 0o100644, .nlink = 1 };
    try tx.stagePut(key.TREE_INODES, &key.inodeKey(child), &iv.encode());
    var dbuf: [300]u8 = undefined;
    const dv = try (record.DentryValue{ .child_ino = child, .file_type = 8, .name = name }).encodeInto(&dbuf);
    const dk = try dentryInsertKey(tx, &name_key, parent, name, &.{});
    try tx.stagePut(key.TREE_DENTRIES, &dk, dv);
    try tx.stageDelta(key.TREE_INODES, &key.inodeKey(parent), record.InodeDelta.times(7, 7));
}

test "the name notes (decision 0003 §AB.2): deduplicated, two at most (a third overflows), a NUL-led stripe marker never noted; they ride into the queue entry" {
    var t = KvTx.init(testing.allocator, @src());
    t.noteName(5, "a");
    t.noteName(5, "a");
    t.noteName(5, "\x00sqz.stripe:00");
    t.noteName(6, "b");
    try testing.expectEqual(@as(usize, 2), t.names.slice().len);
    try testing.expect(!t.names.overflow);
    t.noteName(7, "c");
    try testing.expect(t.names.overflow);
    try t.stagePut(key.TREE_INODES, &key.inodeKey(5), "v");
    var q = try buildQueuedTx(&t, 4096, null);
    defer q.deinit();
    try testing.expectEqualStrings("b", q.names.slice()[1].name);
    try testing.expect(q.names.overflow);
}

test "staging keeps stage order, kinds and bytes; seqs are unassigned until the pass stamps them" {
    var tx = KvTx.init(testing.allocator, @src());
    try testing.expect(tx.isEmpty());
    try stageCreate(&tx, 1, 42, "hello", 0xABCD);
    try tx.stageDelete(key.TREE_XATTRS, &key.xattrKey(42, 5, 0));
    try testing.expectEqual(@as(usize, 4), tx.staged.items.len);
    try testing.expectEqual(RecordKind.put, tx.staged.items[0].kind);
    try testing.expectEqual(RecordKind.put, tx.staged.items[1].kind);
    try testing.expectEqual(RecordKind.delta, tx.staged.items[2].kind);
    try testing.expectEqual(RecordKind.delete, tx.staged.items[3].kind);
    try testing.expectEqual(@as(usize, 0), tx.staged.items[3].value.len);
    try testing.expectEqual(@as(u64, 42), try key.decodeInodeKey(tx.staged.items[0].key));
    const d = try record.InodeDelta.decode(tx.staged.items[2].value);
    try testing.expectEqual(record.DELTA_TIMES, d.mask);

    var q = try buildQueuedTx(&tx, 256 * 1024, null);
    defer q.deinit();
    for (q.recs) |r| try testing.expectEqual(@as(u64, 0), r.seq);
    const span = q.stampSeqs(1000);
    try testing.expectEqual(@as(u64, 1000), q.recs[0].seq);
    try testing.expectEqual(@as(u64, 1003), q.recs[3].seq);
    try testing.expectEqual(@as(u64, 1000), span.lo);
    try testing.expectEqual(1000 + q.len, span.hi);
    try testing.expect(UndoKey.escaped(span, 1002));
    try testing.expect(!UndoKey.escaped(span, 1000 + q.len));
    try testing.expect(!UndoKey.escaped(span, null));
}

test "entry length is the exact journal arithmetic; the queue entry's frames are byte-identical to journal.buildEntryWrite of the same records (batch-of-one shape)" {
    var tx = KvTx.init(testing.allocator, @src());
    try stageCreate(&tx, 1, 77, "batch-of-one", 0x5EED);
    var q = try buildQueuedTx(&tx, 256 * 1024, null);
    defer q.deinit();

    // The exact size: 24 + Σ (1 + 16 + key + value).
    var want: u64 = journal.ENTRY_HDR_LEN;
    for (q.recs) |r| want += 1 + journal.RECORD_HEADER_LEN + r.key.len + r.value.len;
    try testing.expectEqual(want, q.len);
    try testing.expectEqual(want, try entryLenFor(q.recs));

    const seq_base: u64 = 4072 * 3 + 17;
    _ = q.stampSeqs(seq_base);
    const frames = try q.framesAlloc();
    try testing.expectEqual(q.len, try journal.entryLenFor(frames));

    // Solo write of the member's frames at its reservation vs the member as
    // the ONE part of a batch window: identical ops (the shared entryOps).
    const geo: journal.CoreGeometry = .{ .page_data_len = journal.PAGE_DATA_LEN, .pages = 8, .reserve_bytes = 0, .grain = 1 };
    const layout: journal.RingLayout = .{ .base = 0, .pages = 8 };
    const res = journal.Reservation.unpadded(seq_base, q.len);
    var solo = try journal.buildEntryWrite(testing.allocator, layout, geo, 0, res, frames);
    defer solo.deinit();
    var batch = try journal.buildBatchWrite(testing.allocator, layout, geo, 0, res, &.{.{ .res = res, .records = frames }});
    defer batch.deinit();
    try testing.expectEqual(solo.ops.len, batch.ops.len);
    for (solo.ops, batch.ops) |s, b| {
        try testing.expectEqual(s.offset, b.offset);
        try testing.expectEqualSlices(u8, s.bytes, b.bytes);
    }

    // encode ∘ decode: the payload the ops carry decodes to exactly the
    // staged records, tags `tree_id | 0 << 4`, seqs as stamped.
    const image = try testing.allocator.alloc(u8, 8 * journal.PAGE_LEN);
    defer testing.allocator.free(image);
    @memset(image, 0);
    journal.applyOps(image, 0, solo.ops);
    var rec = try journal.replayScanImage(testing.allocator, image, geo, seq_base, 0);
    defer rec.deinit();
    try testing.expectEqual(@as(usize, 1), rec.entries.len);
    try testing.expectEqual(seq_base, rec.entries[0].seq);
    var it = rec.entries[0].records();
    var i: usize = 0;
    while (it.next()) |f| : (i += 1) {
        const r = q.recs[i];
        try testing.expectEqual(journal.tagFor(r.tree_id, 0), f.tag);
        try testing.expectEqual(@as(u8, 0), journal.untag(f.tag).level);
        try testing.expectEqualSlices(u8, r.key, f.key);
        try testing.expectEqualSlices(u8, r.value, f.value);
        try testing.expectEqual(r.seq, f.seq);
        try testing.expectEqual(@backingInt(r.kind), @backingInt(f.kind));
    }
    try testing.expectEqual(q.recs.len, i);
}

test "admission checks every slot-tree key against its kind and routes by the owner; control records pass untouched; a refused key counts and fails the commit alone" {
    var tx = KvTx.init(testing.allocator, @src());
    try tx.stagePut(key.TREE_INODES, &key.inodeKey(key.guestLocalIno(3, 9)), "v");
    try tx.stagePut(key.TREE_BLOCK_REFS, &key.blockRefKey(0x11, 5, 9, 2), &.{ 1, 0, 0, 0 });
    try tx.stagePut(key.TREE_CONTROL, "slot_state:7", "x");
    var q = try buildQueuedTx(&tx, 64 * 1024, null);
    defer q.deinit();
    try testing.expectEqual(key.INODE_KEY_LEN, q.recs[0].key.len);
    try testing.expectEqual(key.TREE_INODES, q.recs[0].key[8]);
    try testing.expectEqual(@as(key.ForestSlot, 4), try key.slotTreeKeySlot(q.recs[0].key));
    try testing.expectEqual(key.BLOCK_REF_KEY_LEN, q.recs[1].key.len);
    try testing.expectEqual(key.TREE_BLOCK_REFS, q.recs[1].key[0]);
    try testing.expectEqual(@as(key.ForestSlot, 0), try key.slotTreeKeySlot(q.recs[1].key));
    try testing.expectEqualStrings("slot_state:7", q.recs[2].key);

    // A key of the wrong shape for its kind refuses, counted; so does a
    // routing ino above the slot namespace.
    const before = stats.meta_kv_forest_key_violations.load();
    var bad = KvTx.init(testing.allocator, @src());
    try bad.stagePut(key.TREE_DENTRIES, &key.inodeKey(5), "v");
    var diag: BuildRefusal = undefined;
    try testing.expectError(error.ForestKeyRefused, buildQueuedTx(&bad, 64 * 1024, &diag));
    try testing.expectEqual(key.ForestKeyError.MalformedKnownKind, diag.forest_key);
    var high = KvTx.init(testing.allocator, @src());
    try high.stagePut(key.TREE_INODES, &key.inodeKey(@as(u64, key.FOREST_SLOT_MAX + 1) << 40), "v");
    try testing.expectError(error.ForestKeyRefused, buildQueuedTx(&high, 64 * 1024, &diag));
    try testing.expectEqual(key.ForestKeyError.SlotOutOfNamespace, diag.forest_key);
    try testing.expectEqual(before + 2, stats.meta_kv_forest_key_violations.load());
}

test "value cap min(64 KiB, node_size/4) + envelope is enforced at the choke point as a typed refusal; the whole-entry cap refuses past 128 KiB" {
    // 64 KiB node: cap = 16 384 + 256.
    const cap_64k = record.recordValueCap(64 * 1024);
    try testing.expectEqual(@as(usize, 16_384 + 256), cap_64k);
    const big = try testing.allocator.alloc(u8, cap_64k + 1);
    defer testing.allocator.free(big);
    @memset(big, 0xAB);
    var tx = KvTx.init(testing.allocator, @src());
    try tx.stagePut(key.TREE_XATTRS, &key.xattrKey(9, 1, 0), big[0..cap_64k]);
    try tx.stagePut(key.TREE_XATTRS, &key.xattrKey(9, 2, 0), big);
    var diag: BuildRefusal = undefined;
    try testing.expectError(error.ValueTooLarge, buildQueuedTx(&tx, 64 * 1024, &diag));
    try testing.expectEqual(cap_64k + 1, diag.value_too_large.len);
    try testing.expectEqual(cap_64k, diag.value_too_large.cap);

    // At the 256 KiB default the cap is exactly XATTR_SIZE_MAX + envelope and
    // the same value is admitted.
    try testing.expectEqual(@as(usize, 65_536 + 256), record.recordValueCap(256 * 1024));
    var ok = KvTx.init(testing.allocator, @src());
    try ok.stagePut(key.TREE_XATTRS, &key.xattrKey(9, 2, 0), big);
    var q = try buildQueuedTx(&ok, 256 * 1024, null);
    q.deinit();

    // Two maximal values exceed the whole-entry cap: EntryTooLarge, typed.
    const maxv = try testing.allocator.alloc(u8, record.recordValueCap(256 * 1024));
    defer testing.allocator.free(maxv);
    @memset(maxv, 1);
    var over = KvTx.init(testing.allocator, @src());
    try over.stagePut(key.TREE_XATTRS, &key.xattrKey(9, 1, 0), maxv);
    try over.stagePut(key.TREE_XATTRS, &key.xattrKey(9, 2, 0), maxv);
    try testing.expectError(error.EntryTooLarge, buildQueuedTx(&over, 256 * 1024, &diag));
    try testing.expectEqual(journal.MAX_ENTRY_LEN, diag.entry_too_large.cap);
    try testing.expect(diag.entry_too_large.len > journal.MAX_ENTRY_LEN);
}

test "guards travel with the entry and release exactly once at the terminal outcome, whichever path reaches it" {
    var g: TestGuards = .{};
    var tx = KvTx.init(testing.allocator, @src());
    tx.holdGuards(g.set());
    try tx.stagePut(key.TREE_INODES, &key.inodeKey(3), "v");
    var q = try buildQueuedTx(&tx, 64 * 1024, null);
    try testing.expect(q.guards.isHeld());
    try testing.expectEqual(@as(u32, 0), g.released);
    // Fan-out drops the guards BEFORE the ack; a later deinit is a no-op.
    q.guards.release();
    try testing.expectEqual(@as(u32, 1), g.released);
    try testing.expect(!q.guards.isHeld());
    q.deinit();
    try testing.expectEqual(@as(u32, 1), g.released);

    // A refused build releases the guards it was handed (the committer's
    // frame is the only other owner, and it fails out).
    var g2: TestGuards = .{};
    var bad = KvTx.init(testing.allocator, @src());
    bad.holdGuards(g2.set());
    const big = try testing.allocator.alloc(u8, record.recordValueCap(64 * 1024) + 1);
    defer testing.allocator.free(big);
    @memset(big, 0);
    try bad.stagePut(key.TREE_XATTRS, &key.xattrKey(9, 1, 0), big);
    try testing.expectError(error.ValueTooLarge, buildQueuedTx(&bad, 64 * 1024, null));
    try testing.expectEqual(@as(u32, 1), g2.released);

    // A tx that never becomes an entry releases on its own deinit.
    var g3: TestGuards = .{};
    var dropped = KvTx.init(testing.allocator, @src());
    dropped.holdGuards(g3.set());
    dropped.deinit();
    try testing.expectEqual(@as(u32, 1), g3.released);
}

test "chain occupancy folds the live scan through the overlay: a staged Delete frees its slot, a staged Put takes one, a Delta is inert, other chains are ignored" {
    const seed: key.NameKey = .init(99);
    const parent: u64 = 1;
    const hash = key.dentryNameHash54("name-a", &seed);
    const range = dentryChainRange(parent, hash);
    const k0 = key.dentryKey(parent, hash, 0);
    const k1 = key.dentryKey(parent, hash, 1);
    const k2 = key.dentryKey(parent, hash, 2);
    const k7 = key.dentryKey(parent, hash, 7);
    const live = [_][]const u8{ &k0, &k1, &k7 };

    var tx = KvTx.init(testing.allocator, @src());
    defer tx.deinit();
    var buf: [256]u8 = undefined;
    try testing.expectEqualSlices(u8, &.{ 0, 1, 7 }, chainOccupancy(&tx, key.TREE_DENTRIES, &live, &range.start, &range.end, &buf));

    // Rename EXCHANGE: delete slot 1 ⇒ free; put slot 2 ⇒ taken.
    try tx.stageDelete(key.TREE_DENTRIES, &k1);
    try tx.stagePut(key.TREE_DENTRIES, &k2, "d");
    // A Delta on the chain and records of other chains / kinds: inert.
    try tx.stageDeltaRaw(key.TREE_DENTRIES, &k0, &.{ 0, 0 });
    try tx.stagePut(key.TREE_DENTRIES, &key.dentryKey(parent, hash + 1, 3), "x");
    try tx.stagePut(key.TREE_DENTRIES, &key.dentryKey(parent + 1, hash, 4), "x");
    try tx.stageDelete(key.TREE_XATTRS, &key.xattrKey(parent, hash, 7));
    try testing.expectEqualSlices(u8, &.{ 0, 2, 7 }, chainOccupancy(&tx, key.TREE_DENTRIES, &live, &range.start, &range.end, &buf));

    // The insert key is the first free slot through the overlay.
    const ins = try dentryInsertKey(&tx, &seed, parent, "name-a", &live);
    try testing.expectEqual(@as(u8, 1), (try key.DentryKey.decode(&ins)).collSeq());
    try testing.expectEqual(hash, (try key.DentryKey.decode(&ins)).hash54());
}

test "dentry insert key: name too long refuses; a full chain of 256 is the counted clean overflow" {
    var tx = KvTx.init(testing.allocator, @src());
    defer tx.deinit();
    const long: [256]u8 = @splat('n');
    const nk1: key.NameKey = .init(1);
    try testing.expectError(error.NameTooLong, dentryInsertKey(&tx, &nk1, 1, &long, &.{}));

    const seed: key.NameKey = .init(5);
    const hash = key.dentryNameHash54("crowded", &seed);
    var keys: [256][key.DENTRY_KEY_LEN]u8 = undefined;
    var live: [256][]const u8 = undefined;
    for (0..256) |i| {
        keys[i] = key.dentryKey(1, hash, @intCast(i));
        live[i] = &keys[i];
    }
    const before = key.stats.meta_kv_dentry_collision_overflows.load();
    try testing.expectError(error.DentryChainOverflow, dentryInsertKey(&tx, &seed, 1, "crowded", &live));
    try testing.expectEqual(before + 1, key.stats.meta_kv_dentry_collision_overflows.load());
    // One staged Delete reopens exactly that slot.
    try tx.stageDelete(key.TREE_DENTRIES, &keys[200]);
    const ins = try dentryInsertKey(&tx, &seed, 1, "crowded", &live);
    try testing.expectEqual(@as(u8, 200), (try key.DentryKey.decode(&ins)).collSeq());
}

test "undo pre-images project to their compensating records: live ⇒ Put(pre), tombstone/absent ⇒ Delete" {
    const u_live = UndoKey{ .tree_id = key.TREE_INODES, .key = &key.inodeKey(1), .pre = .{ .live = "before" } };
    const c = u_live.compensation();
    try testing.expectEqual(RecordKind.put, c.kind);
    try testing.expectEqualStrings("before", c.value);
    const u_tomb = UndoKey{ .tree_id = key.TREE_INODES, .key = &key.inodeKey(1), .pre = .tombstone };
    try testing.expectEqual(RecordKind.delete, u_tomb.compensation().kind);
    try testing.expectEqual(@as(usize, 0), u_tomb.compensation().value.len);
    const u_abs = UndoKey{ .tree_id = key.TREE_INODES, .key = &key.inodeKey(1), .pre = .absent };
    try testing.expectEqual(RecordKind.delete, u_abs.compensation().kind);
}

test "an empty tx builds an empty entry of exactly the header length (the degenerate group member)" {
    var tx = KvTx.init(testing.allocator, @src());
    var q = try buildQueuedTx(&tx, 64 * 1024, null);
    defer q.deinit();
    try testing.expectEqual(@as(usize, 0), q.recs.len);
    try testing.expectEqual(journal.ENTRY_HDR_LEN, q.len);
    try testing.expectEqual(AdmissionClass.user, q.class);
    try testing.expect(!q.token_quiet);
}
