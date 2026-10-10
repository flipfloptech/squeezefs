//! The NVMe read cache — the mmap segment ring (charter §6.3 "NVMe read
//! cache (mmap)"; `docs/porting/data_read_path.md` §2.4, §2.5, §13 Q3, Q9,
//! Q10). The core is PORTABLE over any `[]u8` image (tests on every host);
//! the Linux binding (`Ring`) opens the format-declared cache paths — the
//! same dirs as staging, 0700 / 0600 — and maps them shared.
//!
//! On-media frame (Q9 — the reference's `0xCAFEBABE ‖ key_len ‖ val_len`
//! had no checksum, an integer magic and a 4 KiB key pad): ONE 64 B
//! `extern struct` `{magic "SQZRCACH", version, flags, key, val_len,
//! incarnation, xxh3}` on the 4 KiB grain, the value at `+4096` so a serve
//! can DMA from it. The xxh3 covers the HEADER: a torn or stale header is
//! refused; the value's integrity is the incarnation + binding lattice's
//! (a read cache serves only bytes it itself wrote under a validated fill).
//! No `msync` anywhere: a read cache owes no durability — it is WIPED at
//! mount on purpose (offsets are not incarnation-stable across mounts and no
//! cross-session incarnation store exists; a resurrected entry under a
//! reused key is the crash twin of the stale-fill family).
//!
//! Two-phase `put`: (1) under the shard lock compute the aligned placement
//! — wrap at the end, bump past PENDING and PINNED extents (bounded), evict
//! every unpinned entry the window overlaps GEOMETRY-COMPLETE over the
//! index (the reference's queue-order front-run clobbered still-indexed
//! entries — fstest.3), un-index the old same-key copy, push `pending`;
//! (2) memcpy header + value OUTSIDE the lock; (3) under the lock `validate`
//! at the INDEX INSERT — the visibility point; a refusal zeroes the magic
//! while `pending` still guards the extent, so no reader can observe the
//! bytes. Evictions are INDEX-ONLY (the materializing flavour's page-in +
//! memcpy of every victim was 44 % of daemon CPU + ~6× spurious reads).
//!
//! Lock discipline (Q3): a per-shard spin lock for the O(1) index ops and
//! per-entry PIN counts for the copy-out (the reference's writer-preferring
//! RwLock + `read_recursive` + blocking-pool writers was the Hang-1 wedge).
//! A serve pins under the lock, copies out of the lock, unpins; a removal or
//! replacement of a pinned entry RETIRES it and the last unpin frees the
//! cell — the placement never overwrites a pinned extent.
//!
//! Q10 (the WRITE_FIXED publish from the fill-pool slab) is owed: this wave
//! publishes by memcpy into the mapping — the A/B's other arm.

const std = @import("std");
const fuzzkit = @import("../core/fuzzkit.zig");
const testkit = @import("../core/testkit.zig");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;
const Atomic = std.atomic.Value;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const derive = @import("../core/derive.zig");
const checksum = @import("../core/checksum.zig");
const knobs = @import("../core/knobs.zig");
const budget = @import("../mem/budget.zig");
const jc = @import("../meta/journal_core.zig");
const staging = @import("staging.zig");
const rsc = @import("read_size_class.zig");
const hot_tier = @import("hot_tier.zig");

pub const BlockKey = rsc.BlockKey;
pub const Validate = hot_tier.Validate;

pub const MAGIC = "SQZRCACH".*;
pub const FRAME_VERSION: u16 = 1;
/// The record grain: header at `off`, value at `off + GRAIN`, every extent
/// a multiple of the grain (the O_DIRECT / `READ_FIXED` grain of a future
/// device-side serve).
pub const GRAIN: u64 = rsc.LBA_GRAIN;
pub const HEADER_LEN: usize = 64;
/// Read budget default: `aggregate_capacity / 4` of the cache devices.
pub const DEFAULT_BUDGET_DIVISOR: u64 = 4;

pub const FrameWire = extern struct {
    magic: [8]u8,
    version_le: [2]u8,
    flags_le: [2]u8,
    val_len_le: [4]u8,
    vol_tag_le: [8]u8,
    offset_le: [8]u8,
    incarnation_le: [8]u8,
    reserved: [16]u8,
    xxh3_le: [8]u8,
};

comptime {
    assert(@sizeOf(FrameWire) == HEADER_LEN);
    assert(@offsetOf(FrameWire, "xxh3_le") == 56);
}

