//! `squeezefs status sqmeta://<path>` (charter §8): the superblock, the
//! newest ledger record, the set config and a one-line summary, read
//! O_DIRECT through the io pool without the writer guard (a read-only
//! snapshot beside a live daemon is legal: both superblock copies and
//! every ledger slot are whole-sector, checksummed units; the control
//! tree is read at the newest CHECKPOINT — a verb's record committed since
//! and not yet checkpointed is in the journal, which a status never
//! replays), and the data volumes' REFERENCE CENSUS out of the native slot
//! tree (records and distinct blocks per volume — the pack census the
//! smoke reads, the C8 oracle's durable half). Also the preflight
//! classifier `format` shares. Linux only.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const cli = @import("cli.zig");
const args = @import("args.zig");
const uri = @import("uri.zig");
const superblock = @import("../meta/superblock.zig");
const ledger = @import("../meta/ledger.zig");
const node = @import("../meta/node.zig");
const node_cache = @import("../meta/node_cache.zig");
const node_seq = @import("../meta/node_seq.zig");
const tree = @import("../meta/tree.zig");
const key = @import("../meta/key.zig");
const device_loader = @import("../meta/device_loader.zig");
const set_config = @import("../data/set_config.zig");
const uring_fs = @import("../io/uring_fs.zig");
const align_probe = @import("../io/align_probe.zig");
const derive = @import("../core/derive.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("cli/status is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

/// What sector 0 (or, when it errors, the DUR-5 tail copy) says.
pub const Prior = union(enum) {
    blank,
    formatted,
    legacy: u32,
    /// The superblock refusal's text (in the caller's buffer).
    refused: []const u8,
};

pub const Snapshot = struct {
    prior: Prior,
    /// The v4 superblock when `prior == .formatted`.
    sb: ?superblock.Superblock,
    slot: superblock.SuperblockSlot,
    volume_len: u64,
};

pub const ClassifyError = error{ CannotStat, NotADeviceOrFile, Unexpected, DeviceRefused, Io, OutOfMemory };

/// Read both superblock copies through the pool and classify (`format`'s
/// preflight, `status`'s first line).
pub fn classify(gpa: std.mem.Allocator, env: *const std.process.Environ.Map, pool: *uring_fs.Pool, path: []const u8, reason_buf: []u8) ClassifyError!Snapshot {
    const volume_len = try device_loader.volumeLength(path);
    const mode = uring_fs.registerMetaDevice(gpa, env, path) catch return error.DeviceRefused;
    var dev: device_loader.DeviceLoader = .{ .gpa = gpa, .pool = pool, .path = path, .mode = mode, .ledger_base = 0, .bitmap_base = 0 };
    const sector0 = try dev.readRange(gpa, 0, superblock.sector_len);
    defer gpa.free(sector0);
    const backup: ?[]u8 = if (superblock.backupOffsetFor(volume_len)) |off| try dev.readRange(gpa, off, superblock.sector_len) else null;
    defer if (backup) |b| gpa.free(b);
    var diag: superblock.Refusal = undefined;
    const classified = superblock.classifyVolumeImages(sector0, backup, volume_len, &diag) catch {
        var w: Io.Writer = .fixed(reason_buf);
        diag.write(&w) catch {};
        return .{ .prior = .{ .refused = w.buffered() }, .sb = null, .slot = .primary, .volume_len = volume_len };
    };
    return switch (classified.format) {
        .blank => .{ .prior = .blank, .sb = null, .slot = classified.slot, .volume_len = volume_len },
        .legacy => |v| .{ .prior = .{ .legacy = v }, .sb = null, .slot = classified.slot, .volume_len = volume_len },
        .v4 => |sb| .{ .prior = .formatted, .sb = sb, .slot = classified.slot, .volume_len = volume_len },
    };
}

/// Is a writer's layer-A guard held on `path` right now? A shared,
/// non-blocking `flock` probe that is released at once: it conflicts with
/// the daemon's exclusive lock and nothing else.
pub fn writerGuardHeld(path: []const u8) bool {
    var zbuf: [linux.PATH_MAX]u8 = undefined;
    const z = align_probe.pathZ(&zbuf, path) catch return false;
    const rc = linux.open(z, .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) return false;
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    const op: i32 = std.posix.LOCK.SH | std.posix.LOCK.NB;
    return linux.errno(linux.flock(fd, op)) == .AGAIN;
}

/// The newest valid ledger record of a v4 volume (caller frees).
pub fn newestLedger(gpa: std.mem.Allocator, env: *const std.process.Environ.Map, pool: *uring_fs.Pool, path: []const u8, sb: superblock.Superblock) ClassifyError!?ledger.LedgerRecord {
    const mode = uring_fs.registerMetaDevice(gpa, env, path) catch return error.DeviceRefused;
    var dev: device_loader.DeviceLoader = .{ .gpa = gpa, .pool = pool, .path = path, .mode = mode, .ledger_base = sb.root_ledger.start, .bitmap_base = sb.alloc_bitmap.start };
    const img = try dev.readRange(gpa, sb.root_ledger.start, @intCast(sb.root_ledger.len));
    defer gpa.free(img);
    return ledger.newestValid(gpa, img);
}

fn writeUuid(w: *Io.Writer, u: [16]u8) Io.Writer.Error!void {
    try w.print("{x}-{x}-{x}-{x}-{x}", .{ u[0..4], u[4..6], u[6..8], u[8..10], u[10..16] });
}

/// A read-only status needs a handful of nodes: the control tree's root
/// and the leaf holding the one record. The budget is the floor the cache
/// shards derive from (a budget below one node is one shard).
const status_cache_budget_bytes: u64 = 16 << 20;

/// One data volume's durable reference population at the newest checkpoint.
pub const VolCensus = struct { vol_tag: u64, references: u64, blocks: u64 };

/// The reference records of every data volume `cfg` names, read offline
/// from the native slot tree: records and DISTINCT block indices (the
/// refs key sorts by block, so the distinct count is one pass).
pub fn readRefCensus(gpa: std.mem.Allocator, io: Io, env: *const std.process.Environ.Map, pool: *uring_fs.Pool, path: []const u8, sb: superblock.Superblock, rec: *const ledger.LedgerRecord, cfg: *const set_config.Config, out: *std.ArrayList(VolCensus)) ClassifyError!void {
    const mode = uring_fs.registerMetaDevice(gpa, env, path) catch return error.DeviceRefused;
    var dev: device_loader.DeviceLoader = .{ .gpa = gpa, .pool = pool, .path = path, .mode = mode, .ledger_base = sb.root_ledger.start, .bitmap_base = sb.alloc_bitmap.start };
    const layout = node.NodeLayout.new(sb.node_size, null) catch return error.Unexpected;
    const cache = try node_cache.NodeCache.init(gpa, io, .{ .layout = layout, .heap_base = sb.heap.start, .budget_bytes = status_cache_budget_bytes }, dev.loader());
    defer cache.deinit();
    cache.setDurableTail(rec.journal_tail_seq);
    var seq = node_seq.NodeSeqHandle.shared(rec.node_seq_watermark);
    const t = tree.KvTree.openFromLedger(gpa, cache, rec, .native_slot, &seq) catch |e| return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        error.Io => error.Io,
        else => error.Unexpected,
    };
    defer t.destroy();
    for (cfg.data) |d| {
        var census: VolCensus = .{ .vol_tag = d.vol_tag, .references = 0, .blocks = 0 };
        const range = key.volumeRange(d.vol_tag);
        var cursor = range.lo;
        var last_block: ?u64 = null;
        while (true) {
            var kvs: std.ArrayList(tree.KeyValue) = .empty;
            defer tree.deinitKeyValues(gpa, &kvs);
            t.range(gpa, cursor.slice(), range.hi.slice(), 512, &kvs) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.Io => error.Io,
                else => error.Unexpected,
            };
            var n: usize = 0;
            for (kvs.items) |kv| {
                if (!key.lessThan(kv.key, range.hi.slice())) break;
                const k = key.BlockRefKey.decode(kv.key) catch return error.Unexpected;
                census.references += 1;
                if (last_block == null or last_block.? != k.blockIdx()) census.blocks += 1;
                last_block = k.blockIdx();
                n += 1;
            }
            if (n < 512 or kvs.items.len == 0) break;
            cursor = key.sameKindSuccessor(kvs.items[kvs.items.len - 1].key) orelse break;
        }
        try out.append(gpa, census);
    }
}

