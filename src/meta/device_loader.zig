//! The metadata device binding (Linux): the node cache's `NodeLoader` over
//! the io_uring file layer and the registered metadata device, the
//! checkpoint cycle's device sinks, the format plan's writer, and the
//! mount's range reads (charter §5.1 "Device posture": O_DIRECT from the
//! first byte, a misaligned product write REFUSED and counted, a narrow
//! read widened and sliced; §5.3 the checkpoint order; §6.4's "barrier only
//! what the op touched"; survey §7.1 `node_cache.zig` "byte images travel
//! through a `NodeLoader` the device binding implements").
//!
//! ## Which barrier covers which bytes
//!
//! One metadata volume is one device (one inode): `fdatasync` on ANY fd of
//! it flushes every dirty byte of that inode and issues the device's cache
//! flush — so `barrier()` here (the pool's `IORING_FSYNC_DATASYNC` on its
//! cached fd) covers everything written to the device BEFORE it started
//! through any fd: node appends and images (this loader), bitmap pages and
//! ledger slots (the sinks), the recovery pad (`writeOps`), AND the journal
//! lane's entries — user windows and the structural SMO/compensation
//! entries alike (its own fd on the same inode). The lane's barrier (an
//! `FSYNC|DATASYNC` SQE on the lane's ring) covers the same population from
//! its side. "Barrier only what the op touched" holds at
//! the device grain: a checkpoint barriers its own volume, never a
//! sibling's; nothing here reaches another device.
//!
//! The checkpoint cycle's order rides these sinks unchanged (charter §5.3):
//! appends → bitmap pages → barrier #1 → the ledger slot → (barrier #2).

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;

