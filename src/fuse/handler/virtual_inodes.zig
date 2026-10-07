//! The root's virtual control inodes `.stats` / `.config` / `.trace`
//! (charter §10; `docs/porting/stats_cli.md` §1.1–§1.3, §7 Q3/Q15/Q16):
//! ONE serve mechanism — per-lookup GENERATIONS.
//!
//! `lookup(1, name)` renders the payload ONCE into a fresh generation,
//! mints a generation ino and replies `{entry_valid = 0, attr_valid = 0,
//! attr.size = len}`. The generation is immutable, so a kernel that owns
//! `i_size` under the writeback cache (`FUSE_WRITEBACK_CACHE` discards the
//! size of every later attr reply — the reference's measured torn-JSON
//! class: 71352 → 71350 → 71349 replies against a frozen 71352) is simply
//! right: the size it learned IS the served bytes, forever, for that ino.
//! TTL 0 ⇒ every path walk revalidates ⇒ a fresh generation per consumer
//! and a prompt FORGET.
//!
//! THE GENERATION IS THE PIN: a generation holds one reference for the
//! kernel (dropped by FORGET), one for the FIFO ring (dropped when the ring
//! evicts it) and one per open fh (OPEN/RELEASE); the payload frees at zero
//! and a retired generation answers `ESTALE`. There is no second table.
//!
//! Visibility: lookup-only — never listed by readdir (fstests generic/062:
//! a recursive walk must not see fabricated files); the two reserved
//! readdir cookies ride above the real cookie space and resume as an empty
//! page. Mode 0400 + the mount uid (`-o default_permissions` is the shipped
//! posture, so the kernel enforces it): the payload names every cached key,
//! every backing path and every co-tenant's I/O.
//!
//! Portable and store-independent (a plain registry over owned byte
//! slices); `kv_fs` binds it and the renderer that fills it.

const std = @import("std");
const Allocator = std.mem.Allocator;
const counters = @import("../../core/counters.zig");
const jc = @import("../../meta/journal_core.zig");

/// The class in the ino's low bits, so FORGET/GETATTR on a retired
/// generation still knows which file it was.
pub const Class = enum(u2) {
    stats = 0,
    config = 1,
    trace = 2,

    pub fn name(self: Class) []const u8 {
        return switch (self) {
            .stats => STATS_NAME,
            .config => CONFIG_NAME,
            .trace => TRACE_NAME,
        };
    }
};

pub const STATS_NAME = ".stats";
pub const CONFIG_NAME = ".config";
pub const TRACE_NAME = ".trace";

pub fn classOfName(name: []const u8) ?Class {
    if (std.mem.eql(u8, name, STATS_NAME)) return .stats;
    if (std.mem.eql(u8, name, CONFIG_NAME)) return .config;
    if (std.mem.eql(u8, name, TRACE_NAME)) return .trace;
    return null;
}

/// 0400 (VAL-7a): the payload is a complete map of the daemon's private
/// state and of every co-tenant's I/O; `0400 + mount uid` is the access
/// boundary and root still reads.
pub const VIRTUAL_INODE_MODE: u32 = 0o400;
/// The generation range `[2⁶⁴ − 2³², …fff0]`: real inos are monotonic from
/// 1 with a cap ≥ 100 M ≪ 2³² (the port's `INO_SPACE` is 2⁵⁶, still
/// below), so the range is disjoint from every real ino forever and below
/// the two canonical inos.
pub const GEN_INO_FIRST: u64 = 0xffff_ffff_0000_0000;
pub const GEN_INO_LAST: u64 = 0xffff_ffff_ffff_fff0;
/// The reference's fixed-ino protocol inos: NOT served (nothing predates
/// the generation scheme here), reserved so a real ino never collides.
pub const CONFIG_INODE: u64 = 0xffff_ffff_ffff_fffe;
pub const STATS_INODE: u64 = 0xffff_ffff_ffff_fffd;
pub const CLASS_BITS: u6 = 2;
/// The generation registry cap: a safety rail against a kernel that never
/// FORGETs — 256 × the field-measured ≈ 72 KiB payload ≈ 18 MiB worst case.
/// A fixed count (not a derived budget) is deliberate: it bounds a KERNEL
/// defect, never a tuning posture. Eviction order = mint order (a FIFO
/// ring: O(1) per mint, never a scan).
pub const REGISTRY_CAP: usize = 256;
/// The reserved readdir cookies: above the real cookie space `[3, 2⁶²+2]`
/// (`key.encodeReaddirCookie`), sign bit clear for the i64 FUSE surface;
/// never emitted, honoured only as a stream terminator on resume.
pub const READDIR_CONFIG_COOKIE: u64 = (1 << 62) + 3;
pub const READDIR_STATS_COOKIE: u64 = (1 << 62) + 4;

