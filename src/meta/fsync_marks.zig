//! What an fsync of one inode must make durable, per metadata member
//! (charter §5.2 "The ack point", §6.4; decision 0003 §CH). PORTABLE.
//!
//! Every user transaction lands on ONE member (a span of two is a cross-
//! owner intent, barriered on every member before its op returns —
//! `sym/xv/ops.zig`). A member's conveyor keeps, per inode stripe, the
//! highest padded window end of an acked entry that names an inode of the
//! stripe (`Marks.stamp`, by the committer after its outcome, before its
//! answer leaves): the entry's records keyed by the inode (inode, xattr —
//! the layout, the symlink —, block map, block refs by owner) and the
//! dentry `Put`s naming it as the child (a create, a link, a rename's new
//! name; a same-member unlink or rename-away stages the child's own inode
//! record beside its `Delete`). Beside the stripes, two words every inode
//! reads: `chain` — the end of the last window holding a directory's
//! dentry `Put` (a mkdir, a directory's rename: the dentry chain any file's
//! name hangs from), stamped once per window by the pass — and `overflow`
//! — an entry naming more inodes than `Deps` holds.
//!
//! An fsync of inode `i` needs, on member `m`, a barrier covering
//! `need(i) = max(stripe(i), chain, overflow)` — nothing if the member's
//! durable floor is already there. The stripes are SHARED (a hash of the
//! inode) and that is correct: a false share asks one barrier a member did
//! not owe, never skips one it did.

const std = @import("std");
const counters = @import("../core/counters.zig");
const dlm = @import("dlm.zig");
const key = @import("key.zig");
const tx = @import("tx.zig");
const economy = @import("economy.zig");
const route_table = @import("routing/route_table.zig");

const Atomic = std.atomic.Value;
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

/// The dentry type byte of a directory (`economy.ftByte`).
const DT_DIR: u8 = economy.ftByte(economy.S.IFDIR);

pub const stats = struct {
    /// fsync/fdatasync of one inode through the set (`MetaSet.fsyncIno`).
    pub var meta_fsync_ino_calls: counters.ShardedCounter = .{};
    /// Every-member durability points (fsyncdir, syncfs, the cross-owner
    /// intents' barriers, the freed-block hold's drain).
    pub var meta_fsync_set_calls: counters.ShardedCounter = .{};
    /// Barrier requests the calls made, one per member asked.
    pub var meta_fsync_member_requests: counters.ShardedCounter = .{};
    /// Members a call asked nothing of: what it needed was durable.
    pub var meta_fsync_member_skips: counters.ShardedCounter = .{};
    /// Per-inode requests by what the member held: the inode's own stripe
    /// un-durable (`own`); only the chain or the overflow word (`chain`);
    /// nothing of the inode's at all (`none` — the every-member control
    /// arm's waste; 0 under the shipped scope).
    pub var meta_fsync_requests_own: counters.ShardedCounter = .{};
    pub var meta_fsync_requests_chain: counters.ShardedCounter = .{};
    pub var meta_fsync_requests_none: counters.ShardedCounter = .{};
    /// Entries naming more inodes than `Deps.MAX`: stamped on every inode.
    pub var meta_fsync_marks_overflows: counters.ShardedCounter = .{};
    /// The set's durability wait per call, exact-sum (per-inode and
    /// every-member calls apart).
    pub var meta_fsync_ino_ns: counters.ShardedLatencyHistogram = .{};
    pub var meta_fsync_set_ns: counters.ShardedLatencyHistogram = .{};
};