pub const stats = struct {
    pub var read_cache_hits: counters.ShardedCounter = .{};
    pub var read_cache_puts: counters.ShardedCounter = .{};
    pub var read_cache_put_refusals: counters.ShardedCounter = .{};
    pub var read_cache_put_skips: counters.ShardedCounter = .{};
    pub var read_cache_evictions: counters.ShardedCounter = .{};
    pub var read_cache_logical_bytes: counters.Gauge = .{};
    pub var read_cache_shards: counters.Gauge = .{};
    /// A frame whose header failed its magic/xxh3 under a live index entry
    /// (must stay 0: the index is RAM-only and the mapping ours).
    pub var read_cache_torn_frames: counters.Tripwire = .init("read_cache_torn_frames");
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"read_cache_hits\":{d},\"read_cache_puts\":{d},\"read_cache_put_refusals\":{d},\"read_cache_put_skips\":{d},\"read_cache_evictions\":{d},\"read_cache_logical_bytes\":{d},\"read_cache_shards\":{d},\"read_cache_torn_frames\":{d}}}", .{
        stats.read_cache_hits.load(),
        stats.read_cache_puts.load(),
        stats.read_cache_put_refusals.load(),
        stats.read_cache_put_skips.load(),
        stats.read_cache_evictions.load(),
        stats.read_cache_logical_bytes.load(),
        stats.read_cache_shards.load(),
        stats.read_cache_torn_frames.load(),
    });
}

/// `--read-disk-cache-size` else `aggregate_capacity / 4`.
pub fn resolveReadDiskBudget(explicit_mib: ?u64, aggregate_capacity: u64) u64 {
    if (explicit_mib) |m| return m * derive.MiB;
    return aggregate_capacity / DEFAULT_BUDGET_DIVISOR;
}

/// The knob row (`SQUEEZEFS_READ_DISK_CACHE_MB`) over the resolver.
pub fn budgetFromEnv(env: *const std.process.Environ.Map, aggregate_capacity: u64) u64 {
    return resolveReadDiskBudget(knobs.optIntKnob("SQUEEZEFS_READ_DISK_CACHE_MB", u64, env), aggregate_capacity);
}

/// Record footprint: one header grain + the value rounded to the grain.
pub fn footprint(val_len: u64) u64 {
    return GRAIN + derive.alignUp(val_len, GRAIN);
}

/// `shardPlan(per_device_capacity, cores, max_entry)` — the staging ring's
/// law verbatim (a shard must fit a same-key replace of a block).
pub const shardPlan = staging.shardPlan;

pub fn encodeHeader(out: *[HEADER_LEN]u8, key: BlockKey, val_len: u32, incarnation: u64) void {
    var h: FrameWire = undefined;
    h.magic = MAGIC;
    std.mem.writeInt(u16, &h.version_le, FRAME_VERSION, .little);
    std.mem.writeInt(u16, &h.flags_le, 0, .little);
    std.mem.writeInt(u32, &h.val_len_le, val_len, .little);
    std.mem.writeInt(u64, &h.vol_tag_le, key.vol_tag, .little);
    std.mem.writeInt(u64, &h.offset_le, key.offset, .little);
    std.mem.writeInt(u64, &h.incarnation_le, incarnation, .little);
    h.reserved = @splat(0);
    h.xxh3_le = @splat(0);
    out.* = std.mem.toBytes(h);
    checksum.stamp(out, 56);
}

pub const Header = struct { key: BlockKey, val_len: u32, incarnation: u64 };

/// Total: null on any refusal (magic, version, flags, reserved, xxh3).
pub fn decodeHeader(bytes: *const [HEADER_LEN]u8) ?Header {
    if (!checksum.verify(bytes, 56)) return null;
    const h = std.mem.bytesToValue(FrameWire, bytes);
    if (!std.mem.eql(u8, &h.magic, &MAGIC)) return null;
    if (std.mem.readInt(u16, &h.version_le, .little) != FRAME_VERSION) return null;
    if (std.mem.readInt(u16, &h.flags_le, .little) != 0) return null;
    if (!std.mem.allEqual(u8, &h.reserved, 0)) return null;
    return .{
        .key = .{ .vol_tag = std.mem.readInt(u64, &h.vol_tag_le, .little), .offset = std.mem.readInt(u64, &h.offset_le, .little) },
        .val_len = std.mem.readInt(u32, &h.val_len_le, .little),
        .incarnation = std.mem.readInt(u64, &h.incarnation_le, .little),
    };
}

pub const PutOutcome = enum {
    published,
    /// The validation refused at the index insert: the frame was voided.
    refused,
    /// No placement (over capacity, or a pathological pinned/pending
    /// layout): a cache put is best-effort.
    skipped,
};