/// Is `cookie` one of the reserved virtual cookies (⇒ an empty page)?
pub fn isReservedCookie(cookie: u64) bool {
    return cookie == READDIR_CONFIG_COOKIE or cookie == READDIR_STATS_COOKIE;
}

pub fn isVirtualIno(ino: u64) bool {
    return ino >= GEN_INO_FIRST;
}

/// The class of a generation ino (its low bits).
pub fn classOf(ino: u64) Class {
    return @fromBackingInt(@intCast(@as(u2, @truncate(ino))));
}

/// The sequence encoded by a generation ino (the registry's key).
fn seqOf(ino: u64) u64 {
    return (ino - GEN_INO_FIRST) >> CLASS_BITS;
}

/// `ino = FIRST + (seq % slots) × 4 + class`: a slot every 4 inos over the
/// range; wraps after ≈ 1 G mints per class.
pub const SEQ_SLOTS: u64 = (GEN_INO_LAST - GEN_INO_FIRST) >> CLASS_BITS;

pub fn inoOf(seq: u64, class: Class) u64 {
    return GEN_INO_FIRST + ((seq % SEQ_SLOTS) << CLASS_BITS) + @backingInt(class);
}

/// `blocks = ceil(size / 512)` (`st_blocks` is in 512 B units).
pub fn blocksOf(size: u64) u64 {
    return (size + 511) / 512;
}

pub const stats = struct {
    /// Live generations (the registry population, pinned orphans included).
    pub var stats_generations_live: counters.Gauge = .{};
    /// Ring evictions of a generation the kernel had not yet FORGOTTEN
    /// (≈ 0 under a kernel that FORGETs; the rail's engagement).
    pub var stats_generations_evicted: counters.ShardedCounter = .{};
    /// Reads/opens/getattrs on a retired generation (ESTALE replies).
    pub var stats_generation_stale: counters.ShardedCounter = .{};
};

/// A rendered payload the registry takes ownership of: the capacity the
/// renderer reserved and the bytes it wrote.
pub const Owned = struct {
    buf: []u8,
    len: usize,

    pub fn bytes(self: Owned) []const u8 {
        return self.buf[0..self.len];
    }
};

const Generation = struct {
    seq: u64,
    class: Class,
    payload: Owned,
    /// kernel (1) + ring (1) + opens.
    refs: u32,
    /// Still in the ring (the ring's reference is live).
    ringed: bool,
};

