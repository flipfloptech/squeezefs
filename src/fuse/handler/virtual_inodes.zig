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
//! ONE RENDER IN FLIGHT PER CLASS, IN ARRIVAL ORDER (decision 0003 §CC.2):
//! a lookup that arrives while a render runs waits for the NEXT render,
//! which serves every lookup that waited for it at once (the same nodeid,
//! one kernel reference each); a lookup that arrives with no render in
//! flight starts one. A completed generation is never reused and a render
//! that began before a lookup arrived is never joined, so every `.stats`
//! read reflects every effect that completed before its lookup arrived
//! (§CC.6: read-your-own-effects), while N concurrent readers cost O(1)
//! renders per render-time instead of O(N) each. WHY: the kernel's
//! `__lookup_slow` revalidates the dentry a sibling's lookup just created,
//! and a revalidation answered with a different nodeid invalidates it and
//! retries — 32 readers of `.stats` on an idle box livelocked into 2,179
//! renders for 55 reads, 21 s a read, the supervisor's probe in the same
//! queue (a SIGABRT). A waiter parks on the class's `Notify` registered-
//! first; the leader's mint completes the render under the registry lock
//! and announces after it.
//!
//! THE GENERATION IS THE PIN: a generation holds one reference per LOOKUP
//! answered with it (the kernel's, all dropped by the FORGET that evicts
//! the ino from the lookup table — the kernel's `nlookup` may arrive in
//! parts), one for the FIFO ring (dropped when the ring evicts it) and one
//! per open fh (OPEN/RELEASE); the payload frees at zero and a retired
//! generation answers `ESTALE`. There is no second table.
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
const Io = std.Io;
const counters = @import("../../core/counters.zig");
const jc = @import("../../meta/journal_core.zig");
const notify = @import("../../sync/notify.zig");

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
    /// Lookups that waited for a render in flight to finish and took the
    /// next render's generation (module doc) instead of one of their own.
    pub var stats_generation_shares: counters.ShardedCounter = .{};
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

/// A lookup's answer: the generation ino and its payload's length.
pub const Shared = struct { ino: u64, len: u64 };

/// The renderer a lookup calls (`kv_fs.VirtualFace.build`'s shape).
pub const RenderFn = *const fn (ctx: *anyopaque, class: Class, gpa: Allocator) Allocator.Error!Owned;

/// The class's render in flight and the lookups waiting for the next one
/// (module doc). Under the registry lock; `done` announced after it.
const Flight = struct {
    rendering: bool = false,
    /// Renders started (the index of the one in flight when `rendering`).
    started: u64 = 0,
    /// Renders completed; `last_seq` the generation of the newest.
    completed: u64 = 0,
    last_seq: u64 = Registry.no_seq,
    done: notify.Notify = .init,
};

