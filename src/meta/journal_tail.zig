//! The journal tail: a kernel-held mirror of a writable volume's journal
//! ring that outlives the daemon (charter §5.2 "The ack point"; decision
//! 0003 §BB.2–BB.3). Under the coalesced cadence a window's members are
//! acked once the RAM apply is published and the window's image is COPIED
//! here, before the landing; the mount's supervisor holds an fd to the tail
//! and, at the daemon's death, writes every un-landed slot to the device in
//! seq order and barriers before it releases the writer guard — a daemon
//! crash loses nothing acked.
//!
//! The shape (the staging ring's trick: a memfd the daemon writes with a
//! memcpy and another process reads): a header (magic, version, the device
//! path, the ring's segments and grain), a slot table, and the MIRROR — the
//! ring's bytes at the ring's own offsets. The applier copies the window's
//! aligned ops into the mirror at `op.offset − ring_base` and publishes the
//! slot (`written`, release) with the window's covering runs; the lane's
//! terminal outcome frees the slot. The bytes the lane writes ARE the
//! replay's bytes: one memcpy per window, no second codec, byte-identical
//! to the device image, so a landed window rewritten is idempotent by
//! construction and a slot freed is skipped.
//!
//! A slot's covering runs hold only bytes the window owns: on a padded
//! ring the runs are the window's aligned runs themselves; on an unpadded
//! ring the span from its first op to its last holds its entries, the page
//! headers it owns (every page boundary inside a window's logical span is
//! its own) and the holes of rolled-out members — never another window's
//! byte. A window with apply holes is acked at the landing (the hole
//! checkpoint must precede its survivors' ack), so a hole's zeros replayed
//! are never ahead of an acked survivor.
//!
//! Portable: the image over a byte buffer (`Image`) and the replay over a
//! sink are tested on every host; the memfd and the device replay are the
//! Linux part (`Tail`, `replayFd`).

const std = @import("std");
const builtin = @import("builtin");

const journal = @import("journal.zig");
const testkit = @import("../core/testkit.zig");

pub const MAGIC: [8]u8 = "SQZJTAIL".*;
pub const VERSION: u32 = 1;
/// The device path in the header: the kernel's own bound.
pub const PATH_CAP: usize = std.os.linux.PATH_MAX;
/// The mirror begins on a page: a replayer's `pwrite` of a page-aligned
/// run from a page-aligned source.
const MIRROR_ALIGN: u64 = journal.PAGE_LEN;

pub const Segment = extern struct { start: u64, pages: u64 };

pub const Header = extern struct {
    magic: [8]u8,
    version: u32,
    /// 0 = the one implicit segment at `base`.
    n_segments: u32,
    base: u64,
    pages: u64,
    grain: u64,
    n_slots: u32,
    path_len: u32,
    slots_off: u64,
    mirror_off: u64,
    segments: [journal.RING_SEGMENTS_MAX]Segment,
    path: [PATH_CAP]u8,

    pub fn ringBytes(self: *const Header) u64 {
        return self.pages * journal.PAGE_LEN;
    }

    pub fn pathSlice(self: *const Header) []const u8 {
        return self.path[0..self.path_len];
    }

    /// Device offset of logical page `page` (`RingLayout.pageOffset`).
    pub fn pageOffset(self: *const Header, page: u64) u64 {
        if (self.n_segments == 0) return self.base + page * journal.PAGE_LEN;
        var p = page;
        for (self.segments[0..self.n_segments]) |s| {
            if (p < s.pages) return s.start + p * journal.PAGE_LEN;
            p -= s.pages;
        }
        unreachable;
    }

    /// The ring offset of a device offset inside the ring; null outside it.
    pub fn ringOffsetOf(self: *const Header, dev_off: u64) ?u64 {
        if (self.n_segments == 0) {
            if (dev_off < self.base or dev_off >= self.base + self.ringBytes()) return null;
            return dev_off - self.base;
        }
        var page_base: u64 = 0;
        for (self.segments[0..self.n_segments]) |s| {
            const len = s.pages * journal.PAGE_LEN;
            if (dev_off >= s.start and dev_off < s.start + len) return page_base * journal.PAGE_LEN + (dev_off - s.start);
            page_base += s.pages;
        }
        return null;
    }
};

pub const SLOT_FREE: u32 = 0;
pub const SLOT_FILLING: u32 = 1;
pub const SLOT_WRITTEN: u32 = 2;