pub const Registry = struct {
    gpa: Allocator,
    lock: jc.SpinLock = .{},
    /// ONE shared minter across every lane: a split counter could mint the
    /// SAME generation ino on two queues.
    next_seq: u64 = 0,
    gens: std.AutoHashMapUnmanaged(u64, *Generation) = .empty,
    /// FIFO ring of the seqs holding a ring reference; `no_seq` = empty slot.
    ring: [REGISTRY_CAP]u64 = @splat(no_seq),

    const no_seq = std.math.maxInt(u64);

    pub fn init(gpa: Allocator) Registry {
        return .{ .gpa = gpa };
    }

    pub fn deinit(self: *Registry) void {
        var it = self.gens.valueIterator();
        while (it.next()) |g| {
            self.gpa.free(g.*.payload.buf);
            self.gpa.destroy(g.*);
        }
        self.gens.deinit(self.gpa);
        stats.stats_generations_live.set(0);
        self.* = undefined;
    }

    /// Mint a generation owning `payload`; the ring's oldest loses its ring
    /// reference when the ring is full (evicted; freed when nothing else
    /// pins it). Returns the generation ino.
    pub fn mint(self: *Registry, class: Class, payload: Owned) Allocator.Error!u64 {
        const g = try self.gpa.create(Generation);
        errdefer self.gpa.destroy(g);
        self.lock.lock();
        defer self.lock.unlock();
        try self.gens.ensureUnusedCapacity(self.gpa, 1);
        const seq = self.next_seq;
        self.next_seq += 1;
        g.* = .{ .seq = seq, .class = class, .payload = payload, .refs = 2, .ringed = true };
        const slot = &self.ring[@intCast(seq % REGISTRY_CAP)];
        if (slot.* != no_seq) {
            if (self.gens.get(slot.*)) |old| {
                old.ringed = false;
                // An eviction the kernel still references is the rail's
                // engagement; a forgotten generation leaving is the norm.
                if (old.refs > 1) stats.stats_generations_evicted.inc();
                self.dropLocked(old);
            }
        }
        slot.* = seq;
        self.gens.putAssumeCapacity(seq, g);
        stats.stats_generations_live.set(self.gens.count());
        return inoOf(seq, class);
    }

    fn findLocked(self: *Registry, ino: u64) ?*Generation {
        const g = self.gens.get(seqOf(ino)) orelse return null;
        if (g.class != classOf(ino)) return null;
        return g;
    }

    /// One reference down; frees at zero (under the lock).
    fn dropLocked(self: *Registry, g: *Generation) void {
        g.refs -= 1;
        if (g.refs != 0) return;
        _ = self.gens.remove(g.seq);
        if (g.ringed) self.ring[@intCast(g.seq % REGISTRY_CAP)] = no_seq;
        self.gpa.free(g.payload.buf);
        self.gpa.destroy(g);
        stats.stats_generations_live.set(self.gens.count());
    }

    /// The frozen size, or null for a retired generation (ESTALE).
    pub fn size(self: *Registry, ino: u64) ?u64 {
        self.lock.lock();
        defer self.lock.unlock();
        const g = self.findLocked(ino) orelse {
            stats.stats_generation_stale.inc();
            return null;
        };
        return g.payload.len;
    }

    /// OPEN: the fh pin (+1); false for a retired generation.
    pub fn pin(self: *Registry, ino: u64) bool {
        self.lock.lock();
        defer self.lock.unlock();
        const g = self.findLocked(ino) orelse {
            stats.stats_generation_stale.inc();
            return false;
        };
        g.refs += 1;
        return true;
    }

    /// RELEASE: the fh pin dropped.
    pub fn unpin(self: *Registry, ino: u64) void {
        self.lock.lock();
        defer self.lock.unlock();
        const g = self.findLocked(ino) orelse return;
        self.dropLocked(g);
    }

    /// FORGET: the kernel's reference dropped.
    pub fn forget(self: *Registry, ino: u64) void {
        self.unpin(ino);
    }

    /// READ: the slice `[off, off + dest.len)` of the frozen bytes copied
    /// into `dest` (empty past EOF); null for a retired generation. The
    /// copy runs under a pin, not under the lock.
    pub fn read(self: *Registry, ino: u64, off: u64, dest: []u8) ?usize {
        if (!self.pin(ino)) return null;
        defer self.unpin(ino);
        // The payload is immutable while pinned: no lock for the copy.
        const g = blk: {
            self.lock.lock();
            defer self.lock.unlock();
            break :blk self.findLocked(ino).?;
        };
        const bytes = g.payload.bytes();
        if (off >= bytes.len) return 0;
        const n = @min(dest.len, bytes.len - @as(usize, @intCast(off)));
        @memcpy(dest[0..n], bytes[@intCast(off)..][0..n]);
        return n;
    }

    pub fn liveCount(self: *Registry) usize {
        self.lock.lock();
        defer self.lock.unlock();
        return self.gens.count();
    }
};

// ---------------------------------------------------------------------------
// Tests: the ino scheme, the ring (mint / evict / ESTALE / class decode),
// the reference law.
// ---------------------------------------------------------------------------

const testing = std.testing;

fn owned(gpa: Allocator, text: []const u8) !Owned {
    const buf = try gpa.alloc(u8, text.len + 8);
    @memcpy(buf[0..text.len], text);
    return .{ .buf = buf, .len = text.len };
}

test "the ino scheme: the range starts at 2^64 − 2^32 and is disjoint from the real ino space and the two canonical inos; the class rides the low 2 bits and survives any registry state" {
    try testing.expectEqual(std.math.maxInt(u64) - (@as(u64, 1 << 32) - 1), GEN_INO_FIRST);
    try testing.expect(GEN_INO_FIRST > (1 << 56)); // INO_SPACE
    try testing.expect(GEN_INO_LAST < STATS_INODE and STATS_INODE < CONFIG_INODE);
    try testing.expect(isVirtualIno(STATS_INODE) and isVirtualIno(GEN_INO_FIRST) and !isVirtualIno(1) and !isVirtualIno((1 << 56) - 1));
    const a = inoOf(0, .stats);
    const b = inoOf(0, .config);
    const c = inoOf(7, .trace);
    try testing.expectEqual(GEN_INO_FIRST, a);
    try testing.expectEqual(GEN_INO_FIRST + 1, b);
    try testing.expectEqual(GEN_INO_FIRST + 7 * 4 + 2, c);
    try testing.expectEqual(Class.stats, classOf(a));
    try testing.expectEqual(Class.config, classOf(b));
    try testing.expectEqual(Class.trace, classOf(c));
    try testing.expectEqual(@as(u64, 7), seqOf(c));
    try testing.expect(inoOf(SEQ_SLOTS - 1, .trace) <= GEN_INO_LAST);
    try testing.expectEqual(inoOf(0, .stats), inoOf(SEQ_SLOTS, .stats));
    try testing.expectEqual(Class.stats, classOfName(".stats").?);
    try testing.expectEqual(Class.trace, classOfName(".trace").?);
    try testing.expect(classOfName("stats") == null and classOfName(".statsx") == null);
    try testing.expectEqual(@as(u64, 0), blocksOf(0));
    try testing.expectEqual(@as(u64, 1), blocksOf(1));
    try testing.expectEqual(@as(u64, 1), blocksOf(512));
    try testing.expectEqual(@as(u64, 2), blocksOf(513));
    try testing.expect(isReservedCookie(READDIR_STATS_COOKIE) and isReservedCookie(READDIR_CONFIG_COOKIE) and !isReservedCookie(3));
    try testing.expect(READDIR_CONFIG_COOKIE > (1 << 62) + 2 and READDIR_STATS_COOKIE < (1 << 63));
}

