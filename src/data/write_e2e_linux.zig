//! The ONE Linux end-to-end of the write path over a real `nvme_dev.Device`
//! on a `/build/tmp` file (O_DIRECT, the io_uring write lanes, the DUR-2
//! barrier): the device seam is the product binding, the block-map
//! authority and the frees ledger are the RAM fakes (the KV layout publish
//! binds in wave 4). Rows: a 4 MiB sequential write (write-through: 1 copy
//! + 1 DMA counted); 4 KiB random overwrites on a published block (W1
//! engaged, `patch_edge_rmw_reads == 0`, zero device reads); out-of-order
//! O_DIRECT-style pieces (the coverage union completes, one upload, no seed
//! read); a partial tail (staging absorbs the spill; the flush unit DMAs
//! straight off the ring); fsync phases summing exactly; a torn DMA (the
//! device's injected EIO) never joining coverage.

const std = @import("std");
const testkit = @import("../core/testkit.zig");
const builtin = @import("builtin");
const Io = std.Io;
const Allocator = std.mem.Allocator;
const linux = std.os.linux;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const derive = @import("../core/derive.zig");
const log = @import("../core/log.zig");
const nvme_dev = @import("../io/nvme_dev.zig");
const layout = @import("layout.zig");
const staging = @import("staging.zig");
const block_alloc = @import("block_alloc.zig");
const seams = @import("write_seams.zig");
const write_admit = @import("write_admit.zig");
const accumulate = @import("accumulate.zig");
const sole_owner_patch = @import("sole_owner_patch.zig");
const device_overlay = @import("device_overlay.zig");
const extent = @import("extent_overlay.zig");
const writeback = @import("writeback.zig");
const fsync_mod = @import("fsync.zig");
const pipeline = @import("write_pipeline.zig");
const wt = @import("write_testing.zig");

comptime {
    assert(builtin.is_test and builtin.os.tag == .linux);
}

const testing = std.testing;
const BS: u64 = 4 * derive.MiB;
const PAGE: u32 = seams.PAGE;
const VOL: u64 = 0xE2E;

/// The product binding of the device seam over ONE `nvme_dev.Device`.
const NvmeSeam = struct {
    dev: *nvme_dev.Device,

    fn seam(self: *NvmeSeam) seams.DeviceSeam {
        return .{ .ctx = self, .vtable = &vtable };
    }

    const vtable: seams.DeviceSeam.VTable = .{ .write = &write, .write_fixed = null, .read = &read, .flush = &flush };

    fn mapErr(e: nvme_dev.DeviceError) seams.DmaError {
        return switch (e) {
            error.NoSpaceLeft => error.NoSpaceLeft,
            error.QueueFull, error.LaneQueueFull => error.QueueFull,
            error.WorkerShutdown, error.WorkerClosed, error.LaneStopped => error.WorkerShutdown,
            error.VolumeFailed => error.VolumeFailed,
            error.WriterGuardFenced, error.ReservationConflict, error.DismountRefused => error.WriterGuardFenced,
            error.OutOfMemory => error.OutOfMemory,
            else => error.InputOutput,
        };
    }

    fn write(ctx: *anyopaque, _: u64, offset: u64, data: []const u8) seams.DmaError!void {
        const self: *NvmeSeam = @ptrCast(@alignCast(ctx));
        self.dev.writeBlock(offset, data) catch |e| return mapErr(e);
    }

    fn read(ctx: *anyopaque, _: u64, offset: u64, out: []u8) seams.ReadError!void {
        const self: *NvmeSeam = @ptrCast(@alignCast(ctx));
        const buf = self.dev.readBlock(offset, out.len) catch |e| return switch (e) {
            error.ShortRead => error.ShortRead,
            error.QueueFull, error.LaneQueueFull => error.QueueFull,
            error.WorkerShutdown, error.WorkerClosed, error.LaneStopped => error.WorkerShutdown,
            error.VolumeFailed => error.VolumeFailed,
            error.OutOfMemory => error.OutOfMemory,
            else => error.InputOutput,
        };
        defer buf.release();
        @memcpy(out, buf.bytes()[0..out.len]);
    }

    fn flush(ctx: *anyopaque, _: u64) seams.DmaError!void {
        const self: *NvmeSeam = @ptrCast(@alignCast(ctx));
        self.dev.flush() catch |e| return mapErr(e);
    }
};