/// The inodes one transaction names, read off its staged records before
/// the commit consumes them.
pub const Deps = struct {
    inos: [MAX]u64 = undefined,
    n: u8 = 0,
    overflow: bool = false,
    /// A dentry `Put` of a directory: the window is the chain's.
    chain: bool = false,

    /// A namespace op names ≤ 4 inodes (a rename's two parents, the moved
    /// inode, the victim), the times drain ≤ 16 per entry; 32 leaves the
    /// layout publish's aggregation room, past it the entry is everyone's.
    pub const MAX = 32;

    pub fn of(t: *const tx.KvTx) Deps {
        var d: Deps = .{};
        // A forgotten corpse's destroy: no name reaches it, no kernel
        // holds it — no fsync can ask for it (`KvTx.leafwise`).
        if (t.leafwise) return d;
        for (t.staged.items) |s| d.note(s);
        return d;
    }

    fn note(d: *Deps, s: tx.Staged) void {
        switch (s.tree_id) {
            key.TREE_INODES, key.TREE_XATTRS, key.TREE_BLOCK_MAP => if (s.key.len >= 8) d.add(std.mem.readInt(u64, s.key[0..8], .big)),
            key.TREE_BLOCK_REFS => if (s.key.len >= key.BLOCK_REF_OWNER_OFF + 8) d.add(std.mem.readInt(u64, s.key[key.BLOCK_REF_OWNER_OFF..][0..8], .big)),
            key.TREE_DENTRIES => {
                if (s.kind != .put) return;
                // `child_ino u64 LE ‖ file_type u8 ‖ …` (record.DentryValue).
                if (s.value.len < 9) return d.setOverflow();
                const child = std.mem.readInt(u64, s.value[0..8], .little);
                if (s.value[8] == DT_DIR) d.chain = true;
                if (!route_table.isRoutable(child)) return d.setOverflow();
                d.add(route_table.effectiveIno(child));
            },
            else => {},
        }
    }

    fn add(d: *Deps, ino: u64) void {
        if (d.overflow) return;
        for (d.inos[0..d.n]) |x| if (x == ino) return;
        if (d.n == MAX) return d.setOverflow();
        d.inos[d.n] = ino;
        d.n += 1;
    }

    fn setOverflow(d: *Deps) void {
        d.overflow = true;
    }

    pub fn slice(d: *const Deps) []const u64 {
        return d.inos[0..d.n];
    }
};

pub const Marks = struct {
    words: []Atomic(u64),
    mask: u64,
    chain: Atomic(u64) = .init(0),
    overflow: Atomic(u64) = .init(0),

    /// One word per routing slot's worth (W = 2^16): a false share asks one
    /// extra barrier (the every-member law's cost) with probability
    /// (un-durable inodes on the member ÷ W) — under 1 % up to 650 inodes
    /// changed per flush interval; 512 KiB per member beside a node cache
    /// of ≥ 512 MiB.
    pub const WIDTH: usize = route_table.W;

    pub fn init(gpa: Allocator, width: usize) Allocator.Error!Marks {
        assert(std.math.isPowerOfTwo(width));
        const words = try gpa.alloc(Atomic(u64), width);
        @memset(words, .init(0));
        return .{ .words = words, .mask = width - 1 };
    }

    pub fn deinit(self: *Marks, gpa: Allocator) void {
        gpa.free(self.words);
        self.* = undefined;
    }

    fn word(self: *Marks, ino: u64) *Atomic(u64) {
        return &self.words[@intCast(dlm.splitmix64(ino) & self.mask)];
    }

    /// The committer's stamp: `end` is its window's padded end. Called
    /// after the outcome and BEFORE the commit returns, so an fsync ordered
    /// after the op's answer reads it (program order, then the reply).
    pub fn stamp(self: *Marks, d: *const Deps, end: u64) void {
        if (end == 0) return;
        if (d.overflow) {
            stats.meta_fsync_marks_overflows.inc();
            raise(&self.overflow, end);
            return;
        }
        for (d.slice()) |ino| raise(self.word(ino), end);
    }

    /// The pass's stamp of a window holding a directory's dentry `Put`
    /// (one RMW per window, never per committer).
    pub fn stampChain(self: *Marks, end: u64) void {
        raise(&self.chain, end);
    }

    pub const Need = struct {
        /// The inode's own stripe.
        own: u64,
        /// `max(own, chain, overflow)`: what a barrier must cover.
        all: u64,
    };

    /// What an fsync of `ino` needs covered on this member.
    pub fn need(self: *Marks, ino: u64) Need {
        // Acquire pairs with `raise`'s release CAS: a word read at the
        // stamp's value sees the stamp's window as reserved.
        const own = self.word(ino).load(.acquire);
        return .{ .own = own, .all = @max(own, @max(self.chain.load(.acquire), self.overflow.load(.acquire))) };
    }
};

/// Monotone max: a load first (the common already-covered case writes
/// nothing), then a release CAS loop.
fn raise(w: *Atomic(u64), v: u64) void {
    var cur = w.load(.monotonic);
    while (cur < v) cur = w.cmpxchgWeak(cur, v, .release, .monotonic) orelse return;
}

