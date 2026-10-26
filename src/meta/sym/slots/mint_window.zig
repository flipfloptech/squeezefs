//! The per-stream MINT WINDOW (charter §5.5 "Slot acquisition and
//! minting", amended 2026-10-26; decision 0003 §BV;
//! `docs/evidence/rung-10-create-path-8.md`): beside another minting
//! stream, a rotor mint takes the right edge of ONE rotor tree for a
//! bounded run of mints, and no other stream of the mount takes that
//! tree's window meanwhile, so concurrent creates append to different
//! trees' rightmost leaves. A STREAM is the FUSE caller (the request's
//! pid: one rank, one shell — whichever drain worker serves it) or, for a
//! mint with no caller, the calling thread. A lone stream (no other
//! stream's census on the plane within one window age) keeps the census's
//! per-mint spread. PORTABLE: atomics and arithmetic; the plane
//! (`plane.zig`) owns the census and the lease reads.
//!
//! Why the window is a TREE and not an ino range inside one tree: a slot
//! tree is ordered by ino, so every fresh range — whatever its offset —
//! lies above the tree's last key, in its rightmost leaf, until splits cut
//! that leaf between the ranges; and the headroom law moves the rotor's
//! mints to another tree at the first split (its node count is the law's
//! unit). Concurrent ranges in one tree therefore share the hot leaf for
//! as long as they share the tree (decision 0003 §BD.2 measured it: no
//! change). A tree per window separates them from the first mint. Why a
//! caller and not a thread: a rank the scheduler moves between CPUs is
//! served by another queue's worker, and two ranks on one CPU by one —
//! a thread's window then holds two ranks' creates in flight in one tree.
//!
//! The window reserves NO inos: each mint is the slot cursor's one
//! `fetch_add` as before (inos stay dense; nothing to return or skip; the
//! watermark, its recovery and `statfs` are untouched). What the window
//! adds is the claim (one word per slot: the claiming stream's token and
//! the claim's expiry); the recheck that keeps an ino only while its slot
//! still stands at the decision's generation (`plane.mintStands`) guards
//! window and census mints alike (modelled: `mint_window_model.
//! MintRecheck`; the claim's CAS: `WindowClaim`).

const std = @import("std");
const Allocator = std.mem.Allocator;
const counters = @import("../../../core/counters.zig");
const ledger = @import("../../ledger.zig");
const node = @import("../../node.zig");
const record = @import("../../record.zig");
const key = @import("../../key.zig");
const derive = @import("../../../core/derive.zig");
const jc = @import("../../journal_core.zig");
const lt = @import("lease_table.zig");

const assert = std.debug.assert;

pub const stats = struct {
    /// Windows a thread claimed (the census ran, the tree was free).
    pub var mint_window_grants: counters.ShardedCounter = .{};
    /// Rotor mints served from a standing window (no census).
    pub var mint_window_mints: counters.ShardedCounter = .{};
    /// Of the grants: the claimed tree's word held an expired claim.
    pub var mint_window_steals: counters.ShardedCounter = .{};
    /// The census found every rotor tree claimed by another thread: the
    /// mint took the headroom law's pick unclaimed (shared).
    pub var mint_window_shared: counters.ShardedCounter = .{};
    /// The claim's CAS lost to another thread's (the mint goes ahead
    /// in the tree it chose, with no window).
    pub var mint_window_claim_races: counters.ShardedCounter = .{};
};

/// The mints one window serves before the census re-judges the rotor:
/// the inode records that grow an append-ordered tree by ONE node — the
/// headroom law's unit (`acquire.mintChoice` counts trees in nodes). The
/// rightmost leaf splits at its byte midpoint (`node.splitCut`), so each
/// split leaves half a fold capacity behind and the edge leaf refills
/// from half: one new node per `foldCapacity / 2` bytes of appended
/// inode records.
pub fn windowMints(node_size: usize) u32 {
    const fold = (node.NodeLayout{ .node_size = node_size }).foldCapacity();
    return @intCast(fold / 2 / INODE_RECORD_LEN);
}

/// One inode record as a leaf stores it: the record header, the 9-byte
/// key, the packed inode.
pub const INODE_RECORD_LEN: usize = record.RECORD_HEADER_LEN + key.INODE_KEY_LEN + record.INODE_VALUE_LEN;