const E2E = struct {
    path_buf: [96]u8 = undefined,
    path: [:0]const u8 = "",
    file: std.Io.File = undefined,
    dev: *nvme_dev.Device = undefined,
    nseam: NvmeSeam = undefined,
    alloc: *block_alloc.BlockAllocator = undefined,
    vols: seams.Volumes = .{},
    map: wt.RamMap = undefined,
    frees: wt.RamFrees = undefined,
    stripes: layout.MetaStripes = .{},
    wp: *write_admit.WritePath = undefined,

    var seq: std.atomic.Value(u32) = .init(0);

    fn init(self: *E2E, gpa: Allocator, overlay_on: bool) !void {
        const dir: []const u8 = blk: {
            std.Io.Dir.accessAbsolute(testing.io, "/build/tmp", .{}) catch {
                std.Io.Dir.accessAbsolute(testing.io, "/build", .{}) catch break :blk "/tmp";
                break :blk "/build";
            };
            break :blk "/build/tmp";
        };
        self.path = try std.mem.printSentinel(&self.path_buf, "{s}/sqz-write-e2e-{d}-{d}.img", .{ dir, linux.getpid(), seq.fetchAdd(1, .monotonic) }, 0);
        self.file = try std.Io.Dir.createFileAbsolute(testing.io, self.path, .{ .read = true, .truncate = true });
        const capacity = 64 * derive.MiB;
        try self.file.setLength(testing.io, capacity);
        self.dev = try nvme_dev.Device.open(gpa, self.path);
        self.nseam = .{ .dev = self.dev };
        self.alloc = try block_alloc.BlockAllocator.init(gpa, .{ .vol_tag = VOL, .vol = 0, .block_size = BS, .capacity_bytes = capacity });
        self.vols = .{};
        self.vols.add(.{ .vol_tag = VOL, .ordinal = 0, .alloc = self.alloc, .passthrough = true, .write_cache_volatile = true });
        self.map = wt.RamMap.init(gpa);
        self.map.chunk = BS;
        self.frees = wt.RamFrees.init(gpa, &self.vols);
        self.stripes = .{};
        const cfg: write_admit.Config = .{
            .block_size = BS,
            .patch_max = sole_owner_patch.patchMaxDerived(BS),
            .overlay = .{ .block_size = BS, .patch_max = sole_owner_patch.patchMaxDerived(BS), .enabled = overlay_on },
            .rewrite_shadow = true,
            .parked_cap = 8,
            .depth_override = .sync_inline,
            .fsync = .{},
        };
        self.wp = try write_admit.WritePath.init(gpa, cfg, .{ .io = testing.io, .vols = &self.vols, .device = self.nseam.seam(), .map = self.map.seam(), .free = self.frees.seam(), .stripes = &self.stripes, .budget_bytes = 1 * derive.GiB });
    }

    fn deinit(self: *E2E) void {
        self.wp.deinit(testing.io);
        self.frees.deinit();
        self.map.deinit();
        self.alloc.deinit();
        self.dev.deinit();
        self.file.close(testing.io);
        std.Io.Dir.deleteFileAbsolute(testing.io, self.path) catch {};
    }

    fn striped(size: u64) seams.FileView {
        return .{ .kind = .striped, .size = size, .map_resident = true };
    }

    /// The page-cache view after O_DIRECT writes (the completion invalidated it).
    fn readAt(self: *E2E, buf: []u8, offset: u64) !void {
        const n = try self.file.readPositionalAll(testing.io, buf, offset);
        try testing.expectEqual(buf.len, n);
    }
};