test "two lookups mint two distinct frozen generations; reads slice the snapshot; the size is exact; the kernel ref + the fh pin + the ring ref each hold the payload" {
    var r = Registry.init(testing.allocator);
    defer r.deinit();
    const a = try r.mint(.stats, try owned(testing.allocator, "{\"a\":1}\n"));
    const b = try r.mint(.stats, try owned(testing.allocator, "{\"a\":22}\n"));
    try testing.expect(a != b);
    try testing.expectEqual(@as(?u64, 8), r.size(a));
    try testing.expectEqual(@as(?u64, 9), r.size(b));
    var buf: [16]u8 = undefined;
    try testing.expectEqual(@as(?usize, 8), r.read(a, 0, &buf));
    try testing.expectEqualStrings("{\"a\":1}\n", buf[0..8]);
    try testing.expectEqual(@as(?usize, 3), r.read(b, 6, &buf));
    try testing.expectEqualStrings("2}\n", buf[0..3]);
    try testing.expectEqual(@as(?usize, 0), r.read(b, 9, &buf));
    try testing.expectEqual(@as(?usize, 0), r.read(b, 100, &buf));
    // OPEN pins; FORGET alone does not free; RELEASE then ring eviction does.
    try testing.expect(r.pin(a));
    r.forget(a);
    try testing.expectEqual(@as(?u64, 8), r.size(a));
    r.unpin(a);
    try testing.expectEqual(@as(?u64, 8), r.size(a)); // the ring still holds it
    try testing.expectEqual(@as(usize, 2), r.liveCount());
}

test "the FIFO ring evicts in mint order at the cap; a retired generation is ESTALE for size/pin/read; a pinned evictee survives its eviction until its last reference" {
    var r = Registry.init(testing.allocator);
    defer r.deinit();
    const evicted0 = stats.stats_generations_evicted.load();
    var first: u64 = 0;
    var i: usize = 0;
    while (i < REGISTRY_CAP) : (i += 1) {
        const ino = try r.mint(.config, try owned(testing.allocator, "x"));
        if (i == 0) first = ino;
        r.forget(ino); // the kernel forgot every one: the ring alone holds them
    }
    try testing.expectEqual(REGISTRY_CAP, r.liveCount());
    try testing.expectEqual(@as(?u64, 1), r.size(first));
    // The 257th mint evicts the first (mint order), which frees it (no other ref).
    const n = try r.mint(.config, try owned(testing.allocator, "yy"));
    try testing.expectEqual(REGISTRY_CAP, r.liveCount());
    try testing.expect(r.size(first) == null);
    try testing.expect(!r.pin(first));
    var buf: [4]u8 = undefined;
    try testing.expect(r.read(first, 0, &buf) == null);
    try testing.expectEqual(@as(?u64, 2), r.size(n));
    try testing.expectEqual(evicted0, stats.stats_generations_evicted.load()); // forgotten ⇒ not an eviction of a live kernel ref
    r.forget(n);
    // A generation the kernel still references AND has open survives the ring.
    const held = try r.mint(.config, try owned(testing.allocator, "held"));
    try testing.expect(r.pin(held));
    i = 0;
    while (i < REGISTRY_CAP) : (i += 1) {
        const ino = try r.mint(.config, try owned(testing.allocator, "z"));
        r.forget(ino);
    }
    try testing.expectEqual(evicted0 + 1, stats.stats_generations_evicted.load());
    try testing.expectEqual(@as(?u64, 4), r.size(held));
    try testing.expectEqual(REGISTRY_CAP + 1, r.liveCount());
    r.unpin(held);
    try testing.expectEqual(@as(?u64, 4), r.size(held));
    r.forget(held);
    try testing.expect(r.size(held) == null);
    try testing.expectEqual(REGISTRY_CAP, r.liveCount());
    // A wrong-class ino over a live seq is stale too.
    try testing.expect(r.size(inoOf(seqOf(n), .trace)) == null);
}