/// A window's lifetime bound, and the age past which another thread may
/// take an idle thread's claim: the headroom law's input (a tree's node
/// count) moves at writeback, which runs within one checkpoint age —
/// re-judging sooner reads the same census, later lets a window's tree
/// grow past the balance the law keeps by more than one writeback.
pub fn windowAgeNs() u64 {
    return ledger.CHECKPOINT_MAX_AGE_MS * std.time.ns_per_ms;
}

// ---------------------------------------------------------------------------
// The claim words.
// ---------------------------------------------------------------------------

/// `token u24 ‖ expires_ms u40` (monotonic ms: 2^40 ms is 34 years of
/// uptime); 0 = free.
const EXPIRY_BITS: u6 = 40;
const EXPIRY_MASK: u64 = (1 << EXPIRY_BITS) - 1;
/// The token space: 24 bits, 0 reserved for "free". A caller's token is
/// its pid under the top bit (Linux's `pid_max` ceiling is 2^22, so a pid
/// fits the 23 bits below it); a thread's counts in the half below.
const TOKEN_MASK: u32 = (1 << 24) - 1;
const CALLER_BIT: u32 = 1 << 23;
const PID_MASK: u32 = CALLER_BIT - 1;

pub fn packClaim(token: u32, expires_ms: u64) u64 {
    assert(token != 0 and token <= TOKEN_MASK);
    return (@as(u64, token) << EXPIRY_BITS) | (expires_ms & EXPIRY_MASK);
}

fn claimToken(word: u64) u32 {
    return @intCast(word >> EXPIRY_BITS);
}

fn claimLive(word: u64, now_ms: u64) bool {
    return word != 0 and (now_ms & EXPIRY_MASK) < (word & EXPIRY_MASK);
}

/// One word per routing slot (512 KiB per volume, beside the stamp's
/// cell table of the same width).
pub const Claims = struct {
    words: []std.atomic.Value(u64),

    pub fn init(gpa: Allocator) Allocator.Error!Claims {
        const words = try gpa.alloc(std.atomic.Value(u64), lt.W);
        for (words) |*w| w.* = .init(0);
        return .{ .words = words };
    }

    pub fn deinit(self: *Claims, gpa: Allocator) void {
        gpa.free(self.words);
        self.* = undefined;
    }

    /// A live claim on `slot`, whoever's.
    pub fn live(self: *const Claims, slot: u16, now_ms: u64) bool {
        return claimLive(self.words[slot].load(.monotonic), now_ms);
    }

    /// A live claim of another thread on `slot`.
    pub fn heldByOther(self: *const Claims, slot: u16, token: u32, now_ms: u64) bool {
        const w = self.words[slot].load(.monotonic);
        return claimLive(w, now_ms) and claimToken(w) != token;
    }

    /// Claim `slot` until `expires_ms`: one CAS from the word read — free,
    /// expired, or this thread's own (a re-claim). Null = another thread's
    /// live claim, or the CAS lost (counted).
    pub fn tryClaim(self: *Claims, slot: u16, token: u32, now_ms: u64, expires_ms: u64) ?u64 {
        const cur = self.words[slot].load(.monotonic);
        if (claimLive(cur, now_ms) and claimToken(cur) != token) return null;
        const mine = packClaim(token, expires_ms);
        // acq_rel: the claim orders nothing the mint relies on (the
        // recheck against the lease word does); it publishes the owner.
        if (self.words[slot].cmpxchgStrong(cur, mine, .acq_rel, .monotonic) != null) {
            stats.mint_window_claim_races.inc();
            return null;
        }
        if (cur != 0 and claimToken(cur) != token) stats.mint_window_steals.inc();
        return mine;
    }

    /// The claim `word` still stands on `slot` (no thread took it over).
    pub fn stillMine(self: *const Claims, slot: u16, word: u64) bool {
        return self.words[slot].load(.monotonic) == word;
    }

    /// Give `slot` back iff `word` is still the claim on it.
    pub fn release(self: *Claims, slot: u16, word: u64) void {
        _ = self.words[slot].cmpxchgStrong(word, 0, .acq_rel, .monotonic);
    }
};