/// What the control tree holds under `set_config` at the newest checkpoint.
pub const SetConfigRead = union(enum) {
    /// No record (a volume formatted before the record existed, or not
    /// member 0 of its set).
    absent,
    config: set_config.Config,
    /// The record is present but refused by the codec (the text).
    refused: []const u8,
};

/// Open the control tree offline over the node cache and look the record
/// up. `reason_buf` carries a refusal's text.
pub fn readSetConfig(gpa: std.mem.Allocator, io: Io, env: *const std.process.Environ.Map, pool: *uring_fs.Pool, path: []const u8, sb: superblock.Superblock, rec: *const ledger.LedgerRecord, reason_buf: []u8) ClassifyError!SetConfigRead {
    const mode = uring_fs.registerMetaDevice(gpa, env, path) catch return error.DeviceRefused;
    var dev: device_loader.DeviceLoader = .{ .gpa = gpa, .pool = pool, .path = path, .mode = mode, .ledger_base = sb.root_ledger.start, .bitmap_base = sb.alloc_bitmap.start };
    const layout = node.NodeLayout.new(sb.node_size, null) catch return error.Unexpected;
    const cache = try node_cache.NodeCache.init(gpa, io, .{ .layout = layout, .heap_base = sb.heap.start, .budget_bytes = status_cache_budget_bytes }, dev.loader());
    defer cache.deinit();
    cache.setDurableTail(rec.journal_tail_seq);
    var seq = node_seq.NodeSeqHandle.shared(rec.node_seq_watermark);
    const t = tree.KvTree.openFromLedger(gpa, cache, rec, .control, &seq) catch |e| return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        error.Io => error.Io,
        else => error.Unexpected,
    };
    defer t.destroy();
    const value = (t.lookup(gpa, set_config.CONTROL_KEY) catch |e| return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        error.Io => error.Io,
        else => error.Unexpected,
    }) orelse return .absent;
    defer gpa.free(value);
    var diag: set_config.Refusal = undefined;
    const cfg = set_config.decode(gpa, value, &diag) catch |e| switch (e) {
        error.OutOfMemory => return error.OutOfMemory,
        else => {
            var w: Io.Writer = .fixed(reason_buf);
            diag.write(&w) catch {};
            return .{ .refused = w.buffered() };
        },
    };
    return .{ .config = cfg };
}