const Generation = struct {
    seq: u64,
    class: Class,
    payload: Owned,
    /// kernel lookups + ring (1) + opens.
    refs: u32,
    /// The kernel's share of `refs`: one per LOOKUP answered with this
    /// generation (the leader's and every waiter's).
    kernel: u32,
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
    flights: [class_count]Flight = @splat(.{}),

    const no_seq = std.math.maxInt(u64);
    const class_count = @typeInfo(Class).@"enum".field_names.len;

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

    /// `lookup(1, <class name>)` (module doc): the leader renders through
    /// `render(ctx, class, gpa)` and mints; a lookup that arrives while a
    /// render runs waits for the next one and takes its generation. The
    /// render's error is the leader's answer; the waiters go on without it.
    pub fn lookup(self: *Registry, io: Io, class: Class, render: RenderFn, ctx: *anyopaque, gpa: Allocator) Allocator.Error!Shared {
        var target: u64 = undefined;
        var lead_now = false;
        {
            self.lock.lock();
            defer self.lock.unlock();
            const f = &self.flights[@backingInt(class)];
            if (!f.rendering) {
                f.rendering = true;
                f.started += 1;
                lead_now = true;
            } else target = f.started + 1;
        }
        if (lead_now) return self.renderUnlocked(io, class, render, ctx, gpa);
        while (true) {
            // Registered first, the condition re-checked under the registry
            // lock, then the park (notify's law): an announcement between
            // the re-check and the park is never lost.
            const f = &self.flights[@backingInt(class)];
            var n = f.done.notified();
            _ = n.enable(io);
            var lead = false;
            var took: ?Shared = null;
            {
                self.lock.lock();
                defer self.lock.unlock();
                if (f.completed >= target) {
                    if (self.gens.get(f.last_seq)) |g| {
                        g.refs += 1;
                        g.kernel += 1;
                        took = .{ .ino = inoOf(g.seq, class), .len = g.payload.len };
                    } else {
                        // The ring evicted it between the completion and this
                        // wake (REGISTRY_CAP mints in between): render anew.
                        target = f.completed + 1;
                    }
                }
                if (took == null and !f.rendering and f.completed < target) {
                    f.rendering = true;
                    f.started += 1;
                    lead = true;
                }
            }
            if (took) |shared| {
                n.cancel(io);
                stats.stats_generation_shares.inc();
                return shared;
            }
            if (lead) {
                n.cancel(io);
                return self.renderUnlocked(io, class, render, ctx, gpa);
            }
            n.waitUncancelable(io);
        }
    }

    /// The leader's render: a mint completes the flight, a failure abandons
    /// it; either way the waiters are announced to.
    fn renderUnlocked(self: *Registry, io: Io, class: Class, render: RenderFn, ctx: *anyopaque, gpa: Allocator) Allocator.Error!Shared {
        const f = &self.flights[@backingInt(class)];
        defer f.done.notifyWaiters(io);
        const payload = render(ctx, class, gpa) catch |e| {
            self.abandon(class);
            return e;
        };
        const ino = self.mint(class, payload) catch |e| {
            gpa.free(payload.buf);
            self.abandon(class);
            return e;
        };
        return .{ .ino = ino, .len = payload.len };
    }

    /// The leader's render failed: the flight is open again (a waiter
    /// starts the next render).
    fn abandon(self: *Registry, class: Class) void {
        self.lock.lock();
        defer self.lock.unlock();
        self.flights[@backingInt(class)].rendering = false;
    }

    /// Mint a generation owning `payload` — the leader's render completed
    /// (the class's flight closes on it); the ring's oldest loses its ring
    /// reference when the ring is full (evicted; freed when nothing else
    /// pins it). Returns the generation ino; a failure leaves the flight
    /// to the leader's `abandon`.
    pub fn mint(self: *Registry, class: Class, payload: Owned) Allocator.Error!u64 {
        const g = try self.gpa.create(Generation);
        errdefer self.gpa.destroy(g);
        self.lock.lock();
        defer self.lock.unlock();
        const f = &self.flights[@backingInt(class)];
        try self.gens.ensureUnusedCapacity(self.gpa, 1);
        const seq = self.next_seq;
        self.next_seq += 1;
        g.* = .{ .seq = seq, .class = class, .payload = payload, .refs = 2, .kernel = 1, .ringed = true };
        const slot = &self.ring[@intCast(seq % REGISTRY_CAP)];
        if (slot.* != no_seq) {
            if (self.gens.get(slot.*)) |old| {
                old.ringed = false;
                // An eviction the kernel still references is the rail's
                // engagement; a forgotten generation leaving is the norm.
                if (old.refs > 1) stats.stats_generations_evicted.inc();
                self.dropLocked(old, 1);
            }
        }
        slot.* = seq;
        self.gens.putAssumeCapacity(seq, g);
        if (f.rendering) {
            f.rendering = false;
            f.completed = f.started;
        }
        f.last_seq = seq;
        stats.stats_generations_live.set(self.gens.count());
        return inoOf(seq, class);
    }

    fn findLocked(self: *Registry, ino: u64) ?*Generation {
        const g = self.gens.get(seqOf(ino)) orelse return null;
        if (g.class != classOf(ino)) return null;
        return g;
    }

    /// `n` references down (never below zero); frees at zero (under the lock).
    fn dropLocked(self: *Registry, g: *Generation, n: u64) void {
        g.refs -= @intCast(@min(n, g.refs));
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
        self.dropLocked(g, 1);
    }

    /// FORGET (the one that evicts the ino from the lookup table): every
    /// kernel reference dropped — one per LOOKUP answered with this
    /// generation, the mint's and every share's.
    pub fn forget(self: *Registry, ino: u64) void {
        self.lock.lock();
        defer self.lock.unlock();
        const g = self.findLocked(ino) orelse return;
        const n = g.kernel;
        g.kernel = 0;
        self.dropLocked(g, n);
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

/// A test renderer: counts its builds, writes `value`'s current reading
/// into the payload, and can hold a render open on `gate` so a lookup
/// stays in flight while others arrive.
const FlightRenderer = struct {
    builds: std.atomic.Value(u32) = .init(0),
    value: std.atomic.Value(u64) = .init(0),
    gate: ?*Io.Event = null,
    /// Set when a render is inside the gate (the test's synchronization).
    inside: Io.Event = .unset,

    fn render(ctx: *anyopaque, class: Class, gpa: Allocator) Allocator.Error!Owned {
        const self: *FlightRenderer = @ptrCast(@alignCast(ctx));
        _ = self.builds.fetchAdd(1, .monotonic);
        const v = self.value.load(.seq_cst);
        if (self.gate) |g| {
            self.inside.set(testing.io);
            g.wait(testing.io) catch unreachable;
        }
        var buf: [64]u8 = undefined;
        const text = std.fmt.bufPrint(&buf, "{s}:{d}", .{ @tagName(class), v }) catch unreachable;
        return owned(gpa, text);
    }

    fn valueOf(r: *Registry, shared: Shared) u64 {
        var buf: [64]u8 = undefined;
        const n = r.read(shared.ino, 0, &buf).?;
        const text = buf[0..n];
        return std.fmt.parseInt(u64, text[std.mem.indexOfScalar(u8, text, ':').? + 1 ..], 10) catch unreachable;
    }
};

test "one render in flight per class, in arrival order (decision 0003 §CC.2): lookups that arrive while a render runs wait for the NEXT render and share its generation (one build for all of them, each a kernel reference), the leader keeps its own; sequential lookups mint distinct generations; another class renders on its own" {
    const io = testing.io;
    var r = Registry.init(testing.allocator);
    defer r.deinit();
    var gate: Io.Event = .unset;
    var fr: FlightRenderer = .{ .gate = &gate };
    const shares0 = stats.stats_generation_shares.load();
    const Leader = struct {
        fn run(reg: *Registry, io_: Io, ctx: *FlightRenderer, out: *Shared) void {
            out.* = reg.lookup(io_, .stats, FlightRenderer.render, ctx, testing.allocator) catch unreachable;
        }
    };
    var lead: Shared = undefined;
    const lt = try std.Thread.spawn(.{}, Leader.run, .{ &r, io, &fr, &lead });
    try fr.inside.wait(io);
    // Eight lookups arrive while the leader's render is held open.
    var outs: [8]Shared = undefined;
    var ts: [8]std.Thread = undefined;
    for (&ts, &outs) |*t, *o| t.* = try std.Thread.spawn(.{}, Leader.run, .{ &r, io, &fr, o });
    // The waiters are parked on the flight, not rendering: one build so far.
    while (r.flights[0].done.waiters(io) < 8) try std.Thread.yield();
    try testing.expectEqual(@as(u32, 1), fr.builds.load(.monotonic));
    // Release the leader: its mint closes the flight; a waiter leads the
    // next render (held open again), every other waiter takes it.
    fr.inside.reset();
    gate.set(io);
    lt.join();
    try fr.inside.wait(io);
    try testing.expectEqual(@as(u32, 2), fr.builds.load(.monotonic));
    fr.gate = null;
    gate.set(io);
    for (ts) |t| t.join();
    try testing.expectEqual(@as(u32, 2), fr.builds.load(.monotonic));
    for (outs) |o| {
        try testing.expectEqual(outs[0].ino, o.ino);
        try testing.expect(o.ino != lead.ino);
    }
    try testing.expectEqual(shares0 + 7, stats.stats_generation_shares.load());
    try testing.expectEqual(@as(usize, 2), r.liveCount());
    // The evicting FORGET of the shared ino returns all eight references.
    r.forget(outs[0].ino);
    try testing.expectEqual(@as(?u64, 7), r.size(outs[0].ino)); // the ring's reference keeps it
    // Sequential lookups with nothing in flight: a render each, distinct.
    const a = try r.lookup(io, .stats, FlightRenderer.render, &fr, testing.allocator);
    const b = try r.lookup(io, .stats, FlightRenderer.render, &fr, testing.allocator);
    try testing.expect(a.ino != b.ino);
    try testing.expectEqual(@as(u32, 4), fr.builds.load(.monotonic));
    // Another class is its own flight.
    const c = try r.lookup(io, .config, FlightRenderer.render, &fr, testing.allocator);
    try testing.expectEqual(Class.config, classOf(c.ino));
    try testing.expectEqual(@as(u32, 5), fr.builds.load(.monotonic));
    try testing.expectEqual(shares0 + 7, stats.stats_generation_shares.load());
}

test "read-your-own-effects under a herd (decision 0003 §CC.6): sixteen threads each bump a counter and look `.stats` up 50 times; every generation a lookup gets was rendered after the lookup arrived, so it carries that thread's bump (the value read ≥ the bump), while the herd shares renders (builds < lookups)" {
    const io = testing.io;
    var r = Registry.init(testing.allocator);
    defer r.deinit();
    var fr: FlightRenderer = .{};
    const Herd = struct {
        fn run(reg: *Registry, io_: Io, ctx: *FlightRenderer, failed: *std.atomic.Value(u32)) void {
            var i: usize = 0;
            while (i < 50) : (i += 1) {
                const mine = ctx.value.fetchAdd(1, .seq_cst) + 1;
                const got = reg.lookup(io_, .stats, FlightRenderer.render, ctx, testing.allocator) catch {
                    _ = failed.fetchAdd(1, .monotonic);
                    return;
                };
                if (FlightRenderer.valueOf(reg, got) < mine) _ = failed.fetchAdd(1, .monotonic);
                reg.forget(got.ino);
            }
        }
    };
    var failed: std.atomic.Value(u32) = .init(0);
    var ts: [16]std.Thread = undefined;
    for (&ts) |*t| t.* = try std.Thread.spawn(.{}, Herd.run, .{ &r, io, &fr, &failed });
    for (ts) |t| t.join();
    try testing.expectEqual(@as(u32, 0), failed.load(.monotonic));
    try testing.expect(fr.builds.load(.monotonic) <= 16 * 50);
    try testing.expectEqual(@as(u64, 16 * 50), fr.value.load(.monotonic));
}

test "a render that fails abandons the flight: the waiters go on — one of them renders, the error is the leader's alone" {
    const io = testing.io;
    var r = Registry.init(testing.allocator);
    defer r.deinit();
    const Failing = struct {
        calls: std.atomic.Value(u32) = .init(0),
        gate: Io.Event = .unset,
        inside: Io.Event = .unset,
        fn render(ctx: *anyopaque, class: Class, gpa: Allocator) Allocator.Error!Owned {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            const n = self.calls.fetchAdd(1, .monotonic);
            if (n == 0) {
                self.inside.set(testing.io);
                self.gate.wait(testing.io) catch unreachable;
                return error.OutOfMemory;
            }
            return owned(gpa, @tagName(class));
        }
        fn lead(reg: *Registry, io_: Io, ctx: *@This(), out: *Allocator.Error!Shared) void {
            out.* = reg.lookup(io_, .trace, @This().render, ctx, testing.allocator);
        }
    };
    var f: Failing = .{};
    var lead_out: Allocator.Error!Shared = undefined;
    var wait_out: Allocator.Error!Shared = undefined;
    const lt = try std.Thread.spawn(.{}, Failing.lead, .{ &r, io, &f, &lead_out });
    try f.inside.wait(io);
    const wt = try std.Thread.spawn(.{}, Failing.lead, .{ &r, io, &f, &wait_out });
    while (r.flights[@backingInt(Class.trace)].done.waiters(io) < 1) try std.Thread.yield();
    f.gate.set(io);
    lt.join();
    wt.join();
    try testing.expectError(error.OutOfMemory, lead_out);
    const got = try wait_out;
    try testing.expectEqual(@as(?u64, 5), r.size(got.ino));
    try testing.expectEqual(@as(u32, 2), f.calls.load(.monotonic));
    try testing.expect(!r.flights[@backingInt(Class.trace)].rendering);
}
