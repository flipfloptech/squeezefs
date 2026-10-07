//! `squeezefs df [--json] <sqmeta uri|mountpoint>` (charter §8; `docs/
//! porting/stats_cli.md` §4.6, §7 Q10): the set's space, OFFLINE and O(1) in
//! files — never an inode-tree walk. Read-only beside a live daemon (every
//! unit read is whole-sector and checksummed; the trees at the newest
//! CHECKPOINT, which the report names).
//!
//!   capacity = Σ (capacity_blocks − 1) × block over the non-retired data
//!              volumes (block 0 is the reserved signature region; the set's
//!              capacity IS its devices' — no quota, charter §8 thin
//!              provisioning is a by-design exception);
//!   used     = the DURABLE reference census: distinct referenced blocks per
//!              volume × block (`status.readRefCensus`, one kind-pure range
//!              per volume);
//!   avail    = capacity − used;
//!   inodes   = `INO_SPACE` total; `used` the minted watermark (+ the root)
//!              as the checkpoint recorded it — the live population needs
//!              the mount's destroy tally (`statfs` reports it live).
//!
//! The META volume table: size, the KV heap, its durable bitmap population
//! (`heap_used + heap_free == heap`, pinned) and `next_ino`. Linux only.

const std = @import("std");
const builtin = @import("builtin");
const Io = std.Io;
const cli = @import("cli.zig");
const args = @import("args.zig");
const uri = @import("uri.zig");
const target = @import("target.zig");
const status = @import("status.zig");
const superblock = @import("../meta/superblock.zig");
const ledger = @import("../meta/ledger.zig");
const alloc_ext = @import("../meta/alloc_ext.zig");
const device_loader = @import("../meta/device_loader.zig");
const inos = @import("../meta/inos.zig");
const set_config = @import("../data/set_config.zig");
const uring_fs = @import("../io/uring_fs.zig");
const derive = @import("../core/derive.zig");
const kv_fs = @import("../fuse/handler/kv_fs.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("cli/df is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const MetaRow = struct {
    path: []const u8,
    size_bytes: u64,
    kv_heap_bytes: u64,
    kv_heap_used_bytes: u64,
    next_ino: u64,
    checkpoint_seq: u64,

    pub fn heapFree(self: MetaRow) u64 {
        return self.kv_heap_bytes - self.kv_heap_used_bytes;
    }
};

pub const DataRow = struct {
    vol_tag: u64,
    uri: []const u8,
    size_bytes: u64,
    capacity_bytes: u64,
    allocated_bytes: u64,
};

pub const Report = struct {
    block_size: u64,
    capacity_bytes: u64,
    used_bytes: u64,
    inodes_total: u64,
    inodes_used: u64,
    meta: []const MetaRow,
    data: []const DataRow,

    pub fn freeBytes(self: Report) u64 {
        return self.capacity_bytes -| self.used_bytes;
    }

    /// `{block_size, capacity_bytes, used_bytes, free_bytes, inodes{total,
    /// used, free}, data_volumes[{path, size_bytes, allocated_bytes}],
    /// meta_volumes[{path, size_bytes, kv_heap_bytes, kv_heap_free_bytes,
    /// kv_heap_used_bytes, next_ino}]}` — the reference's key set, pinned.
    pub fn writeJson(self: Report, w: *Io.Writer) Io.Writer.Error!void {
        try w.print("{{\"block_size\":{d},\"capacity_bytes\":{d},\"used_bytes\":{d},\"free_bytes\":{d},\"inodes\":{{\"total\":{d},\"used\":{d},\"free\":{d}}},\"data_volumes\":[", .{ self.block_size, self.capacity_bytes, self.used_bytes, self.freeBytes(), self.inodes_total, self.inodes_used, self.inodes_total -| self.inodes_used });
        for (self.data, 0..) |d, i| {
            if (i != 0) try w.writeByte(',');
            try w.print("{{\"vol_tag\":\"vol-{x:0>16}\",\"path\":\"{s}\",\"size_bytes\":{d},\"capacity_bytes\":{d},\"allocated_bytes\":{d}}}", .{ d.vol_tag, d.uri, d.size_bytes, d.capacity_bytes, d.allocated_bytes });
        }
        try w.writeAll("],\"meta_volumes\":[");
        for (self.meta, 0..) |m, i| {
            if (i != 0) try w.writeByte(',');
            try w.print("{{\"path\":\"{s}\",\"size_bytes\":{d},\"kv_heap_bytes\":{d},\"kv_heap_free_bytes\":{d},\"kv_heap_used_bytes\":{d},\"next_ino\":{d},\"checkpoint_seq\":{d}}}", .{ m.path, m.size_bytes, m.kv_heap_bytes, m.heapFree(), m.kv_heap_used_bytes, m.next_ino, m.checkpoint_seq });
        }
        try w.writeAll("]}\n");
    }

    pub fn writeText(self: Report, w: *Io.Writer) Io.Writer.Error!void {
        const pct: u64 = if (self.capacity_bytes == 0) 0 else self.used_bytes * 100 / self.capacity_bytes;
        try w.print("Data:   capacity {d} MiB  used {d} MiB ({d}%)  free {d} MiB  (block {d} B)\n", .{ self.capacity_bytes >> 20, self.used_bytes >> 20, pct, self.freeBytes() >> 20, self.block_size });
        try w.print("Inodes: space {d}  minted {d}  free {d}\n", .{ self.inodes_total, self.inodes_used, self.inodes_total -| self.inodes_used });
        try w.writeAll("DATA VOLUME         SIZE MiB  CAPACITY MiB  ALLOCATED MiB  URI\n");
        for (self.data) |d| try w.print("vol-{x:0>16} {d:>8} {d:>13} {d:>14}  {s}\n", .{ d.vol_tag, d.size_bytes >> 20, d.capacity_bytes >> 20, d.allocated_bytes >> 20, d.uri });
        try w.writeAll("META VOLUME  SIZE MiB  HEAP MiB  HEAP USED MiB  HEAP FREE MiB  NEXT_INO  CHECKPOINT\n");
        for (self.meta) |m| try w.print("{s}  {d}  {d}  {d}  {d}  {d}  seq {d}\n", .{ m.path, m.size_bytes >> 20, m.kv_heap_bytes >> 20, m.kv_heap_used_bytes >> 20, m.heapFree() >> 20, m.next_ino, m.checkpoint_seq });
    }
};

/// The durable heap-bitmap population of a v4 volume: the newest valid
/// slot of every page (A/B by generation), bits over the extent count.
pub fn heapUsedBytes(gpa: std.mem.Allocator, env: *const std.process.Environ.Map, pool: *uring_fs.Pool, path: []const u8, sb: superblock.Superblock) status.ClassifyError!u64 {
    const mode = uring_fs.registerMetaDevice(gpa, env, path) catch return error.DeviceRefused;
    var dev: device_loader.DeviceLoader = .{ .gpa = gpa, .pool = pool, .path = path, .mode = mode, .ledger_base = sb.root_ledger.start, .bitmap_base = sb.alloc_bitmap.start };
    const total = sb.totalExtents();
    const pages = alloc_ext.bitmapPagesFor(total);
    const region = try dev.readRange(gpa, sb.alloc_bitmap.start, @intCast(alloc_ext.bitmapRegionLen(total)));
    defer gpa.free(region);
    var set_bits: u64 = 0;
    var page: u64 = 0;
    while (page < pages) : (page += 1) {
        var best: ?alloc_ext.DecodedPage = null;
        for ([_]u8{ 0, 1 }) |slot| {
            const off: usize = @intCast(alloc_ext.pageSlotOffset(page, slot));
            const decoded = alloc_ext.decodeBitmapPage(region[off..], @intCast(page)) catch continue;
            if (best == null or decoded.generation > best.?.generation) best = decoded;
        }
        const b = best orelse continue;
        // Only the bits inside the extent count (the last page's tail is padding).
        const first_bit = page * alloc_ext.page_bits;
        const bits_here: usize = @intCast(@min(alloc_ext.page_bits, total -| first_bit));
        var i: usize = 0;
        while (i < bits_here) : (i += 1) {
            if (b.bits[i / 8] & (@as(u8, 1) << @intCast(i % 8)) != 0) set_bits += 1;
        }
    }
    return set_bits * sb.node_size;
}

pub fn run(p: cli.Process, d: args.Df) u8 {
    var resolved = target.resolve(p.gpa, p.io, d.target) catch |e| {
        target.writeRefusal(p.stderr, "df", d.target, e) catch {};
        return cli.exit_failed;
    };
    defer resolved.deinit(p.gpa);
    var members: [uri.MAX_MEMBERS][]const u8 = undefined;
    var udiag: uri.Refusal = undefined;
    const meta = uri.parse(.meta, resolved.meta_uri, &members, &udiag) catch {
        p.stderr.print("squeezefs df: {f}\n", .{udiag}) catch {};
        return cli.exit_usage;
    };
    const pool = uring_fs.Pool.init(p.gpa, uring_fs.Config.resolve(p.env, derive.facts())) catch |e| {
        p.stderr.print("squeezefs df: io pool init failed ({s})\n", .{@errorName(e)}) catch {};
        return cli.exit_failed;
    };
    defer pool.deinit();
    var meta_rows: std.ArrayList(MetaRow) = .empty;
    defer meta_rows.deinit(p.gpa);
    var data_rows: std.ArrayList(DataRow) = .empty;
    defer data_rows.deinit(p.gpa);
    var cfg: ?set_config.Config = null;
    defer if (cfg) |*c| c.deinit(p.gpa);
    var inodes_used: u64 = inos.ROOT_INO;
    var block_size: u64 = 0;
    for (meta.paths, 0..) |path, i| {
        var reason_buf: [512]u8 = undefined;
        const snap = status.classify(p.gpa, p.env, pool, path, &reason_buf) catch |e| {
            p.stderr.print("squeezefs df: cannot read {s}: {s}\n", .{ path, @errorName(e) }) catch {};
            return cli.exit_failed;
        };
        const sb = snap.sb orelse {
            p.stderr.print("squeezefs df: {s} is not a formatted v4 metadata volume — volume not formatted\n", .{path}) catch {};
            return cli.exit_failed;
        };
        var rec = (status.newestLedger(p.gpa, p.env, pool, path, sb) catch |e| {
            p.stderr.print("squeezefs df: cannot read the ledger of {s}: {s}\n", .{ path, @errorName(e) }) catch {};
            return cli.exit_failed;
        }) orelse {
            p.stderr.print("squeezefs df: {s} has no valid ledger record — corruption\n", .{path}) catch {};
            return cli.exit_failed;
        };
        defer rec.deinit(p.gpa);
        const heap_used = heapUsedBytes(p.gpa, p.env, pool, path, sb) catch |e| {
            p.stderr.print("squeezefs df: cannot read the heap bitmap of {s}: {s}\n", .{ path, @errorName(e) }) catch {};
            return cli.exit_failed;
        };
        meta_rows.append(p.gpa, .{ .path = path, .size_bytes = sb.volume_len, .kv_heap_bytes = sb.heap.len, .kv_heap_used_bytes = heap_used, .next_ino = rec.next_ino, .checkpoint_seq = rec.seq }) catch return cli.exit_failed;
        inodes_used +|= rec.next_ino -| inos.LOCAL_INO_BASE;
        if (i == 0) {
            var cfg_reason: [512]u8 = undefined;
            const read = status.readSetConfig(p.gpa, p.io, p.env, pool, path, sb, &rec, &cfg_reason) catch |e| {
                p.stderr.print("squeezefs df: cannot read the control tree of {s}: {s}\n", .{ path, @errorName(e) }) catch {};
                return cli.exit_failed;
            };
            switch (read) {
                .absent => {
                    p.stderr.print("squeezefs df: {s} carries no set_config record — volume not formatted as member 0 of a set\n", .{path}) catch {};
                    return cli.exit_failed;
                },
                .refused => |why| {
                    p.stderr.print("squeezefs df: {s}: set config refused: {s}\n", .{ path, why }) catch {};
                    return cli.exit_failed;
                },
                .config => |c| cfg = c,
            }
            block_size = cfg.?.block_size;
            var census: std.ArrayList(status.VolCensus) = .empty;
            defer census.deinit(p.gpa);
            if (cfg.?.data.len != 0) status.readRefCensus(p.gpa, p.io, p.env, pool, path, sb, &rec, &cfg.?, &census) catch |e| {
                p.stderr.print("squeezefs df: cannot read the reference census of {s}: {s}\n", .{ path, @errorName(e) }) catch {};
                return cli.exit_failed;
            };
            for (cfg.?.data) |dv| {
                if (dv.state == .retired) continue;
                var allocated: u64 = 0;
                for (census.items) |vc| if (vc.vol_tag == dv.vol_tag) {
                    allocated = vc.blocks * block_size;
                };
                data_rows.append(p.gpa, .{ .vol_tag = dv.vol_tag, .uri = dv.uri, .size_bytes = dv.volume_len, .capacity_bytes = (dv.capacity_blocks -| 1) * block_size, .allocated_bytes = allocated }) catch return cli.exit_failed;
            }
        }
    }
    var capacity: u64 = 0;
    var used: u64 = 0;
    for (data_rows.items) |r| {
        capacity += r.capacity_bytes;
        used += r.allocated_bytes;
    }
    const report: Report = .{ .block_size = block_size, .capacity_bytes = capacity, .used_bytes = used, .inodes_total = kv_fs.INO_SPACE, .inodes_used = inodes_used, .meta = meta_rows.items, .data = data_rows.items };
    (if (d.json) report.writeJson(p.stdout) else report.writeText(p.stdout)) catch return cli.exit_failed;
    p.stdout.flush() catch {};
    return cli.exit_ok;
}

const testing = std.testing;

test "the df report: the JSON key set is pinned, heap_used + heap_free == heap, free = capacity − used, inodes free = space − used; the text names capacity/used/free" {
    const meta_rows = [_]MetaRow{.{ .path = "/dev/m", .size_bytes = 1 << 30, .kv_heap_bytes = 900 << 20, .kv_heap_used_bytes = 100 << 20, .next_ino = 42, .checkpoint_seq = 9 }};
    const data_rows = [_]DataRow{.{ .vol_tag = 0xfeed, .uri = "/dev/d0", .size_bytes = 2 << 30, .capacity_bytes = 508 << 20, .allocated_bytes = 12 << 20 }};
    const r: Report = .{ .block_size = 4 << 20, .capacity_bytes = 508 << 20, .used_bytes = 12 << 20, .inodes_total = kv_fs.INO_SPACE, .inodes_used = 41, .meta = &meta_rows, .data = &data_rows };
    var out: Io.Writer.Allocating = .init(testing.allocator);
    defer out.deinit();
    try r.writeJson(&out.writer);
    const text = out.written();
    for ([_][]const u8{ "\"block_size\":4194304", "\"capacity_bytes\":532676608", "\"used_bytes\":12582912", "\"free_bytes\":520093696", "\"inodes\":{\"total\":72057594037927936,\"used\":41,\"free\":72057594037927895}", "\"data_volumes\":[{\"vol_tag\":\"vol-000000000000feed\",\"path\":\"/dev/d0\",\"size_bytes\":2147483648,\"capacity_bytes\":532676608,\"allocated_bytes\":12582912}]", "\"meta_volumes\":[{\"path\":\"/dev/m\",\"size_bytes\":1073741824,\"kv_heap_bytes\":943718400,\"kv_heap_free_bytes\":838860800,\"kv_heap_used_bytes\":104857600,\"next_ino\":42,\"checkpoint_seq\":9}]" }) |needle| {
        if (std.mem.indexOf(u8, text, needle) == null) {
            std.debug.print("\n--- missing {s} in {s} ---\n", .{ needle, text });
            return error.TestExpectedNeedle;
        }
    }
    try testing.expectEqual(meta_rows[0].kv_heap_bytes, meta_rows[0].heapFree() + meta_rows[0].kv_heap_used_bytes);
    out.clearRetainingCapacity();
    try r.writeText(&out.writer);
    try testing.expect(std.mem.indexOf(u8, out.written(), "Data:   capacity 508 MiB  used 12 MiB (2%)  free 496 MiB") != null);
    try testing.expect(std.mem.indexOf(u8, out.written(), "vol-000000000000feed") != null);
}