fn fillPattern(buf: []u8, seed: u64) void {
    for (buf, 0..) |*b, i| b.* = @truncate(((seed + i) *% 2654435761) >> 24);
}

fn expectPattern(buf: []const u8, seed: u64) !void {
    for (buf, 0..) |b, i| {
        const want: u8 = @truncate(((seed + i) *% 2654435761) >> 24);
        if (b != want) return error.TestExpectedEqual;
    }
}

test "e2e(nvme_dev): a 4 MiB sequential write through accumulation is 1 copy + 1 DMA; 4 KiB random overwrites patch in place (W1: patch_writes == ops, zero reads, patch_edge_rmw_reads == 0); out-of-order pieces complete the union with one upload; a partial tail spills to staging and the flush unit DMAs straight off the ring; fsync phases sum exactly; a torn DMA never joins coverage" {
    const io = testing.io;
    const gpa = testing.allocator;
    var e: E2E = .{};
    e.init(gpa, false) catch |err| switch (err) {
        error.DirectIoUnsupported, error.DirectOpenRefused => return testkit.skip(@src(), .no_device, "the venue's filesystem refuses O_DIRECT"),
        else => return err,
    };
    defer e.deinit();
    const payload = try gpa.alignedAlloc(u8, .fromByteUnits(PAGE), BS);
    defer gpa.free(payload);
    const got = try gpa.alloc(u8, BS);
    defer gpa.free(got);
    const fence0 = counters.tripwireCount("overlay_fence_drops");
    const unpub0 = counters.tripwireCount("overlay_unpublished_at_fsync");

    // Row 1: 4 MiB sequential as the kernel's four 1 MiB pieces, in order.
    try e.map.create(1, .striped, 0);
    fillPattern(payload, 1);
    const copies0 = accumulate.stats.nt_copy_bytes.load();
    const wt0 = accumulate.stats.write_through_blocks.load();
    const puts0 = nvme_dev.stats.put_obj.load();
    var piece: u64 = 0;
    while (piece < 4) : (piece += 1) {
        const o = try e.wp.write(io, .{ .ino = 1, .offset = piece * derive.MiB, .payload = .{ .bytes = payload[piece * derive.MiB .. (piece + 1) * derive.MiB] }, .class = .entire_op, .file = E2E.striped(piece * derive.MiB) });
        try testing.expectEqual(derive.MiB, o.written);
    }
    try testing.expectEqual(copies0 + BS, accumulate.stats.nt_copy_bytes.load()); // ONE userspace copy
    try testing.expectEqual(wt0 + 1, accumulate.stats.write_through_blocks.load());
    try testing.expectEqual(puts0 + 1, nvme_dev.stats.put_obj.load()); // ONE DMA
    const b1 = e.map.seam().lookup(1, 0).?;
    try e.readAt(got, b1.offset);
    try expectPattern(got, 1);

    // Row 2: 4 KiB random overwrites on the published block ride W1.
    const patches0 = sole_owner_patch.stats.patch_writes.load();
    const gets0 = nvme_dev.stats.get_obj.load();
    var page: [PAGE]u8 align(4096) = undefined;
    const offsets = [_]u64{ 37, 3, 900, 512, 1023, 64, 37 };
    for (offsets, 0..) |pg, i| {
        fillPattern(&page, 1000 + i);
        const o = try e.wp.write(io, .{ .ino = 1, .offset = pg * PAGE, .payload = .{ .bytes = &page }, .class = .shared, .file = E2E.striped(BS) });
        try testing.expectEqual(@as(u32, 1), o.count(.patch));
    }
    try testing.expectEqual(patches0 + offsets.len, sole_owner_patch.stats.patch_writes.load());
    try testing.expectEqual(gets0, nvme_dev.stats.get_obj.load()); // zero device reads
    try testing.expectEqual(@as(u64, 0), counters.tripwireCount("patch_edge_rmw_reads"));
    try testing.expect(e.map.seam().lookup(1, 0).?.eql(b1)); // in place: the binding did not move
    try e.readAt(got, b1.offset);
    try expectPattern(got[37 * PAGE .. 38 * PAGE], 1000 + 6); // the last write to page 37 won
    try expectPattern(got[3 * PAGE .. 4 * PAGE], 1001);
    try expectPattern(got[38 * PAGE .. 39 * PAGE], 1 + 38 * PAGE); // a neighbour untouched

    // Row 3: out-of-order O_DIRECT-style pieces (end first, middle last) — one upload, no seed read.
    try e.map.create(2, .striped, 0);
    fillPattern(payload, 2);
    const reads_before = nvme_dev.stats.get_obj.load();
    const wt1 = accumulate.stats.write_through_blocks.load();
    _ = try e.wp.write(io, .{ .ino = 2, .offset = 3 * derive.MiB, .payload = .{ .bytes = payload[3 * derive.MiB ..] }, .class = .entire_op, .file = E2E.striped(0) });
    _ = try e.wp.write(io, .{ .ino = 2, .offset = 0, .payload = .{ .bytes = payload[0..derive.MiB] }, .class = .entire_op, .file = E2E.striped(BS) });
    const o3 = try e.wp.write(io, .{ .ino = 2, .offset = derive.MiB, .payload = .{ .bytes = payload[derive.MiB .. 3 * derive.MiB] }, .class = .entire_op, .file = E2E.striped(BS) });
    try testing.expectEqual(@as(u32, 1), o3.count(.write_through));
    try testing.expectEqual(wt1 + 1, accumulate.stats.write_through_blocks.load());
    try testing.expectEqual(reads_before, nvme_dev.stats.get_obj.load());
    try e.readAt(got, e.map.seam().lookup(2, 0).?.offset);
    try expectPattern(got, 2);

    // Row 4: a partial tail parks; the spill stages it (no DMA); the flush unit DMAs straight off the ring.
    try e.map.create(3, .striped, 0);
    const tail_len: usize = 1_000_000;
    fillPattern(payload[0..tail_len], 3);
    const o4 = try e.wp.write(io, .{ .ino = 3, .offset = 0, .payload = .{ .bytes = payload[0..tail_len] }, .class = .entire_op, .file = E2E.striped(0) });
    try testing.expectEqual(@as(u32, 1), o4.count(.extent) + o4.count(.accumulate_park)); // < a quarter block: W2 parks it compactly
    try testing.expect(e.wp.table.has(3, 0));
    const image = try gpa.alignedAlloc(u8, .fromByteUnits(PAGE), 16 * BS);
    defer gpa.free(image);
    @memset(image, 0);
    var seg = try staging.Segment.init(gpa, image, .none);
    defer seg.deinit();
    var pool = staging.Pool.init(gpa, 8 * BS, .none);
    defer pool.deinit();
    var worker = try writeback.Worker.init(gpa, 4);
    defer worker.deinit();
    const wb_deps: writeback.Deps = .{ .device = e.nseam.seam(), .map = e.map.seam(), .free = e.frees.seam(), .purge = .none, .vols = &e.vols, .picker = e.wp.picker.picker(), .locks = &e.wp.locks, .stripes = &e.stripes, .pool = &pool, .seg = &seg };
    const puts_spill0 = nvme_dev.stats.put_obj.load();
    try writeback.spillParkedBlock(&e.wp.table, wb_deps, io, &worker, 3, 0);
    try testing.expect(!e.wp.table.has(3, 0));
    try testing.expectEqual(puts_spill0, nvme_dev.stats.put_obj.load()); // the spill is no DMA
    try testing.expectEqual(@as(u32, 1), worker.drainOnce(io, wb_deps));
    try testing.expectEqual(puts_spill0 + 1, nvme_dev.stats.put_obj.load()); // ONE DMA off the ring
    const b3 = e.map.seam().lookup(3, 0).?;
    try e.readAt(got, b3.offset);
    try expectPattern(got[0..tail_len], 3);
    try testing.expect(std.mem.allEqual(u8, got[tail_len..], 0)); // the zero-completed complement
    try testing.expectEqual(@as(u64, 0), pool.currentBytes());

    // Row 5: fsync of a file with a parked partial block — the phases sum exactly, the barrier is one data flush.
    try e.map.create(4, .striped, 0);
    _ = try e.wp.write(io, .{ .ino = 4, .offset = 0, .payload = .{ .bytes = payload[0..tail_len] }, .class = .entire_op, .file = E2E.striped(0) });
    const syncs0 = nvme_dev.stats.data_device_syncs.load();
    var prof = fsync_mod.Prof.start();
    try fsync_mod.fsync(e.wp.fsyncCtx(), io, 4, &prof);
    prof.finish();
    var sum: u64 = 0;
    for (prof.legs) |l| sum += l;
    try testing.expectEqual(prof.total(), sum);
    try testing.expectEqual(syncs0 + 1, nvme_dev.stats.data_device_syncs.load());
    try testing.expect(!e.wp.table.has(4, 0));
    try testing.expect(e.map.seam().lookup(4, 0) != null);

    // Row 6: a torn DMA (injected EIO) on a W1 patch: EIO for that write, the block re-stabilized, bytes intact.
    e.dev.setFailNextWritesErrno(1, .IO);
    fillPattern(&page, 7777);
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.InputOutput, e.wp.write(io, .{ .ino = 1, .offset = 5 * PAGE, .payload = .{ .bytes = &page }, .class = .shared, .file = E2E.striped(BS) }));
    try log.testing.expectAllFired();
    try testing.expect(block_alloc.Incarnation.isStable(e.alloc.incarnationWord(e.alloc.idxOf(b1.offset))));
    try e.readAt(got, b1.offset);
    try expectPattern(got[5 * PAGE .. 6 * PAGE], 1 + 5 * PAGE);
    try testing.expectEqual(fence0, counters.tripwireCount("overlay_fence_drops"));
    try testing.expectEqual(unpub0, counters.tripwireCount("overlay_unpublished_at_fsync"));
}