/// The last census mint on a plane: its thread and the end of its window
/// age, in the claim word's encoding. A census that finds ANOTHER thread
/// here, live, is concurrent and opens a window; a lone thread never does.
pub const Recent = struct {
    word: std.atomic.Value(u64) = .init(0),

    pub fn otherLive(self: *const Recent, token: u32, now_ms: u64) bool {
        const w = self.word.load(.monotonic);
        return claimLive(w, now_ms) and claimToken(w) != token;
    }

    pub fn note(self: *Recent, token: u32, now_ms: u64, age_ms: u64) void {
        self.word.store(packClaim(token, now_ms + age_ms), .monotonic);
    }
};

// ---------------------------------------------------------------------------
// The streams' windows.
// ---------------------------------------------------------------------------

/// One stream's standing window.
pub const Window = struct {
    slot: u16 = 0,
    /// The lease generation the window was granted under.
    g: u32 = 0,
    /// The claim word this stream stored (0 = no window).
    claim: u64 = 0,
    /// Mints left.
    left: u32 = 0,
    expires_ns: u64 = 0,
};

var next_thread_token: std.atomic.Value(u32) = .init(0);
threadlocal var tl_token: u32 = 0;

/// The minting stream's token: the FUSE caller's pid, else (`pid` 0: a
/// mint no caller issued) the calling thread's. A thread token wraps
/// after 2^23 − 1 threads and may then alias another thread's: a shared
/// tree, never a wrong ino — the claim orders no mint.
pub fn minterToken(pid: u32) u32 {
    if (pid != 0) return CALLER_BIT | (pid & PID_MASK);
    if (tl_token == 0) tl_token = next_thread_token.fetchAdd(1, .monotonic) % PID_MASK + 1;
    return tl_token;
}

/// A set's ways: a stream hashes to one set and takes a way of it.
pub const WAYS: usize = 4;

/// The sets per plane: `nextPow2(4 × cpus)` — the streams minting at once
/// are the callers in flight, about one per CPU and two on an
/// oversubscribed run, so 16 ways per CPU keep a set from overflowing its
/// four (an overflowing stream loses its window to the next census — a
/// shared tree, never a wrong ino).
pub fn setsFor(cpus: u64) usize {
    return @intCast(derive.nextPow2(4 * @max(cpus, 1)));
}