const Cell = struct {
    key: BlockKey = undefined,
    off: u64 = 0,
    len: u32 = 0,
    live: bool = false,
    /// Removed/replaced while pinned: freed by the last unpin.
    retired: bool = false,
    pins: Atomic(u32) = .init(0),
};

const Extent = struct { start: u64, end: u64 };

pub const Shard = struct {
    gpa: Allocator,
    image: []u8,
    lock: jc.SpinLock = .{},
    index: rsc.KeyMap(u32) = .empty,
    cells: []Cell,
    free: []u32,
    free_len: usize,
    pending: std.ArrayList(Extent) = .empty,
    write_offset: u64 = 0,
    logical_bytes: Atomic(u64) = .init(0),

    /// Every record occupies ≥ 2 grains, so the cell population is bounded
    /// by `capacity / (2 × GRAIN)` + the in-flight replacement copies.
    pub fn maxCells(image_len: u64) usize {
        return @intCast(image_len / (2 * GRAIN) + 2);
    }

    pub fn init(gpa: Allocator, image: []u8) Allocator.Error!Shard {
        assert(derive.isAligned(image.len, GRAIN) and image.len >= 2 * GRAIN);
        const n = maxCells(image.len);
        const cells = try gpa.alloc(Cell, n);
        errdefer gpa.free(cells);
        @memset(cells, .{});
        const free = try gpa.alloc(u32, n);
        errdefer gpa.free(free);
        for (free, 0..) |*f, i| f.* = @intCast(n - 1 - i);
        var self: Shard = .{ .gpa = gpa, .image = image, .cells = cells, .free = free, .free_len = n };
        try self.index.ensureTotalCapacity(gpa, @intCast(n));
        try self.pending.ensureTotalCapacity(gpa, n);
        return self;
    }

    pub fn deinit(self: *Shard) void {
        self.index.deinit(self.gpa);
        self.pending.deinit(self.gpa);
        self.gpa.free(self.cells);
        self.gpa.free(self.free);
        self.* = undefined;
    }

    pub fn capacity(self: *const Shard) u64 {
        return self.image.len;
    }

    fn overlaps(a: Extent, b: Extent) bool {
        return a.start < b.end and b.start < a.end;
    }

    fn cellExtent(c: *const Cell) Extent {
        return .{ .start = c.off, .end = c.off + footprint(c.len) };
    }

    fn freeCellLocked(self: *Shard, idx: u32) void {
        const c = &self.cells[idx];
        c.live = false;
        c.retired = false;
        self.free[self.free_len] = idx;
        self.free_len += 1;
    }

    /// Un-index (index-only eviction); a pinned cell retires instead.
    fn unindexLocked(self: *Shard, idx: u32) void {
        const c = &self.cells[idx];
        assert(self.index.remove(c.key));
        _ = self.logical_bytes.fetchSub(c.len, .monotonic);
        if (c.pins.load(.monotonic) == 0) self.freeCellLocked(idx) else c.retired = true;
    }

    /// Phase 1: the placement. Bounded by `pending + pinned + 2` bumps.
    fn placeLocked(self: *Shard, need: u64) ?u64 {
        if (need > self.capacity()) return null;
        var target = derive.alignUp(self.write_offset, GRAIN);
        var attempts: usize = self.pending.items.len + 2;
        for (self.cells) |*c| attempts += @intFromBool(c.live and c.pins.load(.monotonic) != 0);
        while (attempts > 0) : (attempts -= 1) {
            if (target + need > self.capacity()) target = 0;
            const win: Extent = .{ .start = target, .end = target + need };
            var bump: ?u64 = null;
            for (self.pending.items) |p| if (overlaps(win, p)) {
                bump = if (bump) |b| @max(b, p.end) else p.end;
            };
            for (self.cells) |*c| {
                if (!c.live) continue;
                // A retired (un-indexed, still pinned) cell guards its bytes too.
                if (c.pins.load(.monotonic) == 0 and !c.retired) continue;
                const e = cellExtent(c);
                if (overlaps(win, e)) bump = if (bump) |b| @max(b, e.end) else e.end;
            }
            if (bump) |b| {
                target = derive.alignUp(b, GRAIN);
                continue;
            }
            // Evict every unpinned live entry the window overlaps.
            for (self.cells, 0..) |*c, i| {
                if (!c.live or c.retired) continue;
                if (overlaps(win, cellExtent(c))) {
                    self.unindexLocked(@intCast(i));
                    stats.read_cache_evictions.inc();
                }
            }
            return target;
        }
        return null;
    }

    /// The validated two-phase put.
    pub fn put(self: *Shard, key: BlockKey, value: []const u8, incarnation: u64, validate: Validate) PutOutcome {
        const need = footprint(value.len);
        const target: u64 = blk: {
            self.lock.lock();
            defer self.lock.unlock();
            if (self.free_len == 0) break :blk null;
            const t = self.placeLocked(need) orelse break :blk null;
            // Un-index the old same-key copy now: the new frame supersedes it.
            if (self.index.get(key)) |old| self.unindexLocked(old);
            self.pending.appendAssumeCapacity(.{ .start = t, .end = t + need });
            self.write_offset = t + need;
            break :blk t;
        } orelse {
            stats.read_cache_put_skips.inc();
            return .skipped;
        };
        // Phase 2: the bytes, outside the lock.
        const hdr: *[HEADER_LEN]u8 = self.image[@intCast(target)..][0..HEADER_LEN];
        encodeHeader(hdr, key, @intCast(value.len), incarnation);
        @memcpy(self.image[@intCast(target + GRAIN)..][0..value.len], value);
        // Phase 3: validate at the index insert.
        self.lock.lock();
        defer self.lock.unlock();
        self.removePendingLocked(target);
        if (!validate.run()) {
            @memset(hdr[0..8], 0);
            stats.read_cache_put_refusals.inc();
            return .refused;
        }
        if (self.index.get(key)) |racer| self.unindexLocked(racer);
        assert(self.free_len > 0);
        self.free_len -= 1;
        const idx = self.free[self.free_len];
        self.cells[idx] = .{ .key = key, .off = target, .len = @intCast(value.len), .live = true };
        self.index.putAssumeCapacity(key, idx);
        _ = self.logical_bytes.fetchAdd(value.len, .monotonic);
        stats.read_cache_puts.inc();
        return .published;
    }

    fn removePendingLocked(self: *Shard, start: u64) void {
        for (self.pending.items, 0..) |p, i| if (p.start == start) {
            _ = self.pending.swapRemove(i);
            return;
        };
        unreachable;
    }

    pub fn has(self: *Shard, key: BlockKey) bool {
        self.lock.lock();
        defer self.lock.unlock();
        return self.index.contains(key);
    }

    /// Copy `[lo, hi)` of the value (clamped to it) into `out`; the bytes
    /// copied, or null on a miss. Pins across the copy.
    pub fn getRange(self: *Shard, key: BlockKey, lo: u64, hi: u64, out: []u8) ?usize {
        const idx: u32 = blk: {
            self.lock.lock();
            defer self.lock.unlock();
            const i = self.index.get(key) orelse return null;
            _ = self.cells[i].pins.fetchAdd(1, .monotonic);
            break :blk i;
        };
        defer self.unpin(idx);
        const c = &self.cells[idx];
        const hdr: *const [HEADER_LEN]u8 = self.image[@intCast(c.off)..][0..HEADER_LEN];
        const h = decodeHeader(hdr) orelse {
            stats.read_cache_torn_frames.fire("an indexed read-cache frame failed its header check");
            return null;
        };
        if (!h.key.eql(key) or h.val_len != c.len) {
            stats.read_cache_torn_frames.fire("an indexed read-cache frame names another key");
            return null;
        }
        const start = @min(lo, c.len);
        const end = @min(@max(hi, start), c.len);
        const n: usize = @intCast(end - start);
        const copy_n = @min(n, out.len);
        @memcpy(out[0..copy_n], self.image[@intCast(c.off + GRAIN + start)..][0..copy_n]);
        stats.read_cache_hits.inc();
        return copy_n;
    }

    fn unpin(self: *Shard, idx: u32) void {
        self.lock.lock();
        defer self.lock.unlock();
        const c = &self.cells[idx];
        if (c.pins.fetchSub(1, .acq_rel) == 1 and c.retired) self.freeCellLocked(idx);
    }

    /// The purge arm (index-only; a pinned entry retires).
    pub fn remove(self: *Shard, key: BlockKey) bool {
        self.lock.lock();
        defer self.lock.unlock();
        const idx = self.index.get(key) orelse return false;
        self.unindexLocked(idx);
        return true;
    }

    pub fn live(self: *Shard) usize {
        self.lock.lock();
        defer self.lock.unlock();
        return self.index.count();
    }
};