/// Σ of the device-write byte arms (`brk.py` `DEVICE_WRITE_ARMS` minus the
/// ring's writeback leg and the plane's own-block DMA, which this harness
/// never engages; `durable_upload_bytes_escalation` is a subset of
/// `write_through_bytes` and is not summed twice).
fn deviceWriteBytes() u64 {
    return accumulate.stats.write_through_bytes.load() + extent.stats.fold_inplace_write_bytes.load() + accumulate.stats.drain_inplace_write_bytes.load() + sole_owner_patch.stats.patch_write_bytes.load() + device_overlay.stats.overlay_store_bytes.load();
}

test "e2e(nvme_dev): the W2 amplification law at the 4 MiB block (repro-port: fstests generic/074 `fstest -F -b 512`, 128× on c36b531): 512 B writes at a 1 KiB stride extending a fresh file land each block ONCE at release (device bytes == blocks × 4 MiB = 2× the user bytes, one DMA per block, zero reads) and read back byte-exact; sector writes into 4 scattered pages of a published block fold IN PLACE as 4 page DMAs (16 KiB, the 8× bound) with the binding unchanged" {
    const io = testing.io;
    const gpa = testing.allocator;
    var e: E2E = .{};
    e.init(gpa, false) catch |err| switch (err) {
        error.DirectIoUnsupported, error.DirectOpenRefused => return testkit.skip(@src(), .no_device, "the venue's filesystem refuses O_DIRECT"),
        else => return err,
    };
    defer e.deinit();
    const got = try gpa.alloc(u8, BS);
    defer gpa.free(got);
    // Row 1: the fstest shape over 4 blocks.
    try e.map.create(1, .striped, 0);
    const n_blocks: u64 = 4;
    const stride: u64 = 1024;
    var sector: [512]u8 = undefined;
    const puts0 = nvme_dev.stats.put_obj.load();
    const gets0 = nvme_dev.stats.get_obj.load();
    const dev0 = deviceWriteBytes();
    var off: u64 = 0;
    var size: u64 = 0;
    var user: u64 = 0;
    while (off < n_blocks * BS) : (off += stride) {
        fillPattern(&sector, off);
        const o = try e.wp.write(io, .{ .ino = 1, .offset = off, .payload = .{ .bytes = &sector }, .class = .entire_op, .file = E2E.striped(size) });
        try testing.expectEqual(@as(u64, 512), o.written);
        size = off + 512;
        user += 512;
    }
    e.wp.flushIno(io, 1);
    try testing.expectEqual(n_blocks * BS, deviceWriteBytes() - dev0);
    try testing.expectEqual(puts0 + n_blocks, nvme_dev.stats.put_obj.load());
    try testing.expectEqual(gets0, nvme_dev.stats.get_obj.load());
    try testing.expectEqual(extent.holeBoundPpm(BS, BS / 2), (deviceWriteBytes() - dev0) * 1_000_000 / user);
    var b: u32 = 0;
    while (b < n_blocks) : (b += 1) {
        const m = e.map.seam().lookup(1, b) orelse return error.TestUnexpectedResult;
        try e.readAt(got, m.offset);
        var rel: u64 = 0;
        while (rel < BS) : (rel += stride) {
            fillPattern(&sector, b * BS + rel);
            try testing.expectEqualSlices(u8, &sector, got[rel .. rel + 512]);
            try testing.expect(std.mem.allEqual(u8, got[rel + 512 .. rel + stride], 0));
        }
    }
    // Row 2: four sector writes into scattered pages of the published block 0.
    const m0 = e.map.seam().lookup(1, 0).?;
    const dev1 = deviceWriteBytes();
    const puts1 = nvme_dev.stats.put_obj.load();
    const seeds0 = extent.stats.extent_page_seed_bytes.load();
    const merges = e.map.merges.items.len;
    const used = e.alloc.usedBlocks();
    const pages = [_]u64{ 3, 40, 500, 1000 };
    for (pages, 0..) |pg, i| {
        fillPattern(&sector, 7000 + i);
        const o = try e.wp.write(io, .{ .ino = 1, .offset = pg * PAGE + 2048, .payload = .{ .bytes = &sector }, .class = .shared, .file = E2E.striped(n_blocks * BS) });
        try testing.expectEqual(@as(u32, 1), o.count(.extent));
    }
    // Pages ≥ block/4P would be 256 at 4 MiB: the trigger is the release here.
    try testing.expect(e.wp.table.has(1, 0));
    e.wp.flushIno(io, 1);
    try testing.expect(!e.wp.table.has(1, 0));
    try testing.expectEqual(@as(u64, 4 * PAGE), deviceWriteBytes() - dev1);
    try testing.expectEqual(puts1 + 4, nvme_dev.stats.put_obj.load());
    try testing.expectEqual(seeds0 + 4 * PAGE, extent.stats.extent_page_seed_bytes.load());
    try testing.expect((deviceWriteBytes() - dev1) * 1_000_000 / (4 * 512) <= extent.inPlaceBoundPpm(512, 512));
    try testing.expectEqual(merges, e.map.merges.items.len);
    try testing.expectEqual(used, e.alloc.usedBlocks());
    try testing.expect(e.map.seam().lookup(1, 0).?.eql(m0));
    try e.readAt(got, m0.offset);
    for (pages, 0..) |pg, i| {
        fillPattern(&sector, 7000 + i);
        try testing.expectEqualSlices(u8, &sector, got[pg * PAGE + 2048 .. pg * PAGE + 2560]);
        fillPattern(&sector, pg * PAGE); // the page's first sector: the row-1 pattern, seeded back in place
        try testing.expectEqualSlices(u8, &sector, got[pg * PAGE .. pg * PAGE + 512]);
    }
}