pub const Run = extern struct { ring_off: u64, len: u64 };

pub const Slot = extern struct {
    /// `SLOT_*`: free → filling (the claimer's) → written (release) → free.
    state: u32,
    n_runs: u32,
    seq_base: u64,
    /// The window's covering runs: one, or two across the ring's wrap.
    runs: [2]Run,
};

comptime {
    std.debug.assert(@sizeOf(Slot) == 48);
    std.debug.assert(@sizeOf(Segment) == 16);
}

pub const Facts = struct {
    layout: journal.RingLayout,
    grain: u64,
    path: []const u8,
    n_slots: u32,
};

pub const ImageError = error{ BadMagic, BadVersion, Short, PathTooLong };

pub fn slotsOffset() u64 {
    return std.mem.alignForward(u64, @sizeOf(Header), std.atomic.cache_line);
}

pub fn mirrorOffset(n_slots: u32) u64 {
    return std.mem.alignForward(u64, slotsOffset() + @as(u64, n_slots) * @sizeOf(Slot), MIRROR_ALIGN);
}

/// The whole image's length for `facts`.
pub fn imageLen(facts: Facts) u64 {
    return mirrorOffset(facts.n_slots) + facts.layout.ringBytes();
}

pub const Replay = struct {
    /// Slots written to the device, in seq order.
    windows: u32 = 0,
    bytes: u64 = 0,
    /// Slots skipped: free (landed) or still filling (never acked).
    skipped: u32 = 0,
};

/// The replay's device: one write of `bytes` at device offset `off`.
pub const Sink = struct {
    ctx: *anyopaque,
    write: *const fn (ctx: *anyopaque, off: u64, bytes: []const u8) anyerror!void,
};