pub const Windows = struct {
    sets: []Set,

    pub const Entry = struct { token: u32 = 0, w: Window = .{} };

    pub const Set = struct {
        lock: jc.SpinLock = .{},
        ways: [WAYS]Entry = @splat(.{}),

        /// The stream's entry, if it holds one.
        pub fn find(self: *Set, token: u32) ?*Entry {
            for (&self.ways) |*e| if (e.token == token) return e;
            return null;
        }

        /// The stream's entry, else a free way, else the way whose window
        /// expires first (its claim lapses at its expiry).
        pub fn take(self: *Set, token: u32) *Entry {
            if (self.find(token)) |e| return e;
            var victim = &self.ways[0];
            for (&self.ways) |*e| {
                if (e.token == 0) {
                    victim = e;
                    break;
                }
                if (e.w.expires_ns < victim.w.expires_ns) victim = e;
            }
            victim.* = .{ .token = token };
            return victim;
        }
    };

    pub fn init(gpa: Allocator, cpus: u64) Allocator.Error!Windows {
        const sets = try gpa.alloc(Set, setsFor(cpus));
        for (sets) |*st| st.* = .{};
        return .{ .sets = sets };
    }

    pub fn deinit(self: *Windows, gpa: Allocator) void {
        gpa.free(self.sets);
        self.* = undefined;
    }

    pub fn setOf(self: *Windows, token: u32) *Set {
        // Fibonacci hashing: the pids of one job are consecutive.
        const h = token *% 0x9E37_79B9;
        return &self.sets[h % self.sets.len];
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "window derivations: one node of inode records per window (half a fold capacity — the midpoint split's refill — over the record's bytes), one checkpoint age per window" {
    // A leaf's inode record: 16 B header + 9 B key + 68 B inode = 93 B.
    try testing.expectEqual(@as(usize, 93), INODE_RECORD_LEN);
    const fold256 = 256 * 1024 - 4096 - node.FRAME_LEN - 32;
    try testing.expectEqual(fold256, (node.NodeLayout{ .node_size = 256 * 1024 }).foldCapacity());
    try testing.expectEqual(@as(u32, @intCast(fold256 / 2 / 93)), windowMints(256 * 1024));
    try testing.expect(windowMints(256 * 1024) > 1300 and windowMints(256 * 1024) < 1400);
    // The format's node range scales it linearly (64 KiB … 1 MiB).
    try testing.expect(windowMints(64 * 1024) < windowMints(256 * 1024));
    try testing.expect(windowMints(1024 * 1024) > 4 * windowMints(256 * 1024) - 64);
    try testing.expectEqual(@as(u64, 1000) * std.time.ns_per_ms, windowAgeNs());
}

test "claims: a free word claims; another thread's live claim refuses; its expired claim is taken over (counted); a re-claim of one's own; stillMine and release by the exact word" {
    var c = try Claims.init(testing.allocator);
    defer c.deinit(testing.allocator);
    const a: u32 = 7;
    const b: u32 = 9;
    const w = c.tryClaim(3, a, 100, 1100).?;
    try testing.expect(c.stillMine(3, w));
    try testing.expect(c.live(3, 500) and !c.live(3, 1100) and !c.live(4, 500));
    try testing.expect(c.heldByOther(3, b, 500));
    try testing.expect(!c.heldByOther(3, a, 500));
    try testing.expectEqual(@as(?u64, null), c.tryClaim(3, b, 500, 1500));
    // Expired at 1100: B takes it over.
    const s0 = stats.mint_window_steals.load();
    try testing.expect(!c.heldByOther(3, b, 1100));
    const wb = c.tryClaim(3, b, 1100, 2100).?;
    try testing.expectEqual(s0 + 1, stats.mint_window_steals.load());
    try testing.expect(!c.stillMine(3, w));
    // A's release of its stale word leaves B's claim standing.
    c.release(3, w);
    try testing.expect(c.stillMine(3, wb));
    // B re-claims its own (a window refill on the same tree): no steal.
    const wb2 = c.tryClaim(3, b, 1500, 2500).?;
    try testing.expectEqual(s0 + 1, stats.mint_window_steals.load());
    c.release(3, wb2);
    try testing.expect(!c.heldByOther(3, a, 1500));
    try testing.expectEqual(@as(u64, 0), c.words[3].load(.monotonic));
}

test "recent: another thread's live census reads concurrent, one's own or an expired one does not" {
    var r: Recent = .{};
    try testing.expect(!r.otherLive(1, 0));
    r.note(1, 100, 1000);
    try testing.expect(!r.otherLive(1, 500));
    try testing.expect(r.otherLive(2, 500));
    try testing.expect(!r.otherLive(2, 1100));
}

test "streams: a caller's token is its pid under the caller bit, a thread's is its own and stable, the namespaces never meet; a set finds a stream's way, takes a free one, then evicts the soonest expiry" {
    try testing.expectEqual(CALLER_BIT | 4242, minterToken(4242));
    try testing.expectEqual(CALLER_BIT | (4_194_304 & PID_MASK), minterToken(4_194_304));
    const t0 = minterToken(0);
    try testing.expectEqual(t0, minterToken(0));
    try testing.expect(t0 & CALLER_BIT == 0 and t0 != 0);
    var other: u32 = 0;
    const th = try std.Thread.spawn(.{}, struct {
        fn run(out: *u32) void {
            out.* = minterToken(0);
        }
    }.run, .{&other});
    th.join();
    try testing.expect(other != 0 and other != t0 and other & CALLER_BIT == 0);
    try testing.expectEqual(@as(usize, 128), setsFor(32));
    try testing.expectEqual(@as(usize, 4), setsFor(1));
    try testing.expectEqual(@as(usize, 4), setsFor(0));
    var ws = try Windows.init(testing.allocator, 1);
    defer ws.deinit(testing.allocator);
    const st = ws.setOf(7);
    try testing.expectEqual(@as(?*Windows.Entry, null), st.find(7));
    for (0..WAYS) |i| {
        const e = st.take(@intCast(100 + i));
        e.w.expires_ns = 1000 + i;
    }
    try testing.expectEqual(@as(u64, 1002), st.find(102).?.w.expires_ns);
    // A fifth stream evicts the soonest expiry (100's), emptied for it.
    const e5 = st.take(200);
    try testing.expectEqual(@as(u64, 0), e5.w.expires_ns);
    try testing.expectEqual(@as(?*Windows.Entry, null), st.find(100));
    try testing.expect(st.find(101) != null and st.find(200) != null);
}