// ---------------------------------------------------------------------------
// Tests (portable).
// ---------------------------------------------------------------------------

test {
    // The join and the coverage, explored (`FsyncJoin` and its weakenings).
    _ = @import("fsync_join_model.zig");
}

const testing = std.testing;
const record = @import("record.zig");

fn stagedTx(gpa: Allocator) !*tx.KvTx {
    const t = try gpa.create(tx.KvTx);
    t.* = tx.KvTx.init(gpa, @src());
    return t;
}

fn freeTx(gpa: Allocator, t: *tx.KvTx) void {
    t.deinit();
    gpa.destroy(t);
}

test "Deps: an entry names the inodes its records are keyed by and the children its dentry Puts name; a directory's Put is the chain's; a corpse's destroy names nothing" {
    const gpa = testing.allocator;
    const child = key.guestLocalIno(3, 5);
    const child_global = route_table.globalOfEffective(child);
    const parent: u64 = 1;
    {
        const t = try stagedTx(gpa);
        defer freeTx(gpa, t);
        try t.stagePut(key.TREE_INODES, &key.inodeKey(child), "x");
        var buf: [64]u8 = undefined;
        const dv = try (record.DentryValue{ .child_ino = child_global, .file_type = economy.ftByte(economy.S.IFREG), .name = "f" }).encodeInto(&buf);
        try t.stagePut(key.TREE_DENTRIES, &key.dentryKey(parent, 7, 0), dv);
        try t.stagePut(key.TREE_INODES, &key.inodeKey(child), "y");
        const d = Deps.of(t);
        try testing.expectEqualSlices(u64, &.{child}, d.slice());
        try testing.expect(!d.chain and !d.overflow);
    }
    {
        const t = try stagedTx(gpa);
        defer freeTx(gpa, t);
        var buf: [64]u8 = undefined;
        const dv = try (record.DentryValue{ .child_ino = child_global, .file_type = DT_DIR, .name = "d" }).encodeInto(&buf);
        try t.stagePut(key.TREE_DENTRIES, &key.dentryKey(parent, 9, 0), dv);
        try t.stagePut(key.TREE_INODES, &key.inodeKey(parent), "p");
        const d = Deps.of(t);
        try testing.expectEqualSlices(u64, &.{ child, parent }, d.slice());
        try testing.expect(d.chain);
    }
    {
        const t = try stagedTx(gpa);
        defer freeTx(gpa, t);
        t.leafwise = true;
        try t.stagePut(key.TREE_INODES, &key.inodeKey(child), "x");
        const d = Deps.of(t);
        try testing.expectEqual(@as(u8, 0), d.n);
    }
    {
        const t = try stagedTx(gpa);
        defer freeTx(gpa, t);
        var i: u64 = 0;
        while (i < Deps.MAX + 1) : (i += 1) try t.stagePut(key.TREE_INODES, &key.inodeKey(100 + i), "x");
        const d = Deps.of(t);
        try testing.expect(d.overflow);
    }
}

test "Marks: an inode needs its own stripe's latest window, the chain and the overflow; another stripe's stamp is not its" {
    const gpa = testing.allocator;
    var m = try Marks.init(gpa, 1024);
    defer m.deinit(gpa);
    var d: Deps = .{};
    d.add(40);
    m.stamp(&d, 4096);
    try testing.expectEqual(Marks.Need{ .own = 4096, .all = 4096 }, m.need(40));
    // A stamp never moves a word back.
    m.stamp(&d, 1024);
    try testing.expectEqual(@as(u64, 4096), m.need(40).own);
    // Another inode in another stripe needs nothing of it.
    var other: u64 = 41;
    while (dlm.splitmix64(other) & m.mask == dlm.splitmix64(40) & m.mask) other += 1;
    try testing.expectEqual(Marks.Need{ .own = 0, .all = 0 }, m.need(other));
    m.stampChain(8192);
    try testing.expectEqual(Marks.Need{ .own = 0, .all = 8192 }, m.need(other));
    var wide: Deps = .{};
    wide.setOverflow();
    m.stamp(&wide, 12288);
    try testing.expectEqual(Marks.Need{ .own = 4096, .all = 12288 }, m.need(40));
}