/// The tail's image over a byte buffer (a memfd mapping, or a test's
/// page-aligned allocation). Every accessor reads the header's offsets.
pub const Image = struct {
    bytes: []align(MIRROR_ALIGN) u8,

    /// Write the header for `facts` over zeroed bytes of `imageLen(facts)`.
    pub fn format(bytes: []align(MIRROR_ALIGN) u8, facts: Facts) ImageError!Image {
        if (facts.path.len > PATH_CAP) return error.PathTooLong;
        if (bytes.len < imageLen(facts)) return error.Short;
        const h: *Header = @ptrCast(bytes.ptr);
        h.* = .{
            .magic = MAGIC,
            .version = VERSION,
            .n_segments = facts.layout.n_segments,
            .base = facts.layout.base,
            .pages = facts.layout.pages,
            .grain = facts.grain,
            .n_slots = facts.n_slots,
            .path_len = @intCast(facts.path.len),
            .slots_off = slotsOffset(),
            .mirror_off = mirrorOffset(facts.n_slots),
            .segments = undefined,
            .path = undefined,
        };
        for (&h.segments, 0..) |*s, i| s.* = .{ .start = facts.layout.segments[i].start, .pages = facts.layout.segments[i].pages };
        @memset(&h.path, 0);
        @memcpy(h.path[0..facts.path.len], facts.path);
        return .{ .bytes = bytes };
    }

    /// Validate a header another process wrote.
    pub fn open(bytes: []align(MIRROR_ALIGN) u8) ImageError!Image {
        if (bytes.len < @sizeOf(Header)) return error.Short;
        const h: *const Header = @ptrCast(bytes.ptr);
        if (!std.mem.eql(u8, &h.magic, &MAGIC)) return error.BadMagic;
        if (h.version != VERSION) return error.BadVersion;
        if (h.path_len > PATH_CAP or h.n_segments > journal.RING_SEGMENTS_MAX) return error.Short;
        if (h.slots_off != slotsOffset() or h.mirror_off != mirrorOffset(h.n_slots)) return error.Short;
        if (bytes.len < h.mirror_off + h.ringBytes()) return error.Short;
        return .{ .bytes = bytes };
    }

    pub fn header(self: Image) *Header {
        return @ptrCast(self.bytes.ptr);
    }

    pub fn slots(self: Image) []Slot {
        const h = self.header();
        const p: [*]Slot = @ptrCast(@alignCast(self.bytes.ptr + h.slots_off));
        return p[0..h.n_slots];
    }

    pub fn mirror(self: Image) []u8 {
        const h = self.header();
        return self.bytes[h.mirror_off..][0..h.ringBytes()];
    }

    /// Claim a slot for a window: copy `ops` into the mirror, record the
    /// covering runs and `seq_base`, publish `written`. Null when the table
    /// is full (the window is then acked at the landing) or an op lies
    /// outside the ring (a caller bug, counted by the caller).
    pub fn claim(self: Image, cursor: *std.atomic.Value(u32), seq_base: u64, ops: []const journal.WriteOp) ?u32 {
        const h = self.header();
        const table = self.slots();
        if (table.len == 0) return null;
        const ring_bytes = h.ringBytes();
        var lo: u64 = std.math.maxInt(u64);
        var hi: u64 = 0;
        var lo_high: u64 = std.math.maxInt(u64);
        var hi_low: u64 = 0;
        var any = false;
        for (ops) |op| {
            if (op.bytes.len == 0) continue;
            const r = h.ringOffsetOf(op.offset) orelse return null;
            if (r + op.bytes.len > ring_bytes) return null;
            any = true;
            lo = @min(lo, r);
            hi = @max(hi, r + op.bytes.len);
            if (r >= ring_bytes / 2) lo_high = @min(lo_high, r) else hi_low = @max(hi_low, r + op.bytes.len);
        }
        if (!any) return null;
        // The slot: a cursor walk with one CAS per probe (windows free in
        // nearly the order they claim, so the probe after the cursor is
        // free in the common case).
        const start = cursor.fetchAdd(1, .monotonic);
        var probe: u32 = 0;
        const idx: u32 = while (probe < table.len) : (probe += 1) {
            const i: u32 = @intCast((start + probe) % table.len);
            // acq_rel pairs with `release`'s release store: the slot's
            // previous contents are dead before this claimer writes.
            if (@cmpxchgStrong(u32, &table[i].state, SLOT_FREE, SLOT_FILLING, .acq_rel, .monotonic) == null) break i;
        } else return null;
        const s = &table[idx];
        s.seq_base = seq_base;
        // The wrap: ops in both halves whose wrapped cover (the high run to
        // the ring's end, the low run from 0) is shorter than the linear
        // one — a window is under half the ring (`conveyor.resolveCaps`
        // caps it at ring/16), so the shorter cover is the window's.
        const wrapped = lo_high != std.math.maxInt(u64) and hi_low != 0 and (ring_bytes - lo_high) + hi_low < hi - lo;
        if (!wrapped) {
            s.n_runs = 1;
            s.runs[0] = .{ .ring_off = lo, .len = hi - lo };
            s.runs[1] = .{ .ring_off = 0, .len = 0 };
        } else {
            s.n_runs = 2;
            s.runs[0] = .{ .ring_off = lo_high, .len = ring_bytes - lo_high };
            s.runs[1] = .{ .ring_off = 0, .len = hi_low };
        }
        const m = self.mirror();
        for (ops) |op| {
            if (op.bytes.len == 0) continue;
            const r = h.ringOffsetOf(op.offset).?;
            @memcpy(m[r..][0..op.bytes.len], op.bytes);
        }
        // release pairs with the replayer's acquire load of `state`: the
        // runs and the mirror bytes are complete before `written` shows.
        @atomicStore(u32, &s.state, SLOT_WRITTEN, .release);
        return idx;
    }

    /// The window landed (or died unanswered): the slot is free.
    pub fn release(self: Image, idx: u32) void {
        // release: the next claimer's acq_rel CAS sees this slot dead.
        @atomicStore(u32, &self.slots()[idx].state, SLOT_FREE, .release);
    }

    pub fn writtenCount(self: Image) u32 {
        var n: u32 = 0;
        for (self.slots()) |*s| if (@atomicLoad(u32, &s.state, .acquire) == SLOT_WRITTEN) {
            n += 1;
        };
        return n;
    }

    /// Write every `written` slot's runs to `sink` in `seq_base` order,
    /// each run split at the ring's segment boundaries. Idempotent: the
    /// bytes are the lane's own image, a landed slot is free and skipped.
    pub fn replay(self: Image, sink: Sink) anyerror!Replay {
        const h = self.header();
        const table = self.slots();
        var out: Replay = .{};
        // Selection in seq order without allocation: the smallest unvisited
        // `seq_base` each round (the table is the lane's in-flight bound —
        // a few thousand at most — and a replay runs once).
        var last_seq: u64 = 0;
        var first = true;
        while (true) {
            var best: ?usize = null;
            for (table, 0..) |*s, i| {
                // acquire pairs with `claim`'s release store.
                if (@atomicLoad(u32, &s.state, .acquire) != SLOT_WRITTEN) continue;
                if (!first and s.seq_base <= last_seq) continue;
                if (best == null or s.seq_base < table[best.?].seq_base) best = i;
            }
            const i = best orelse break;
            const s = &table[i];
            first = false;
            last_seq = s.seq_base;
            for (s.runs[0..@min(s.n_runs, 2)]) |run| {
                if (run.len == 0) continue;
                if (run.ring_off + run.len > h.ringBytes()) return error.Corrupt;
                try self.writeRun(h, sink, run);
                out.bytes += run.len;
            }
            out.windows += 1;
        }
        for (table) |*s| if (@atomicLoad(u32, &s.state, .acquire) != SLOT_WRITTEN) {
            out.skipped += 1;
        };
        return out;
    }

    /// One run from the mirror, page by page across segments (a run is
    /// contiguous in ring space; its pages may be on different extents).
    fn writeRun(self: Image, h: *const Header, sink: Sink, run: Run) anyerror!void {
        const m = self.mirror();
        var off = run.ring_off;
        const end = run.ring_off + run.len;
        while (off < end) {
            const page = off / journal.PAGE_LEN;
            const in_page = off % journal.PAGE_LEN;
            // Pages of one segment are contiguous on the device: the chunk
            // runs to the segment's end or the run's.
            var chunk_end = end;
            if (h.n_segments != 0) {
                var acc: u64 = 0;
                for (h.segments[0..h.n_segments]) |s| {
                    acc += s.pages;
                    if (page < acc) {
                        chunk_end = @min(end, acc * journal.PAGE_LEN);
                        break;
                    }
                }
            }
            const dev_off = h.pageOffset(page) + in_page;
            try sink.write(sink.ctx, dev_off, m[off..chunk_end]);
            off = chunk_end;
        }
    }
};