/// The cache over N shards (one per device shard plan).
pub const Cache = struct {
    gpa: Allocator,
    shards: []Shard,
    /// Owned images (the RAM double); the Linux binding owns its mappings.
    owned_images: [][]u8,
    mapped: []MappedShard,
    enabled: bool,

    pub const MappedShard = if (builtin.os.tag == .linux) Ring.Mapped else struct {};

    /// A cache-less volume (no format-declared cache paths): every put
    /// returns `.skipped`, every probe misses — the tier simply is not there.
    pub fn initDisabled(gpa: Allocator) Cache {
        return .{ .gpa = gpa, .shards = &.{}, .owned_images = &.{}, .mapped = &.{}, .enabled = false };
    }

    /// The RAM double: `shard_count` images of `per_shard` bytes.
    pub fn initRam(gpa: Allocator, shard_count: u32, per_shard: u64) Allocator.Error!Cache {
        const images = try gpa.alloc([]u8, shard_count);
        errdefer gpa.free(images);
        var built: usize = 0;
        errdefer for (images[0..built]) |im| gpa.free(im);
        for (images) |*im| {
            im.* = try gpa.alloc(u8, @intCast(per_shard));
            @memset(im.*, 0);
            built += 1;
        }
        const shards = try gpa.alloc(Shard, shard_count);
        errdefer gpa.free(shards);
        var sbuilt: usize = 0;
        errdefer for (shards[0..sbuilt]) |*s| s.deinit();
        for (shards, images) |*s, im| {
            s.* = try Shard.init(gpa, im);
            sbuilt += 1;
        }
        stats.read_cache_shards.set(shard_count);
        return .{ .gpa = gpa, .shards = shards, .owned_images = images, .mapped = &.{}, .enabled = true };
    }

    pub fn deinit(self: *Cache) void {
        for (self.shards) |*s| s.deinit();
        if (self.shards.len > 0) self.gpa.free(self.shards);
        for (self.owned_images) |im| self.gpa.free(im);
        if (self.owned_images.len > 0) self.gpa.free(self.owned_images);
        if (builtin.os.tag == .linux) {
            for (self.mapped) |*m| m.close();
            if (self.mapped.len > 0) self.gpa.free(self.mapped);
        }
        self.* = undefined;
    }

    fn shardOf(self: *Cache, key: BlockKey) *Shard {
        return &self.shards[@intCast(key.hash() % self.shards.len)];
    }

    /// `cache_read_block_if`: cache-less ⇒ skipped; `tier_publish_paused`
    /// (the UNRECLAIMABLE arm of the budget, not the level) ⇒ skipped +
    /// counted; else the validated put.
    pub fn putValidated(self: *Cache, key: BlockKey, value: []const u8, incarnation: u64, validate: Validate, publish_paused: bool) PutOutcome {
        if (!self.enabled) return .skipped;
        if (publish_paused) {
            paused_publishes.inc();
            return .skipped;
        }
        const out = self.shardOf(key).put(key, value, incarnation, validate);
        stats.read_cache_logical_bytes.set(self.logicalBytes());
        return out;
    }

    pub fn has(self: *Cache, key: BlockKey) bool {
        if (!self.enabled) return false;
        return self.shardOf(key).has(key);
    }

    pub fn getRange(self: *Cache, key: BlockKey, lo: u64, hi: u64, out: []u8) ?usize {
        if (!self.enabled) return null;
        return self.shardOf(key).getRange(key, lo, hi, out);
    }

    /// The purge arm.
    pub fn purge(self: *Cache, key: BlockKey) bool {
        if (!self.enabled) return false;
        const r = self.shardOf(key).remove(key);
        if (r) stats.read_cache_logical_bytes.set(self.logicalBytes());
        return r;
    }

    pub fn logicalBytes(self: *const Cache) u64 {
        var n: u64 = 0;
        for (self.shards) |*s| n += s.logical_bytes.load(.monotonic);
        return n;
    }

    fn gaugeFn(ctx: *anyopaque) u64 {
        const self: *Cache = @ptrCast(@alignCast(ctx));
        return self.logicalBytes();
    }

    /// `read_tier_mmap`: floor 0, weight 0, kernel-reclaimable — attribution
    /// only (logical bytes over reclaimable page cache pinned phantom Red).
    pub fn component(self: *Cache) budget.Component {
        return budget.Component.init("read_tier_mmap", 0, 0, .{ .func = .{ .ctx = self, .call = &gaugeFn } }, null).kernelReclaimable();
    }
};