/// The report (public so the smoke can read it from a buffer).
pub fn writeReport(w: *Io.Writer, meta_uri: []const u8, snap: Snapshot, rec: ?ledger.LedgerRecord, cfg: ?SetConfigRead, census: []const VolCensus, mounted: bool) Io.Writer.Error!void {
    switch (snap.prior) {
        .blank => return w.print("{s}: not formatted (zeroed superblock) — run `squeezefs format` first\n", .{meta_uri}),
        .legacy => |v| return w.print("{s}: a pre-v4 squeezefs format (version {d}), which this binary does not mount — reformat required\n", .{ meta_uri, v }),
        .refused => |reason| return w.print("{s}: superblock refused: {s}\n", .{ meta_uri, reason }),
        .formatted => {},
    }
    const sb = snap.sb.?;
    try w.print("{s}: SqueezeFS metadata volume, format v4 (one-member forest){s}\n", .{ meta_uri, if (snap.slot == .backup) " — read from the DUR-5 tail copy: sector 0 is unreadable" else "" });
    try w.print("  volume     {d} B; node {d} B; journal {d} B at {d}; ledger {d} B at {d}; bitmap {d} B at {d}; heap {d} extents ({d} B) at {d}; features 0x{x} ro 0x{x}\n", .{ sb.volume_len, sb.node_size, sb.journal.len, sb.journal.start, sb.root_ledger.len, sb.root_ledger.start, sb.alloc_bitmap.len, sb.alloc_bitmap.start, sb.totalExtents(), sb.heap.len, sb.heap.start, sb.features_incompat, sb.features_ro });
    try w.writeAll("  identity   uuid ");
    try writeUuid(w, sb.uuid);
    try w.writeAll(", set ");
    try writeUuid(w, sb.set_uuid);
    try w.print(", hash_seed 0x{x}\n", .{sb.hash_seed});
    if (rec) |r| {
        try w.print("  ledger     seq {d} (slot {d}), journal tail {d}, next_ino {d}, bitmap generation {d}, node_seq watermark {d}, roots:", .{ r.seq, r.slotIndex(), r.journal_tail_seq, r.next_ino, r.alloc_bitmap_generation, r.node_seq_watermark });
        for (r.tree_roots) |t| try w.print(" {t}@{d}/{d}", .{ t.id, t.node_addr, t.node_seq });
        try w.print("; member {d} of {d}, set epoch {d}\n", .{ r.membership_stamp.member_position, r.membership_stamp.member_count, r.membership_stamp.set_epoch });
        if (cfg) |c| switch (c) {
            .absent => try w.writeAll("  set config NONE in the control tree — this volume is not member 0 of its set, or predates the set_config record (reformat)\n"),
            .refused => |reason| try w.print("  set config REFUSED: {s}\n", .{reason}),
            .config => |*conf| try conf.writeSummary(w),
        };
        for (census) |vc| try w.print("  refs       vol-{x:0>16}: {d} reference(s) over {d} block(s)\n", .{ vc.vol_tag, vc.references, vc.blocks });
        try w.print("summary: v4, {d} MiB, checkpoint seq {d}, {d} inode(s) minted, {s}\n", .{ sb.volume_len >> 20, r.seq, r.next_ino -| 2, if (mounted) "MOUNTED by a live daemon (writer guard held)" else "not mounted" });
    } else {
        try w.writeAll("  ledger     NO valid record in any of the 32 slots — corruption (a v4 superblock always has one)\n");
        try w.print("summary: v4, {d} MiB, ledger missing, {s}\n", .{ sb.volume_len >> 20, if (mounted) "MOUNTED by a live daemon" else "not mounted" });
    }
}