test "e2e(nvme_dev): an extending append's drain on a cache-less set (rung-10 dev perf §10.4: 1.6 GB of device writes for 36 KB of appends) — a 10,000 B file's tail block lands once whole (the hole law), then 200 stream-adjacent 72 B appends, each drained at its close, land as ONE DMA of their touched pages: device bytes per drain = pages × 4 KiB (≤ the §J in-place bound for 72 B) against the 4 MiB block per drain before, the binding unchanged, every byte exact on the device" {
    const io = testing.io;
    const gpa = testing.allocator;
    var e: E2E = .{};
    e.init(gpa, false) catch |err| switch (err) {
        error.DirectIoUnsupported, error.DirectOpenRefused => return testkit.skip(@src(), .no_device, "the venue's filesystem refuses O_DIRECT"),
        else => return err,
    };
    defer e.deinit();
    const got = try gpa.alloc(u8, BS);
    defer gpa.free(got);
    try e.map.create(3, .striped, 0);
    var head: [10_000]u8 = undefined;
    fillPattern(&head, 77);
    _ = try e.wp.write(io, .{ .ino = 3, .offset = 0, .payload = .{ .bytes = &head }, .class = .entire_op, .file = E2E.striped(0) });
    const hole0 = deviceWriteBytes();
    e.wp.flushIno(io, 3);
    try testing.expectEqual(BS, deviceWriteBytes() - hole0); // the hole block's one landing
    const m = e.map.seam().lookup(3, 0).?;
    var size: u64 = head.len;
    var line: [72]u8 = undefined;
    const appends = 200;
    var device: u64 = 0;
    var i: u64 = 0;
    while (i < appends) : (i += 1) {
        fillPattern(&line, 1000 + i);
        const o = try e.wp.write(io, .{ .ino = 3, .offset = size, .payload = .{ .bytes = &line }, .class = .entire_op, .file = E2E.striped(size) });
        try testing.expectEqual(@as(u32, 1), o.count(.accumulate_park));
        const dev0 = deviceWriteBytes();
        const puts0 = nvme_dev.stats.put_obj.load();
        e.wp.flushIno(io, 3); // the close's drain
        const pages = (std.mem.alignForward(u64, size + line.len, PAGE) - std.mem.alignBackward(u64, size, PAGE)) / PAGE;
        try testing.expectEqual(pages * PAGE, deviceWriteBytes() - dev0);
        try testing.expectEqual(puts0 + 1, nvme_dev.stats.put_obj.load());
        try testing.expect((deviceWriteBytes() - dev0) * 1_000_000 / line.len <= extent.inPlaceBoundPpm(line.len, 1));
        device += deviceWriteBytes() - dev0;
        size += line.len;
    }
    std.debug.print("\ne2e[append drain]: {d} appends of {d} B ({d} B) drained as {d} B of page DMAs (×{d:.1} per appended byte; the whole-block drain was ×{d})\n", .{ appends, line.len, appends * line.len, device, @as(f64, @floatFromInt(device)) / @as(f64, @floatFromInt(appends * line.len)), BS / line.len });
    try testing.expect(e.map.seam().lookup(3, 0).?.eql(m));
    try e.readAt(got, m.offset);
    try testing.expectEqualSlices(u8, &head, got[0..head.len]);
    i = 0;
    while (i < appends) : (i += 1) {
        fillPattern(&line, 1000 + i);
        try testing.expectEqualSlices(u8, &line, got[head.len + i * line.len ..][0..line.len]);
    }
    try testing.expect(std.mem.allEqual(u8, got[size..], 0)); // the hole landing's zeros past EOF
}