/// `read_tier_publishes_paused`: publishes skipped under the budget's
/// tier-publish pause.
pub var paused_publishes: counters.ShardedCounter = .{};

// ---------------------------------------------------------------------------
// The Linux binding: `<cache_root>/cache_segment/shard-N`, wiped at open.
// ---------------------------------------------------------------------------

pub const Ring = if (builtin.os.tag == .linux) struct {
    pub const SEGMENT_DIR = "cache_segment";

    pub const Mapped = struct {
        fd: linux.fd_t,
        map: []align(4096) u8,

        pub fn close(self: *Mapped) void {
            _ = linux.munmap(self.map.ptr, self.map.len);
            _ = linux.close(self.fd);
            self.* = undefined;
        }
    };

    /// Open `shard_count` segments of `per_shard` bytes under the staging
    /// root (the SAME dirs as staging: 0700 dirs, 0600 files — plaintext
    /// payload on passthrough volumes is private to the daemon's uid),
    /// WIPE them (cold on purpose), map them shared, build the cache.
    pub fn open(gpa: Allocator, root_path: []const u8, shard_count: u32, per_shard: u64) !Cache {
        assert(derive.isAligned(per_shard, GRAIN));
        const root_fd = try staging.Ring.openRoot(root_path);
        defer _ = linux.close(root_fd);
        _ = linux.mkdirat(root_fd, SEGMENT_DIR, staging.Ring.DIR_MODE);
        const mapped = try gpa.alloc(Mapped, shard_count);
        errdefer gpa.free(mapped);
        var built: usize = 0;
        errdefer for (mapped[0..built]) |*m| m.close();
        var name_buf: [32]u8 = undefined;
        for (mapped, 0..) |*m, i| {
            const name = try std.mem.printSentinel(&name_buf, "shard-{d}", .{i}, 0);
            const seg = try staging.Ring.openSegmentIn(root_fd, SEGMENT_DIR, name, per_shard);
            // Cold on purpose: drop every page of a previous session.
            _ = linux.fallocate(seg.fd, linux.FALLOC.FL_PUNCH_HOLE | linux.FALLOC.FL_KEEP_SIZE, 0, @intCast(seg.map.len));
            m.* = .{ .fd = seg.fd, .map = seg.map[0..@intCast(per_shard)] };
            built += 1;
        }
        const shards = try gpa.alloc(Shard, shard_count);
        errdefer gpa.free(shards);
        var sbuilt: usize = 0;
        errdefer for (shards[0..sbuilt]) |*s| s.deinit();
        for (shards, mapped) |*s, *m| {
            s.* = try Shard.init(gpa, m.map);
            sbuilt += 1;
        }
        stats.read_cache_shards.set(shard_count);
        return .{ .gpa = gpa, .shards = shards, .owned_images = &.{}, .mapped = mapped, .enabled = true };
    }
} else struct {};