pub fn run(p: cli.Process, s: args.Status) u8 {
    var members: [uri.MAX_MEMBERS][]const u8 = undefined;
    var udiag: uri.Refusal = undefined;
    const meta = uri.parse(.meta, s.meta_uri, &members, &udiag) catch {
        p.stderr.print("squeezefs status: {f}\n", .{udiag}) catch {};
        return cli.exit_usage;
    };
    if (meta.paths.len != 1) {
        p.stderr.print("squeezefs status: {s} names {d} metadata volumes; this rung reports ONE\n", .{ s.meta_uri, meta.paths.len }) catch {};
        return cli.exit_failed;
    }
    const path = meta.first();
    const pool = uring_fs.Pool.init(p.gpa, uring_fs.Config.resolve(p.env, derive.facts())) catch |e| {
        p.stderr.print("squeezefs status: io pool init failed ({s})\n", .{@errorName(e)}) catch {};
        return cli.exit_failed;
    };
    defer pool.deinit();
    var reason_buf: [512]u8 = undefined;
    const snap = classify(p.gpa, p.env, pool, path, &reason_buf) catch |e| {
        p.stderr.print("squeezefs status: cannot read {s}: {s}\n", .{ path, @errorName(e) }) catch {};
        return cli.exit_failed;
    };
    var rec: ?ledger.LedgerRecord = null;
    defer if (rec) |*r| r.deinit(p.gpa);
    var cfg: ?SetConfigRead = null;
    defer if (cfg) |*c| switch (c.*) {
        .config => |*conf| conf.deinit(p.gpa),
        else => {},
    };
    var cfg_reason: [512]u8 = undefined;
    var census: std.ArrayList(VolCensus) = .empty;
    defer census.deinit(p.gpa);
    if (snap.sb) |sb| {
        rec = newestLedger(p.gpa, p.env, pool, path, sb) catch |e| {
            p.stderr.print("squeezefs status: cannot read the ledger of {s}: {s}\n", .{ path, @errorName(e) }) catch {};
            return cli.exit_failed;
        };
        if (rec) |*r| {
            cfg = readSetConfig(p.gpa, p.io, p.env, pool, path, sb, r, &cfg_reason) catch |e| {
                p.stderr.print("squeezefs status: cannot read the control tree of {s}: {s}\n", .{ path, @errorName(e) }) catch {};
                return cli.exit_failed;
            };
            if (cfg.? == .config and cfg.?.config.data.len > 0) {
                readRefCensus(p.gpa, p.io, p.env, pool, path, sb, r, &cfg.?.config, &census) catch |e| {
                    p.stderr.print("squeezefs status: cannot read the reference census of {s}: {s}\n", .{ path, @errorName(e) }) catch {};
                    return cli.exit_failed;
                };
            }
        }
    }
    writeReport(p.stdout, s.meta_uri, snap, rec, cfg, census.items, writerGuardHeld(path)) catch return cli.exit_failed;
    p.stdout.flush() catch {};
    return if (snap.prior == .formatted) cli.exit_ok else cli.exit_failed;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the report: a blank volume is one line; a v4 volume prints the superblock, the ledger, the set config (present, absent or refused) and a summary naming the mount state" {
    var out: Io.Writer.Allocating = .init(testing.allocator);
    defer out.deinit();
    try writeReport(&out.writer, "sqmeta:///x", .{ .prior = .blank, .sb = null, .slot = .primary, .volume_len = 1 << 20 }, null, null, &.{}, false);
    try testing.expect(std.mem.indexOf(u8, out.written(), "not formatted") != null);
    out.clearRetainingCapacity();
    const sb = superblock.testFixture();
    var roots = [_]ledger.TreeRoot{ .{ .id = .control, .node_addr = sb.heap.start, .node_seq = 7 }, .{ .id = .native_slot, .node_addr = sb.heap.start + sb.node_size, .node_seq = 8 } };
    const rec = ledger.bootstrapRecord(&roots, 42, 9, ledger.MembershipStamp.oneMember(sb.set_uuid));
    const vols = [_]set_config.DataVolume{.{ .vol_tag = 0xfeed, .state = .active, .device_uuid = @splat(1), .added_ts = 0, .capacity_blocks = 10, .volume_len = 1 << 30, .uri = "/dev/d0" }};
    var conf = try set_config.birth(testing.allocator, sb.set_uuid, .{ .compression = .lz4, .block_size = 1 << 20 }, null, &vols, &.{});
    defer conf.deinit(testing.allocator);
    try writeReport(&out.writer, "sqmeta:///x", .{ .prior = .formatted, .sb = sb, .slot = .primary, .volume_len = sb.volume_len }, rec, .{ .config = conf }, &.{.{ .vol_tag = 0xfeed, .references = 3, .blocks = 2 }}, true);
    try testing.expect(std.mem.indexOf(u8, out.written(), "vol-000000000000feed active /dev/d0") != null);
    try testing.expect(std.mem.indexOf(u8, out.written(), "compression lz4") != null);
    try testing.expect(std.mem.indexOf(u8, out.written(), "refs       vol-000000000000feed: 3 reference(s) over 2 block(s)") != null);
    out.clearRetainingCapacity();
    try writeReport(&out.writer, "sqmeta:///x", .{ .prior = .formatted, .sb = sb, .slot = .primary, .volume_len = sb.volume_len }, rec, .absent, &.{}, false);
    try testing.expect(std.mem.indexOf(u8, out.written(), "set config NONE") != null);
    out.clearRetainingCapacity();
    try writeReport(&out.writer, "sqmeta:///x", .{ .prior = .formatted, .sb = sb, .slot = .primary, .volume_len = sb.volume_len }, rec, .{ .refused = "bad magic" }, &.{}, false);
    try testing.expect(std.mem.indexOf(u8, out.written(), "set config REFUSED: bad magic") != null);
    out.clearRetainingCapacity();
    try writeReport(&out.writer, "sqmeta:///x", .{ .prior = .formatted, .sb = sb, .slot = .primary, .volume_len = sb.volume_len }, rec, .{ .config = conf }, &.{.{ .vol_tag = 0xfeed, .references = 3, .blocks = 2 }}, true);
    const text = out.written();
    try testing.expect(std.mem.indexOf(u8, text, "format v4 (one-member forest)") != null);
    try testing.expect(std.mem.indexOf(u8, text, "next_ino 42") != null);
    try testing.expect(std.mem.indexOf(u8, text, "control@") != null);
    try testing.expect(std.mem.indexOf(u8, text, "40 inode(s) minted") != null);
    try testing.expect(std.mem.indexOf(u8, text, "MOUNTED by a live daemon") != null);
    try testing.expect(std.mem.startsWith(u8, text[std.mem.lastIndexOf(u8, text, "summary:").?..], "summary: v4,"));
}

test "the writer-guard probe reads false on a path nobody locks and on a missing path" {
    try testing.expect(!writerGuardHeld("/build/tmp/does-not-exist-status"));
    try testing.expect(!writerGuardHeld("/proc/self/status"));
}