// ---------------------------------------------------------------------------
// The daemon's tail (Linux): the memfd, size-sealed; the fd the supervisor
// receives; the announce channel.
// ---------------------------------------------------------------------------

/// The supervisor's socket in THIS process (`daemon.detach`'s daemon role
/// sets it; −1 = no supervisor: a foreground mount, `--daemon` alone). The
/// mount announces each writable volume's tail and guard fd over it.
pub var supervisor_sock: std.atomic.Value(i32) = .init(-1);

pub fn hasSupervisor() bool {
    return supervisor_sock.load(.acquire) >= 0;
}

pub const linux_tail = if (builtin.os.tag == .linux) struct {
    const linux = std.os.linux;

    pub const Tail = struct {
        fd: linux.fd_t,
        image: Image,
        cursor: std.atomic.Value(u32) = .init(0),

        pub const CreateError = error{ MemfdFailed, TruncateFailed, MapFailed, SealFailed } || ImageError;

        /// `memfd_create` + `ftruncate` + `mmap(MAP_SHARED)` + the size
        /// seals (`F_SEAL_SHRINK | GROW | SEAL`: the supervisor maps a
        /// length that cannot change under it; writes stay the daemon's).
        pub fn create(facts: Facts) CreateError!Tail {
            const len: usize = @intCast(imageLen(facts));
            const rc = linux.memfd_create("sqz-jrnl-tail", linux.MFD.CLOEXEC | linux.MFD.ALLOW_SEALING);
            if (linux.errno(rc) != .SUCCESS) return error.MemfdFailed;
            const fd: linux.fd_t = @intCast(rc);
            errdefer _ = linux.close(fd);
            if (linux.errno(linux.ftruncate(fd, @intCast(len))) != .SUCCESS) return error.TruncateFailed;
            const map_rc = linux.mmap(null, len, .{ .READ = true, .WRITE = true }, .{ .TYPE = .SHARED }, fd, 0);
            if (linux.errno(map_rc) != .SUCCESS) return error.MapFailed;
            const ptr: [*]align(MIRROR_ALIGN) u8 = @ptrFromInt(map_rc);
            errdefer _ = linux.munmap(@ptrCast(ptr), len);
            const seals: u32 = linux.F.SEAL_SHRINK | linux.F.SEAL_GROW | linux.F.SEAL_SEAL;
            if (linux.errno(linux.fcntl(fd, linux.F.ADD_SEALS, seals)) != .SUCCESS) return error.SealFailed;
            const image = try Image.format(ptr[0..len], facts);
            return .{ .fd = fd, .image = image };
        }

        pub fn deinit(self: *Tail) void {
            _ = linux.munmap(@ptrCast(self.image.bytes.ptr), self.image.bytes.len);
            _ = linux.close(self.fd);
        }

        pub fn claim(self: *Tail, seq_base: u64, ops: []const journal.WriteOp) ?u32 {
            return self.image.claim(&self.cursor, seq_base, ops);
        }

        pub fn release(self: *Tail, idx: u32) void {
            self.image.release(idx);
        }
    };

    /// The device as the replay's sink: `pwrite` (buffered — the daemon is
    /// dead, nothing else has the device open for writing).
    const DeviceSink = struct {
        fd: linux.fd_t,

        fn write(ctx: *anyopaque, off: u64, bytes: []const u8) anyerror!void {
            const self: *DeviceSink = @ptrCast(@alignCast(ctx));
            var done: usize = 0;
            while (done < bytes.len) {
                const rc = linux.pwrite(self.fd, bytes.ptr + done, bytes.len - done, @intCast(off + done));
                switch (linux.errno(rc)) {
                    .SUCCESS => {
                        if (rc == 0) return error.InputOutput;
                        done += @intCast(rc);
                    },
                    .INTR => continue,
                    else => return error.InputOutput,
                }
            }
        }
    };

    pub const ReplayFdError = error{ StatFailed, MapFailed, DeviceOpenFailed, DeviceWriteFailed, BarrierFailed } || ImageError;

    /// The supervisor's replay of one tail fd: map it read-only, open the
    /// header's device, write every written slot in seq order, `fsync`.
    /// `path_out` receives the device path (for the log line).
    pub fn replayFd(fd: linux.fd_t, path_out: *[PATH_CAP]u8, path_len: *usize) ReplayFdError!Replay {
        var st: linux.Statx = undefined;
        if (linux.errno(linux.statx(fd, "", linux.AT.EMPTY_PATH, .{ .SIZE = true }, &st)) != .SUCCESS) return error.StatFailed;
        const len: usize = @intCast(st.size);
        const map_rc = linux.mmap(null, len, .{ .READ = true }, .{ .TYPE = .SHARED }, fd, 0);
        if (linux.errno(map_rc) != .SUCCESS) return error.MapFailed;
        const ptr: [*]align(MIRROR_ALIGN) u8 = @ptrFromInt(map_rc);
        defer _ = linux.munmap(@ptrCast(ptr), len);
        const image = try Image.open(ptr[0..len]);
        const h = image.header();
        @memcpy(path_out[0..h.path_len], h.pathSlice());
        path_len.* = h.path_len;
        var zbuf: [PATH_CAP + 1]u8 = undefined;
        @memcpy(zbuf[0..h.path_len], h.pathSlice());
        zbuf[h.path_len] = 0;
        const dev_rc = linux.open(zbuf[0..h.path_len :0], .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
        if (linux.errno(dev_rc) != .SUCCESS) return error.DeviceOpenFailed;
        var sink: DeviceSink = .{ .fd = @intCast(dev_rc) };
        defer _ = linux.close(sink.fd);
        const out = image.replay(.{ .ctx = &sink, .write = DeviceSink.write }) catch return error.DeviceWriteFailed;
        if (linux.errno(linux.fsync(sink.fd)) != .SUCCESS) return error.BarrierFailed;
        return out;
    }
} else struct {};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

const MemDevice = struct {
    bytes: []u8,
    writes: usize = 0,

    fn write(ctx: *anyopaque, off: u64, bytes: []const u8) anyerror!void {
        const self: *MemDevice = @ptrCast(@alignCast(ctx));
        if (off + bytes.len > self.bytes.len) return error.OutOfRange;
        @memcpy(self.bytes[@intCast(off)..][0..bytes.len], bytes);
        self.writes += 1;
    }

    fn sink(self: *MemDevice) Sink {
        return .{ .ctx = self, .write = write };
    }
};

fn testFacts(layout: journal.RingLayout, n_slots: u32) Facts {
    return .{ .layout = layout, .grain = 512, .path = "/dev/sqz-test", .n_slots = n_slots };
}

fn allocImage(facts: Facts) ![]align(MIRROR_ALIGN) u8 {
    const bytes = try testing.allocator.alignedAlloc(u8, .fromByteUnits(MIRROR_ALIGN), @intCast(imageLen(facts)));
    @memset(bytes, 0);
    return bytes;
}

test "codec: the header round-trips through format/open; a wrong magic, version or a short buffer is refused; offsets are page-aligned for the mirror" {
    const layout: journal.RingLayout = .{ .base = 1 << 20, .pages = 8 };
    const facts = testFacts(layout, 16);
    const bytes = try allocImage(facts);
    defer testing.allocator.free(bytes);
    _ = try Image.format(bytes, facts);
    const img = try Image.open(bytes);
    const h = img.header();
    try testing.expectEqual(@as(u64, 1 << 20), h.base);
    try testing.expectEqual(@as(u64, 8), h.pages);
    try testing.expectEqual(@as(u32, 16), h.n_slots);
    try testing.expectEqual(@as(u64, 512), h.grain);
    try testing.expectEqualStrings("/dev/sqz-test", h.pathSlice());
    try testing.expectEqual(@as(u64, 0), h.mirror_off % MIRROR_ALIGN);
    try testing.expectEqual(@as(usize, 16), img.slots().len);
    try testing.expectEqual(@as(usize, 8 * journal.PAGE_LEN), img.mirror().len);
    try testing.expectEqual(@as(?u64, 0), h.ringOffsetOf(1 << 20));
    try testing.expectEqual(@as(?u64, null), h.ringOffsetOf((1 << 20) - 1));
    try testing.expectEqual(@as(?u64, null), h.ringOffsetOf((1 << 20) + 8 * journal.PAGE_LEN));
    // Refusals.
    bytes[0] ^= 1;
    try testing.expectError(error.BadMagic, Image.open(bytes));
    bytes[0] ^= 1;
    h.version += 1;
    try testing.expectError(error.BadVersion, Image.open(bytes));
    h.version -= 1;
    try testing.expectError(error.Short, Image.open(bytes[0..@sizeOf(Header)]));
    try testing.expectError(error.Short, Image.open(@alignCast(bytes[0 .. bytes.len - 1])));
}

test "claim + replay: a claimed window's bytes reach the device at the ops' offsets, in seq order across slots; a released (landed) slot is skipped; a second replay rewrites the same bytes (idempotent); a filling slot is never replayed" {
    const layout: journal.RingLayout = .{ .base = 4096, .pages = 4 };
    const facts = testFacts(layout, 4);
    const bytes = try allocImage(facts);
    defer testing.allocator.free(bytes);
    const img = try Image.format(bytes, facts);
    var cursor: std.atomic.Value(u32) = .init(0);
    var dev_bytes: [5 * 4096]u8 = @splat(0);
    var dev: MemDevice = .{ .bytes = &dev_bytes };

    // Two windows out of seq order: seq 200 claimed first, seq 100 second.
    const a_bytes: [1024]u8 = @splat(0xAA);
    const b_bytes: [512]u8 = @splat(0xBB);
    const b_hdr: [24]u8 = @splat(0xB0);
    const sa = img.claim(&cursor, 200, &.{.{ .offset = 4096 + 8192, .bytes = &a_bytes }}).?;
    const sb = img.claim(&cursor, 100, &.{ .{ .offset = 4096 + 4096, .bytes = &b_hdr }, .{ .offset = 4096 + 4096 + 24, .bytes = &b_bytes } }).?;
    try testing.expect(sa != sb);
    try testing.expectEqual(@as(u32, 2), img.writtenCount());
    // The covering run of the unpadded window spans header + entry.
    try testing.expectEqual(@as(u32, 1), img.slots()[sb].n_runs);
    try testing.expectEqual(@as(u64, 4096), img.slots()[sb].runs[0].ring_off);
    try testing.expectEqual(@as(u64, 24 + 512), img.slots()[sb].runs[0].len);

    var order: std.ArrayList(u64) = .empty;
    defer order.deinit(testing.allocator);
    const Order = struct {
        inner: *MemDevice,
        seen: *std.ArrayList(u64),
        fn write(ctx: *anyopaque, off: u64, b: []const u8) anyerror!void {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            try self.seen.append(testing.allocator, off);
            try MemDevice.write(self.inner, off, b);
        }
    };
    var ord: Order = .{ .inner = &dev, .seen = &order };
    const r1 = try img.replay(.{ .ctx = &ord, .write = Order.write });
    try testing.expectEqual(@as(u32, 2), r1.windows);
    try testing.expectEqual(@as(u64, 1024 + 24 + 512), r1.bytes);
    try testing.expectEqual(@as(u32, 2), r1.skipped);
    // Seq order: 100 (at 8192) before 200 (at 12288).
    try testing.expectEqual(@as(usize, 2), order.items.len);
    try testing.expectEqual(@as(u64, 4096 + 4096), order.items[0]);
    try testing.expectEqual(@as(u64, 4096 + 8192), order.items[1]);
    try testing.expectEqualSlices(u8, &b_hdr, dev_bytes[8192..][0..24]);
    try testing.expectEqualSlices(u8, &b_bytes, dev_bytes[8192 + 24 ..][0..512]);
    try testing.expectEqualSlices(u8, &a_bytes, dev_bytes[12288..][0..1024]);
    try testing.expectEqual(@as(u8, 0), dev_bytes[8192 + 24 + 512]);

    // The landing of seq 100 frees its slot: only seq 200 replays; the
    // device image is unchanged by the rewrite.
    img.release(sb);
    const before = dev_bytes;
    const r2 = try img.replay(dev.sink());
    try testing.expectEqual(@as(u32, 1), r2.windows);
    try testing.expectEqual(@as(u64, 1024), r2.bytes);
    try testing.expectEqualSlices(u8, &before, &dev_bytes);

    // A slot mid-fill is not replayed.
    img.slots()[sb].state = SLOT_FILLING;
    img.slots()[sb].seq_base = 50;
    const r3 = try img.replay(dev.sink());
    try testing.expectEqual(@as(u32, 1), r3.windows);
    img.slots()[sb].state = SLOT_FREE;

    // Refusals at claim: an op outside the ring; a full table.
    try testing.expectEqual(@as(?u32, null), img.claim(&cursor, 300, &.{.{ .offset = 0, .bytes = &b_bytes }}));
    try testing.expectEqual(@as(?u32, null), img.claim(&cursor, 300, &.{}));
    var filled: [4]u32 = undefined;
    var n: usize = 0;
    while (img.claim(&cursor, 400 + n, &.{.{ .offset = 4096, .bytes = &b_bytes }})) |i| : (n += 1) filled[n] = i;
    try testing.expectEqual(@as(usize, 3), n);
    for (filled[0..n]) |i| img.release(i);
    img.release(sa);
    try testing.expectEqual(@as(u32, 0), img.writtenCount());
}

test "claim: a window across the ring's wrap records two runs (the high run to the ring's end, the low run from 0) and replays both; a segmented ring's run is split at the segment boundary" {
    // A wrapped window on a 4-page ring: 512 B at the last page's end and
    // 512 B at page 0's start.
    {
        const layout: journal.RingLayout = .{ .base = 0, .pages = 4 };
        const facts = testFacts(layout, 2);
        const bytes = try allocImage(facts);
        defer testing.allocator.free(bytes);
        const img = try Image.format(bytes, facts);
        var cursor: std.atomic.Value(u32) = .init(0);
        const hi: [512]u8 = @splat(0x11);
        const lo: [512]u8 = @splat(0x22);
        const s = img.claim(&cursor, 7, &.{ .{ .offset = 4 * 4096 - 512, .bytes = &hi }, .{ .offset = 0, .bytes = &lo } }).?;
        try testing.expectEqual(@as(u32, 2), img.slots()[s].n_runs);
        try testing.expectEqual(@as(u64, 4 * 4096 - 512), img.slots()[s].runs[0].ring_off);
        try testing.expectEqual(@as(u64, 512), img.slots()[s].runs[0].len);
        try testing.expectEqual(@as(u64, 0), img.slots()[s].runs[1].ring_off);
        try testing.expectEqual(@as(u64, 512), img.slots()[s].runs[1].len);
        var dev_bytes: [4 * 4096]u8 = @splat(0);
        var dev: MemDevice = .{ .bytes = &dev_bytes };
        const r = try img.replay(dev.sink());
        try testing.expectEqual(@as(u32, 1), r.windows);
        try testing.expectEqual(@as(u64, 1024), r.bytes);
        try testing.expectEqualSlices(u8, &hi, dev_bytes[4 * 4096 - 512 ..]);
        try testing.expectEqualSlices(u8, &lo, dev_bytes[0..512]);
    }
    // Two segments of one page each at device offsets 64 KiB and 16 KiB: a
    // run covering both pages lands on both extents.
    {
        const layout = try journal.RingLayout.fromSegments(&.{ .{ .start = 65536, .len = 4096 }, .{ .start = 16384, .len = 4096 } });
        const facts = testFacts(layout, 2);
        const bytes = try allocImage(facts);
        defer testing.allocator.free(bytes);
        const img = try Image.format(bytes, facts);
        try testing.expectEqual(@as(?u64, 4096), img.header().ringOffsetOf(16384));
        try testing.expectEqual(@as(?u64, 100), img.header().ringOffsetOf(65536 + 100));
        var cursor: std.atomic.Value(u32) = .init(0);
        const p0: [4096]u8 = @splat(0x33);
        const p1: [1024]u8 = @splat(0x44);
        _ = img.claim(&cursor, 1, &.{ .{ .offset = 65536, .bytes = &p0 }, .{ .offset = 16384, .bytes = &p1 } }).?;
        var dev_bytes: [65536 + 4096]u8 = @splat(0);
        var dev: MemDevice = .{ .bytes = &dev_bytes };
        const r = try img.replay(dev.sink());
        try testing.expectEqual(@as(u32, 1), r.windows);
        try testing.expectEqual(@as(u64, 4096 + 1024), r.bytes);
        try testing.expectEqual(@as(usize, 2), dev.writes);
        try testing.expectEqualSlices(u8, &p0, dev_bytes[65536..][0..4096]);
        try testing.expectEqualSlices(u8, &p1, dev_bytes[16384..][0..1024]);
    }
}

test "linux: the memfd tail is size-sealed, claims through the mapping, and replayFd writes the written slots to the header's device and barriers" {
    if (builtin.os.tag != .linux) return testkit.skip(@src(), .no_linux, "the memfd tail is Linux only");
    const linux = std.os.linux;
    var dev_path_buf: [128]u8 = undefined;
    const dev_path = try std.fmt.bufPrintSentinel(&dev_path_buf, "/build/tmp/sqz-jrnl-tail-{d}.img", .{linux.getpid()}, 0);
    _ = linux.mkdirat(linux.AT.FDCWD, "/build/tmp", 0o755);
    const dev_rc = linux.open(dev_path, .{ .ACCMODE = .RDWR, .CLOEXEC = true, .CREAT = true, .TRUNC = true }, 0o644);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(dev_rc));
    const dev_fd: linux.fd_t = @intCast(dev_rc);
    defer _ = linux.close(dev_fd);
    defer _ = linux.unlink(dev_path);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.ftruncate(dev_fd, 8 * 4096)));

    const layout: journal.RingLayout = .{ .base = 4096, .pages = 4 };
    var tail = try linux_tail.Tail.create(.{ .layout = layout, .grain = 512, .path = dev_path, .n_slots = 8 });
    defer tail.deinit();
    // Sealed: growing or shrinking is refused.
    try testing.expectEqual(linux.E.PERM, linux.errno(linux.ftruncate(tail.fd, 4096)));
    const payload: [2048]u8 = @splat(0x5A);
    const slot = tail.claim(11, &.{.{ .offset = 4096 + 4096, .bytes = &payload }}).?;
    var pbuf: [PATH_CAP]u8 = undefined;
    var plen: usize = 0;
    const r = try linux_tail.replayFd(tail.fd, &pbuf, &plen);
    try testing.expectEqual(@as(u32, 1), r.windows);
    try testing.expectEqual(@as(u64, 2048), r.bytes);
    try testing.expectEqualStrings(dev_path, pbuf[0..plen]);
    var got: [2048 + 1]u8 = undefined;
    const n = linux.pread(dev_fd, &got, got.len, 8192);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(n));
    try testing.expectEqual(got.len, n);
    try testing.expectEqualSlices(u8, &payload, got[0..2048]);
    try testing.expectEqual(@as(u8, 0), got[2048]);
    tail.release(slot);
    const r2 = try linux_tail.replayFd(tail.fd, &pbuf, &plen);
    try testing.expectEqual(@as(u32, 0), r2.windows);
}