const counters = @import("../core/counters.zig");
const log = @import("../core/log.zig");
const align_probe = @import("../io/align_probe.zig");
const uring_fs = @import("../io/uring_fs.zig");
const node_cache = @import("node_cache.zig");
const checkpoint_cycle = @import("checkpoint_cycle.zig");
const ledger = @import("ledger.zig");
const alloc_ext = @import("alloc_ext.zig");
const journal = @import("journal.zig");
const format = @import("format.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("device_loader is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const LoaderError = node_cache.LoaderError;

/// `BLKGETSIZE64` = `_IOR(0x12, 114, size_t)`: the block device's byte
/// length (a regular file's is its `st_size`).
pub const BLKGETSIZE64: u32 = 0x80081272;

pub const Stats = struct {
    /// Extent reads served to the cache (`meta_io_node_reads`) and the
    /// bytes copied out of the pool's widened buffers.
    node_reads: counters.ShardedCounter = .{},
    node_read_bytes: counters.ShardedCounter = .{},
    appends: counters.ShardedCounter = .{},
    images: counters.ShardedCounter = .{},
    ledger_slots: counters.ShardedCounter = .{},
    bitmap_pages: counters.ShardedCounter = .{},
    barriers: counters.ShardedCounter = .{},
    /// Device errors answered `error.Io` to the cache / the cycle.
    io_errors: counters.ShardedCounter = .{},
};

pub var stats: Stats = .{};

/// Test seams (`SQUEEZEFS_TEST_*` class): the next N barriers through this
/// module fail synthetically with `fail_errno`'s class before the pool
/// sees them — the checkpointer's fence site (`syncDevice`) on a plain
/// file.
pub const TestSeams = struct {
    fail_barriers: std.atomic.Value(u32) = .init(0),
    /// `@backingInt` of a `linux.E`; 0 = EIO.
    fail_errno: std.atomic.Value(u32) = .init(0),
};

pub var test_seams: TestSeams = .{};

fn takeInjectedBarrierFailure() ?uring_fs.Error {
    if (test_seams.fail_barriers.load(.acquire) == 0) return null;
    _ = test_seams.fail_barriers.fetchSub(1, .acq_rel);
    const raw = test_seams.fail_errno.load(.acquire);
    if (raw == 0) return error.InputOutput;
    return uring_fs.fromErrno(@fromBackingInt(@intCast(@as(u16, @intCast(raw)))));
}

/// The binding: the io pool, the device path, its registered posture.
pub const DeviceLoader = struct {
    gpa: Allocator,
    pool: *uring_fs.Pool,
    path: []const u8,
    mode: uring_fs.MetaIoMode,
    /// Region bases the sinks add (`ledger.slotOffset`, `PageWrite.offset`).
    ledger_base: u64,
    bitmap_base: u64,
    /// The volume's FAILED latch once the lane exists: every device error
    /// through this binding — a sink write, a node append or image, the
    /// checkpointer's barrier — is classified for the FENCE rung here, the
    /// ONE funnel of the checkpoint side (charter §5.7: a reservation
    /// conflict at ANY barrier latches FAILED; the node/ledger/bitmap
    /// writes are fence sites too, DMT:173). Null before the lane spawns
    /// (the mount's reads) and in offline tools.
    latch: ?*journal.FailureLatch = null,

    pub fn loader(self: *DeviceLoader) node_cache.NodeLoader {
        return .{ .ctx = self, .readExtent = readExtent, .appendBset = appendBset, .writeImage = writeImage };
    }

    pub fn barrierSink(self: *DeviceLoader) checkpoint_cycle.BarrierSink {
        return .{ .ctx = self, .barrier = barrierFn };
    }

    pub fn ledgerSink(self: *DeviceLoader) checkpoint_cycle.LedgerSink {
        return .{ .ctx = self, .writeSlot = writeSlotFn };
    }

    pub fn pageSink(self: *DeviceLoader) checkpoint_cycle.PageSink {
        return .{ .ctx = self, .writePage = writePageFn };
    }

    fn ioFailed(self: *const DeviceLoader, what: []const u8, offset: u64, len: usize, e: uring_fs.Error) LoaderError {
        stats.io_errors.inc();
        log.err("metadata device {s}: {s} at offset {d} ({d} B) failed: {s} ({s})", .{ self.path, what, offset, len, @errorName(e), @tagName(uring_fs.classOf(e)) });
        if (self.latch) |l| _ = l.classifyFence(e, what);
        return error.Io;
    }

    // ---- the NodeLoader ----

    /// One extent read, widened to the grain by the pool (`readSpan`) and
    /// copied into the cache's buffer.
    fn readExtent(ctx: *anyopaque, addr: u64, buf: []u8) LoaderError!void {
        const self: *DeviceLoader = @ptrCast(@alignCast(ctx));
        var r = self.pool.readAt(self.path, addr, buf.len) catch |e| return self.ioFailed("node read", addr, buf.len, e);
        defer r.deinit();
        const got = r.bytes();
        if (got.len != buf.len) {
            stats.io_errors.inc();
            log.err("metadata device {s}: node read at offset {d} returned {d} of {d} B (a short extent)", .{ self.path, addr, got.len, buf.len });
            return error.Io;
        }
        @memcpy(buf, got);
        stats.node_reads.inc();
        stats.node_read_bytes.add(buf.len);
    }

    /// A 4 KiB-padded frame at `addr + off`: the posture law applies (a
    /// misaligned product write is refused by `prepareDirectWrite` and
    /// counted on `meta_io_unaligned_refusals`).
    fn appendBset(ctx: *anyopaque, addr: u64, off: usize, bytes: []const u8) LoaderError!void {
        const self: *DeviceLoader = @ptrCast(@alignCast(ctx));
        self.pool.writeAt(self.path, addr + off, bytes) catch |e| return self.ioFailed("bset append", addr + off, bytes.len, e);
        stats.appends.inc();
    }

    fn writeImage(ctx: *anyopaque, addr: u64, bytes: []const u8) LoaderError!void {
        const self: *DeviceLoader = @ptrCast(@alignCast(ctx));
        self.pool.writeAt(self.path, addr, bytes) catch |e| return self.ioFailed("node image", addr, bytes.len, e);
        stats.images.inc();
    }

    // ---- the checkpoint sinks ----

    pub fn barrier(self: *DeviceLoader) error{Io}!void {
        if (takeInjectedBarrierFailure()) |e| return self.ioFailed("fdatasync", 0, 0, e);
        self.pool.fdatasync(self.path) catch |e| return self.ioFailed("fdatasync", 0, 0, e);
        stats.barriers.inc();
    }

    fn barrierFn(ctx: *anyopaque) checkpoint_cycle.IoError!void {
        const self: *DeviceLoader = @ptrCast(@alignCast(ctx));
        return self.barrier();
    }

    fn writeSlotFn(ctx: *anyopaque, slot: u64, image: *const [ledger.ROOT_LEDGER_SLOT_LEN]u8) checkpoint_cycle.IoError!void {
        const self: *DeviceLoader = @ptrCast(@alignCast(ctx));
        const off = ledger.slotOffset(self.ledger_base, slot);
        self.pool.writeAt(self.path, off, image) catch |e| return self.ioFailed("ledger slot", off, image.len, e);
        stats.ledger_slots.inc();
    }

    fn writePageFn(ctx: *anyopaque, offset: u64, image: *const [alloc_ext.page_len]u8) checkpoint_cycle.IoError!void {
        const self: *DeviceLoader = @ptrCast(@alignCast(ctx));
        const off = self.bitmap_base + offset;
        self.pool.writeAt(self.path, off, image) catch |e| return self.ioFailed("bitmap page", off, image.len, e);
        stats.bitmap_pages.inc();
    }

    // ---- ranges, ops, the format plan ----

    /// A batch of journal ops (an SMO entry's aligned runs) as ONE pool
    /// submission.
    pub fn writeOps(self: *DeviceLoader, ops: []const journal.WriteOp) error{Io}!void {
        var buf: [16]uring_fs.WriteOp = undefined;
        const converted: []uring_fs.WriteOp = if (ops.len <= buf.len) buf[0..ops.len] else self.gpa.alloc(uring_fs.WriteOp, ops.len) catch {
            stats.io_errors.inc();
            return error.Io;
        };
        defer if (ops.len > buf.len) self.gpa.free(converted);
        for (ops, converted) |op, *c| c.* = .{ .offset = op.offset, .data = op.bytes };
        self.pool.writeAtBatch(self.path, converted) catch |e| return self.ioFailed("journal entry", if (ops.len > 0) ops[0].offset else 0, 0, e);
    }

    /// Read `[offset, offset + len)` into a fresh buffer (the mount's
    /// superblock, ledger, bitmap and ring reads); a short read is
    /// zero-extended (zeros verify nothing — the decoders' contract).
    pub fn readRange(self: *DeviceLoader, gpa: Allocator, offset: u64, len: usize) error{ Io, OutOfMemory }![]u8 {
        const out = try gpa.alloc(u8, len);
        errdefer gpa.free(out);
        @memset(out, 0);
        var r = self.pool.readAt(self.path, offset, len) catch |e| {
            self.ioFailed("range read", offset, len, e) catch {};
            return error.Io;
        };
        defer r.deinit();
        const got = r.bytes();
        @memcpy(out[0..got.len], got);
        return out;
    }

    /// Execute a format plan in its order: every step a direct write, every
    /// `barrier_after` an `fdatasync` (the ledger, the tail superblock copy,
    /// sector 0 — three on a flat plan).
    pub fn writeSteps(self: *DeviceLoader, steps: []const format.Step) error{Io}!void {
        for (steps) |s| {
            self.pool.writeAt(self.path, s.offset, s.bytes) catch |e| return self.ioFailed("format step", s.offset, s.bytes.len, e);
            if (s.barrier_after) try self.barrier();
        }
    }
};

/// The volume's byte length: a regular file's size, a block device's
/// `BLKGETSIZE64`.
pub fn volumeLength(path: []const u8) error{ CannotStat, NotADeviceOrFile, Unexpected }!u64 {
    const st = (align_probe.statPath(path) catch return error.CannotStat) orelse return error.CannotStat;
    if (st.is_file) return st.size;
    if (!st.is_bdev) return error.NotADeviceOrFile;
    var zbuf: [linux.PATH_MAX]u8 = undefined;
    const z = align_probe.pathZ(&zbuf, path) catch return error.CannotStat;
    const rc = linux.open(z, .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) return error.Unexpected;
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    var size: u64 = 0;
    if (linux.errno(linux.ioctl(fd, BLKGETSIZE64, @intFromPtr(&size))) != .SUCCESS) return error.Unexpected;
    return size;
}

// ---------------------------------------------------------------------------
// Tests (Linux; a regular file on the venue's ext4 under /build/tmp, O_DIRECT
// through `registerMetaDevice`).
// ---------------------------------------------------------------------------

const testing = std.testing;
const node = @import("node.zig");
const key = @import("key.zig");

const TestDevice = struct {
    gpa: Allocator,
    env: std.process.Environ.Map,
    pool: *uring_fs.Pool,
    path_buf: [128]u8 = undefined,
    path_len: usize = 0,
    dev: DeviceLoader,

    fn path(self: *const TestDevice) []const u8 {
        return self.path_buf[0..self.path_len];
    }

    fn create(gpa: Allocator, name: []const u8, len: u64) !*TestDevice {
        const self = try gpa.create(TestDevice);
        errdefer gpa.destroy(self);
        self.* = .{ .gpa = gpa, .env = std.process.Environ.Map.init(gpa), .pool = undefined, .dev = undefined };
        errdefer self.env.deinit();
        var zbuf: [linux.PATH_MAX]u8 = undefined;
        _ = linux.mkdirat(linux.AT.FDCWD, try align_probe.pathZ(&zbuf, "/build/tmp"), 0o755);
        self.path_len = (try std.fmt.bufPrint(&self.path_buf, "/build/tmp/sqz-devl-{d}-{s}.img", .{ linux.getpid(), name })).len;
        const z = try align_probe.pathZ(&zbuf, self.path());
        const rc = linux.open(z, .{ .ACCMODE = .RDWR, .CLOEXEC = true, .CREAT = true, .TRUNC = true }, 0o644);
        if (linux.errno(rc) != .SUCCESS) return error.CannotCreateDevice;
        const fd: linux.fd_t = @intCast(rc);
        defer _ = linux.close(fd);
        if (linux.errno(linux.ftruncate(fd, @intCast(len))) != .SUCCESS) return error.CannotSizeDevice;
        const mode = try uring_fs.registerMetaDevice(gpa, &self.env, self.path());
        self.pool = try uring_fs.Pool.init(gpa, uring_fs.Config.fixed(2, 1024));
        self.dev = .{ .gpa = gpa, .pool = self.pool, .path = self.path(), .mode = mode, .ledger_base = journal.ROOT_LEDGER_START, .bitmap_base = 0 };
        return self;
    }

    fn destroy(self: *TestDevice) void {
        self.pool.deinit();
        var zbuf: [linux.PATH_MAX]u8 = undefined;
        if (align_probe.pathZ(&zbuf, self.path())) |z| {
            _ = linux.unlink(z);
        } else |_| {}
        self.env.deinit();
        self.gpa.destroy(self);
    }
};

test "the loader round-trips a node image and an appended bset through O_DIRECT; a misaligned product write is REFUSED and counted; the sinks land at their bases; the volume length is the file's" {
    const gpa = testing.allocator;
    const d = try TestDevice.create(gpa, "loader", 8 << 20);
    defer d.destroy();
    try testing.expect(d.dev.mode.isDirect());
    try testing.expectEqual(@as(u64, 8 << 20), try volumeLength(d.path()));
    const layout = try node.NodeLayout.new(node.MIN_NODE_SIZE, null);
    const addr: u64 = 2 << 20;
    var w = try node.writeNode(gpa, layout, .{ .node_addr = addr, .node_seq = 7, .tree_id = key.KIND_INTERIOR, .level = 0, .min_key = "", .max_key = &key.KEY_SPACE_MAX }, &.{}, 0, null);
    defer w.deinit(gpa);
    const ld = d.dev.loader();
    try ld.writeImage(ld.ctx, addr, w.image);
    // A 4 KiB frame appended at the tail page.
    const frame = try uring_fs.AlignedBuf.zeroed(gpa, 4096, 4096);
    defer {
        var f = frame;
        f.deinit();
    }
    @memset(frame.bytes, 0xA5);
    try ld.appendBset(ld.ctx, addr, w.image.len, frame.bytes);
    const buf = try gpa.alloc(u8, node.MIN_NODE_SIZE);
    defer gpa.free(buf);
    try ld.readExtent(ld.ctx, addr, buf);
    try testing.expectEqualSlices(u8, w.image, buf[0..w.image.len]);
    try testing.expect(std.mem.allEqual(u8, buf[w.image.len .. w.image.len + 4096], 0xA5));
    // The posture law: a 100-byte append is refused before any byte lands.
    const refusals0 = uring_fs.stats.unaligned_refusals.load();
    const errors0 = stats.io_errors.load();
    // Two loud lines: the posture tripwire (rate-limited to one per site
    // per second, so another suite's firing may have taken this second's)
    // and the loader's own refusal.
    log.testing.expectErrorLogs(2);
    try testing.expectError(error.Io, ld.appendBset(ld.ctx, addr, w.image.len + 4096, frame.bytes[0..100]));
    try testing.expect(log.testing.capturedErrorLogs() >= 1);
    log.testing.expectErrorLogs(0);
    try testing.expectEqual(refusals0 + 1, uring_fs.stats.unaligned_refusals.load());
    try testing.expectEqual(errors0 + 1, stats.io_errors.load());
    // The sinks: a ledger slot at base + slot × 4096, a bitmap page at its
    // offset, a barrier through the pool.
    var slot: [ledger.ROOT_LEDGER_SLOT_LEN]u8 = @splat(0x11);
    const ls = d.dev.ledgerSink();
    try ls.writeSlot(ls.ctx, 3, &slot);
    d.dev.bitmap_base = 1 << 20;
    var page: [alloc_ext.page_len]u8 = @splat(0x22);
    const ps = d.dev.pageSink();
    try ps.writePage(ps.ctx, 8192, &page);
    const bs = d.dev.barrierSink();
    try bs.barrier(bs.ctx);
    const got_slot = try d.dev.readRange(gpa, journal.ROOT_LEDGER_START + 3 * 4096, 4096);
    defer gpa.free(got_slot);
    try testing.expectEqualSlices(u8, &slot, got_slot);
    const got_page = try d.dev.readRange(gpa, (1 << 20) + 8192, 4096);
    defer gpa.free(got_page);
    try testing.expectEqualSlices(u8, &page, got_page);
    // A read past the end is zero-extended, never an error.
    const tail = try d.dev.readRange(gpa, (8 << 20) - 4096, 8192);
    defer gpa.free(tail);
    try testing.expect(std.mem.allEqual(u8, tail[4096..], 0));
}

test "a format plan written through the device lands byte-identical to the in-RAM image, with its barriers in plan order" {
    const gpa = testing.allocator;
    const len: u64 = 64 << 20;
    const d = try TestDevice.create(gpa, "format", len);
    defer d.destroy();
    const opts: format.FormatOptions = .{ .node_size = 65536, .journal_len_override = 1 << 20, .uuid = "device-loader-t!".*, .set = .{ .set_uuid = "device-loader-s!".*, .hash_seed = 0x5EED_CAFE_F00D_D00D } };
    var plan = try format.planFormat(gpa, len, opts, null);
    defer plan.deinit();
    const barriers0 = stats.barriers.load();
    try d.dev.writeSteps(plan.steps);
    try testing.expectEqual(barriers0 + plan.barrierCount(), stats.barriers.load());
    const image = try format.formatImage(gpa, len, opts);
    defer gpa.free(image);
    // Compare the fixed region and the heap's first extents (the rest is
    // untouched zeros on both).
    const head = try d.dev.readRange(gpa, 0, @intCast(plan.superblock.heap.start + 4 * 65536));
    defer gpa.free(head);
    try testing.expectEqualSlices(u8, image[0..head.len], head);
    const tail = try d.dev.readRange(gpa, len - 4096, 4096);
    defer gpa.free(tail);
    try testing.expectEqualSlices(u8, image[len - 4096 ..], tail);
}