// ---------------------------------------------------------------------------

const testing = std.testing;

test "frame: encode/decode round-trips; any flipped header byte refuses; footprint on the grain" {
    var h: [HEADER_LEN]u8 = undefined;
    const key = BlockKey.init(0x1122, 8 << 20);
    encodeHeader(&h, key, 4_000_000, 0x55);
    const d = decodeHeader(&h).?;
    try testing.expect(d.key.eql(key));
    try testing.expectEqual(@as(u32, 4_000_000), d.val_len);
    try testing.expectEqual(@as(u64, 0x55), d.incarnation);
    for (0..HEADER_LEN) |i| {
        var c = h;
        c[i] ^= 0x01;
        try testing.expect(decodeHeader(&c) == null);
    }
    try testing.expectEqual(@as(u64, 2 * GRAIN), footprint(1));
    try testing.expectEqual(@as(u64, 2 * GRAIN), footprint(GRAIN));
    try testing.expectEqual(@as(u64, 3 * GRAIN), footprint(GRAIN + 1));
    try testing.expectEqual(@as(u64, 256 * derive.MiB), resolveReadDiskBudget(null, derive.GiB));
    try testing.expectEqual(@as(u64, 10 * derive.MiB), resolveReadDiskBudget(10, derive.GiB));
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    try testing.expectEqual(@as(u64, 256 * derive.MiB), budgetFromEnv(&env, derive.GiB));
    try env.put("SQUEEZEFS_READ_DISK_CACHE_MB", "12");
    try testing.expectEqual(@as(u64, 12 * derive.MiB), budgetFromEnv(&env, derive.GiB));
}