test "e2e(nvme_dev): the overlay arm — four 1 MiB page-aligned pieces of a fresh block ride the overlay with ZERO userspace copies and four DMAs; the drain publishes once; an aligned 1 MiB overwrite of the published block installs an overwrite record, seeds the gaps RANGED and feeds the epoch" {
    const io = testing.io;
    const gpa = testing.allocator;
    var e: E2E = .{};
    e.init(gpa, true) catch |err| switch (err) {
        error.DirectIoUnsupported, error.DirectOpenRefused => return testkit.skip(@src(), .no_device, "the venue's filesystem refuses O_DIRECT"),
        else => return err,
    };
    defer e.deinit();
    const payload = try gpa.alignedAlloc(u8, .fromByteUnits(PAGE), BS);
    defer gpa.free(payload);
    const got = try gpa.alloc(u8, BS);
    defer gpa.free(got);
    const fence0 = counters.tripwireCount("overlay_fence_drops");
    try e.map.create(1, .striped, 0);
    fillPattern(payload, 11);
    const copies0 = accumulate.stats.nt_copy_bytes.load();
    const puts0 = nvme_dev.stats.put_obj.load();
    const stores0 = device_overlay.stats.overlay_store_bytes.load();
    var piece: u64 = 0;
    while (piece < 4) : (piece += 1) {
        const o = try e.wp.write(io, .{ .ino = 1, .offset = piece * derive.MiB, .payload = .{ .bytes = payload[piece * derive.MiB .. (piece + 1) * derive.MiB] }, .class = .entire_op, .file = E2E.striped(piece * derive.MiB) });
        try testing.expectEqual(@as(u32, 1), o.count(.overlay));
    }
    e.wp.quiesce(io);
    try testing.expectEqual(copies0, accumulate.stats.nt_copy_bytes.load()); // zero daemon copies
    try testing.expectEqual(puts0 + 4, nvme_dev.stats.put_obj.load());
    try testing.expectEqual(stores0 + BS, device_overlay.stats.overlay_store_bytes.load());
    try testing.expect(!e.wp.overlay.anyOpenFast());
    const b1 = e.map.seam().lookup(1, 0).?;
    try e.readAt(got, b1.offset);
    try expectPattern(got, 11);
    // The overwrite: one aligned 1 MiB piece into the mapped block.
    fillPattern(payload[0..derive.MiB], 12);
    const ranged0 = device_overlay.stats.overlay_gap_seed_ranged_bytes.load();
    const feeds0 = device_overlay.stats.overlay_epoch_feeds.load();
    const o = try e.wp.write(io, .{ .ino = 1, .offset = derive.MiB, .payload = .{ .bytes = payload[0..derive.MiB] }, .class = .shared, .file = E2E.striped(BS) });
    try testing.expectEqual(@as(u32, 1), o.count(.overlay));
    try testing.expect(e.wp.overlay.hasCustody(1, 0));
    // Release-class drain: seeds the 3 MiB of gaps ranged, feeds the epoch; the close publishes.
    e.wp.flushIno(io, 1);
    try testing.expectEqual(ranged0 + 3 * derive.MiB, device_overlay.stats.overlay_gap_seed_ranged_bytes.load());
    try testing.expectEqual(feeds0 + 1, device_overlay.stats.overlay_epoch_feeds.load());
    const b2 = e.map.seam().lookup(1, 0).?;
    try testing.expect(!b2.eql(b1)); // a fresh destination, never in place
    try e.readAt(got, b2.offset);
    try expectPattern(got[0..derive.MiB], 11);
    try expectPattern(got[derive.MiB .. 2 * derive.MiB], 12);
    try expectPattern(got[2 * derive.MiB ..], 11 + 2 * derive.MiB);
    try testing.expectEqual(@as(usize, 1), e.frees.freed.items.len); // the displaced A key freed after the save
    try testing.expectEqual(fence0, counters.tripwireCount("overlay_fence_drops"));
}