test "two-phase put: validated at the index insert; a refusal voids the frame; replace supersedes; wrap evicts geometry-complete; ranges clamp; purge" {
    var cache = try Cache.initRam(testing.allocator, 1, 8 * GRAIN);
    defer cache.deinit();
    const k0 = BlockKey.init(1, 0);
    const k1 = BlockKey.init(1, 4 << 20);
    const k2 = BlockKey.init(1, 8 << 20);
    var v: [GRAIN]u8 = @splat(0xA1);
    const Refuse = struct {
        fn no(_: ?*anyopaque) bool {
            return false;
        }
    };
    try testing.expectEqual(PutOutcome.refused, cache.putValidated(k0, &v, 1, .{ .call = &Refuse.no }, false));
    try testing.expect(!cache.has(k0));
    // The voided frame's magic is zero.
    try testing.expect(decodeHeader(cache.shards[0].image[0..HEADER_LEN]) == null);
    try testing.expectEqual(PutOutcome.published, cache.putValidated(k0, &v, 1, .always, false));
    try testing.expect(cache.has(k0));
    var out: [GRAIN]u8 = undefined;
    try testing.expectEqual(@as(?usize, 100), cache.getRange(k0, 10, 110, &out));
    try testing.expectEqual(@as(u8, 0xA1), out[0]);
    // Clamp: past the value is empty.
    try testing.expectEqual(@as(?usize, 0), cache.getRange(k0, GRAIN, GRAIN + 10, &out));
    try testing.expectEqual(@as(?usize, null), cache.getRange(k1, 0, 10, &out));
    // Replace the same key: the old copy is un-indexed, the new one serves.
    v = @splat(0xB2);
    try testing.expectEqual(PutOutcome.published, cache.putValidated(k0, &v, 2, .always, false));
    try testing.expectEqual(@as(?usize, 1), cache.getRange(k0, 0, 1, &out));
    try testing.expectEqual(@as(u8, 0xB2), out[0]);
    try testing.expectEqual(@as(usize, 1), cache.shards[0].live());
    // Capacity 8 grains = 4 records of 2 grains. The cursor is at 6 (the
    // voided frame at 0, the superseded copy at 2, the live k0 at 4): k1
    // lands at 6, k2 wraps to 0, k3 reuses the superseded copy's extent at
    // 2 — four live records fill the ring.
    try testing.expectEqual(PutOutcome.published, cache.putValidated(k1, &v, 1, .always, false));
    try testing.expectEqual(PutOutcome.published, cache.putValidated(k2, &v, 1, .always, false));
    try testing.expectEqual(@as(usize, 3), cache.shards[0].live());
    const k3 = BlockKey.init(1, 12 << 20);
    try testing.expectEqual(PutOutcome.published, cache.putValidated(k3, &v, 1, .always, false));
    try testing.expectEqual(@as(usize, 4), cache.shards[0].live());
    try testing.expectEqual(@as(u64, 2 * GRAIN), cache.shards[0].cells[cache.shards[0].index.get(k3).?].off);
    // The fifth overlaps the live k0 at [4, 6): evicted geometry-complete.
    const k4 = BlockKey.init(1, 16 << 20);
    try testing.expectEqual(PutOutcome.published, cache.putValidated(k4, &v, 1, .always, false));
    try testing.expect(cache.has(k4) and cache.has(k1) and cache.has(k2) and cache.has(k3));
    try testing.expect(!cache.has(k0));
    try testing.expectEqual(@as(u64, 4 * GRAIN), cache.logicalBytes());
    // A publish under the tier pause is skipped and counted.
    const paused_before = paused_publishes.load();
    try testing.expectEqual(PutOutcome.skipped, cache.putValidated(k0, &v, 1, .always, true));
    try testing.expectEqual(paused_before + 1, paused_publishes.load());
    // Larger than the shard: skipped.
    var huge: [9 * GRAIN]u8 = undefined;
    try testing.expectEqual(PutOutcome.skipped, cache.putValidated(k0, &huge, 1, .always, false));
    try testing.expect(cache.purge(k1));
    try testing.expect(!cache.purge(k1));
    try testing.expect(!cache.has(k1));
    const comp = cache.component();
    try testing.expectEqualStrings("read_tier_mmap", comp.name);
    try testing.expect(comp.kernel_reclaimable and comp.weight == 0);
    var off = Cache.initDisabled(testing.allocator);
    defer off.deinit();
    try testing.expectEqual(PutOutcome.skipped, off.putValidated(k0, &v, 1, .always, false));
    try testing.expect(!off.has(k0));
}

test "a pinned entry is never overwritten: the placement bumps past it; the purge retires it and the last unpin frees the cell" {
    var cache = try Cache.initRam(testing.allocator, 1, 6 * GRAIN);
    defer cache.deinit();
    const s = &cache.shards[0];
    const k0 = BlockKey.init(2, 0);
    const k1 = BlockKey.init(2, 1 << 20);
    var v: [GRAIN]u8 = @splat(0x01);
    try testing.expectEqual(PutOutcome.published, cache.putValidated(k0, &v, 1, .always, false));
    // Pin k0 by hand (the serve's lock-protected pin).
    const idx = s.index.get(k0).?;
    _ = s.cells[idx].pins.fetchAdd(1, .monotonic);
    v = @splat(0x02);
    try testing.expectEqual(PutOutcome.published, cache.putValidated(k1, &v, 1, .always, false));
    // Cursor at 4 grains; a third record (2 grains) fits at 4.
    const k2 = BlockKey.init(2, 2 << 20);
    try testing.expectEqual(PutOutcome.published, cache.putValidated(k2, &v, 1, .always, false));
    // A fourth wraps: the window [0, 2) is PINNED (k0) ⇒ bump to 2, evicting k1.
    const k3 = BlockKey.init(2, 3 << 20);
    try testing.expectEqual(PutOutcome.published, cache.putValidated(k3, &v, 1, .always, false));
    try testing.expect(cache.has(k0) and !cache.has(k1) and cache.has(k2) and cache.has(k3));
    try testing.expectEqual(@as(u64, 2 * GRAIN), s.cells[s.index.get(k3).?].off);
    // k0's bytes are intact under the pin.
    var out: [1]u8 = undefined;
    try testing.expectEqual(@as(?usize, 1), cache.getRange(k0, 0, 1, &out));
    try testing.expectEqual(@as(u8, 0x01), out[0]);
    // Purge while pinned retires; the cell frees at the last unpin.
    try testing.expect(cache.purge(k0));
    try testing.expect(s.cells[idx].live and s.cells[idx].retired);
    const free_before = s.free_len;
    s.unpin(idx);
    try testing.expect(!s.cells[idx].live);
    try testing.expectEqual(free_before + 1, s.free_len);
}

test "Linux binding: segments under <root>/cache_segment are 0600 in 0700 dirs, wiped at open, and serve round-trips" {
    if (builtin.os.tag != .linux) return testkit.skip(@src(), .no_linux, "nvme_read_cache: Linux only");
    var path_buf: [96]u8 = undefined;
    const dir: []const u8 = blk: {
        std.Io.Dir.accessAbsolute(testing.io, "/build", .{}) catch break :blk "/tmp";
        break :blk "/build";
    };
    const root = try std.fmt.bufPrint(&path_buf, "{s}/sqz-readcache-{d}", .{ dir, linux.getpid() });
    defer std.Io.Dir.cwd().deleteTree(testing.io, root) catch {};
    var cache = try Ring.open(testing.allocator, root, 2, 8 * GRAIN);
    defer cache.deinit();
    var v: [2 * GRAIN]u8 = @splat(0xC3);
    const k = BlockKey.init(3, 0);
    try testing.expectEqual(PutOutcome.published, cache.putValidated(k, &v, 7, .always, false));
    var out: [GRAIN]u8 = undefined;
    try testing.expectEqual(@as(?usize, GRAIN), cache.getRange(k, GRAIN, 2 * GRAIN, &out));
    try testing.expectEqual(@as(u8, 0xC3), out[GRAIN - 1]);
    var sub_buf: [128]u8 = undefined;
    const sub = try std.mem.printSentinel(&sub_buf, "{s}/{s}/shard-0", .{ root, Ring.SEGMENT_DIR }, 0);
    var st: linux.Statx = undefined;
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.statx(linux.AT.FDCWD, sub, 0, .{ .MODE = true }, &st)));
    try testing.expectEqual(@as(u16, 0o600), st.mode & 0o777);
    var root_z: [128]u8 = undefined;
    const rootz = try std.mem.printSentinel(&root_z, "{s}", .{root}, 0);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.statx(linux.AT.FDCWD, rootz, 0, .{ .MODE = true }, &st)));
    try testing.expectEqual(@as(u16, 0o700), st.mode & 0o777);
    // Reopen: cold — the index is empty even though the file persists.
    cache.deinit();
    cache = try Ring.open(testing.allocator, root, 2, 8 * GRAIN);
    try testing.expect(!cache.has(k));
}

fn cacheHeaderFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [HEADER_LEN]u8 = undefined;
    smith.bytes(&buf);
    _ = decodeHeader(&buf);
    checksum.stamp(&buf, 56);
    if (decodeHeader(&buf)) |h| {
        var re: [HEADER_LEN]u8 = undefined;
        encodeHeader(&re, h.key, h.val_len, h.incarnation);
        try std.testing.expectEqualSlices(u8, &buf, &re);
    }
}

test "fuzz: the read-cache frame header decoder is total on arbitrary and restamped headers; a decoded header re-encodes byte-identically" {
    try fuzzkit.fuzz({}, cacheHeaderFuzz, .{});
}
