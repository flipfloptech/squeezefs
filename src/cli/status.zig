//! `squeezefs status sqmeta://<path>|<mountpoint>` (charter §8): the
//! superblock, the newest ledger record, the set config, the client
//! registrations and a one-line summary, read
//! O_DIRECT through the io pool without the writer guard (a read-only
//! snapshot beside a live daemon is legal: both superblock copies and
//! every ledger slot are whole-sector, checksummed units; the control
//! tree is read at the newest CHECKPOINT — a verb's record committed since
//! and not yet checkpointed is in the journal, which a status never
//! replays), and the data volumes' REFERENCE CENSUS out of every slot
//! tree of the member's forest (records and distinct blocks per volume —
//! the pack census the smoke reads, the C8 oracle's durable half). A mountpoint target resolves
//! through the live mount's `.config` (`stats_cli.md` §7 Q8: the META path,
//! never a cache directory) and adds the `.stats` headline. Also the
//! preflight classifier `format` shares. Linux only.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const cli = @import("cli.zig");
const args = @import("args.zig");
const uri = @import("uri.zig");
const foreign_signature = @import("../meta/foreign_signature.zig");
const superblock = @import("../meta/superblock.zig");
const ledger = @import("../meta/ledger.zig");
const node = @import("../meta/node.zig");
const node_cache = @import("../meta/node_cache.zig");
const node_seq = @import("../meta/node_seq.zig");
const tree = @import("../meta/tree.zig");
const key = @import("../meta/key.zig");
const slot_state = @import("../meta/routing/slot_state.zig");
const appender_page = @import("../meta/sym/appender_page.zig");
const device_loader = @import("../meta/device_loader.zig");
const set_config = @import("../data/set_config.zig");
const uring_fs = @import("../io/uring_fs.zig");
const derive = @import("../core/derive.zig");
const target_mod = @import("target.zig");
const guard = @import("../meta/guard.zig");
const json_scan = @import("../core/json_scan.zig");

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
    /// Our magic reads zero but the first window carries another
    /// filesystem's (or block layer's) signature (`meta/foreign_signature.zig`):
    /// Blank to the mount, never to a bare `format`.
    foreign: foreign_signature.Hit,
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
        // A refusal on our magic's bytes may be another filesystem's own
        // magic at offset 0 (xfs, bfs, cramfs, …): name it rather than
        // print the bytes.
        if (try probeForeign(gpa, &dev, volume_len)) |hit| return .{ .prior = .{ .foreign = hit }, .sb = null, .slot = .primary, .volume_len = volume_len };
        var w: Io.Writer = .fixed(reason_buf);
        diag.write(&w) catch {};
        return .{ .prior = .{ .refused = w.buffered() }, .sb = null, .slot = .primary, .volume_len = volume_len };
    };
    return switch (classified.format) {
        .blank => blk: {
            // Blank to OUR magic: the foreign probe decides whether a bare
            // format may proceed (ext2/btrfs/jfs/… superblocks live past
            // byte 8 and read as zero there).
            if (try probeForeign(gpa, &dev, volume_len)) |hit| break :blk .{ .prior = .{ .foreign = hit }, .sb = null, .slot = classified.slot, .volume_len = volume_len };
            break :blk .{ .prior = .blank, .sb = null, .slot = classified.slot, .volume_len = volume_len };
        },
        .legacy => |v| .{ .prior = .{ .legacy = v }, .sb = null, .slot = classified.slot, .volume_len = volume_len },
        .v5 => |sb| .{ .prior = .formatted, .sb = sb, .slot = classified.slot, .volume_len = volume_len },
    };
}

/// The first window of the device (`foreign_signature.window_len`, rounded
/// to the O_DIRECT grain, no longer than the device) through the loader.
fn probeForeign(gpa: std.mem.Allocator, dev: *device_loader.DeviceLoader, volume_len: u64) error{ Io, OutOfMemory }!?foreign_signature.Hit {
    const want: u64 = @min(foreign_signature.window_len, volume_len);
    const len: usize = @intCast(want - want % superblock.sector_len);
    if (len == 0) return null;
    const window = try dev.readRange(gpa, 0, len);
    defer gpa.free(window);
    return foreign_signature.probe(window);
}

/// The data-device face of the same probe (`cli/format.zig`): the device
/// is opened by the data classifier; the window is read here.
pub fn probeForeignOnPath(gpa: std.mem.Allocator, env: *const std.process.Environ.Map, pool: *uring_fs.Pool, path: []const u8) ClassifyError!?foreign_signature.Hit {
    const volume_len = try device_loader.volumeLength(path);
    const mode = uring_fs.registerMetaDevice(gpa, env, path) catch return error.DeviceRefused;
    var dev: device_loader.DeviceLoader = .{ .gpa = gpa, .pool = pool, .path = path, .mode = mode, .ledger_base = 0, .bitmap_base = 0 };
    return probeForeign(gpa, &dev, volume_len);
}

/// Is a writer's layer-A guard held on `path` right now? Read off
/// `/proc/locks`: the probe takes no lock of its own.
pub fn writerGuardHeld(io: Io, path: []const u8) bool {
    return guard.proc.writerFlockHeld(io, path, null);
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

/// Appender 0's newest valid page on a symmetric volume (null on a flat one).
fn readAppender0Page(gpa: std.mem.Allocator, dev: *device_loader.DeviceLoader, sb: superblock.Superblock) ClassifyError!?appender_page.Page {
    if (!sb.symmetricPlaneStamped()) return null;
    const offs = appender_page.appender0PageOffsets(sb.journal.start);
    var bufs: [4][appender_page.PAGE_LEN]u8 = undefined;
    var imgs: [4]?[]const u8 = .{ null, null, null, null };
    for (offs, 0..) |off, i| {
        const img = dev.readRange(gpa, off, appender_page.PAGE_LEN) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
        };
        defer gpa.free(img);
        @memcpy(&bufs[i], img[0..appender_page.PAGE_LEN]);
        imgs[i] = &bufs[i];
    }
    return appender_page.newestValid(imgs) catch return error.Unexpected;
}

/// The reference records of every data volume `cfg` names, read offline
/// from EVERY slot tree of this member's forest (the native tree the
/// ledger names on slot 0's host, every guest tree tree 0's `slot_state`
/// records name): records and DISTINCT block indices (a block's refs
/// sort together within one tree; across trees the distinct count is a
/// sorted merge of each tree's blocks).
pub fn readRefCensus(gpa: std.mem.Allocator, io: Io, env: *const std.process.Environ.Map, pool: *uring_fs.Pool, path: []const u8, sb: superblock.Superblock, rec: *const ledger.LedgerRecord, cfg: *const set_config.Config, out: *std.ArrayList(VolCensus)) ClassifyError!void {
    const mode = uring_fs.registerMetaDevice(gpa, env, path) catch return error.DeviceRefused;
    var dev: device_loader.DeviceLoader = .{ .gpa = gpa, .pool = pool, .path = path, .mode = mode, .ledger_base = sb.root_ledger.start, .bitmap_base = sb.alloc_bitmap.start };
    const layout = node.NodeLayout.new(sb.node_size, null) catch return error.Unexpected;
    const cache = try node_cache.NodeCache.init(gpa, io, .{ .layout = layout, .heap_base = sb.heap.start, .budget_bytes = status_cache_budget_bytes }, dev.loader());
    defer cache.deinit();
    cache.setDurableTail(rec.journal_tail_seq);
    var seq = node_seq.NodeSeqHandle.shared(rec.node_seq_watermark);
    var trees: std.ArrayList(*tree.KvTree) = .empty;
    defer {
        for (trees.items) |t| t.destroy();
        trees.deinit(gpa);
    }
    if (rec.root(.native_slot) != null) {
        try trees.append(gpa, tree.KvTree.openFromLedger(gpa, cache, rec, .native_slot, &seq) catch |e| return mapTreeOpen(e));
    }
    {
        const control = tree.KvTree.openFromLedger(gpa, cache, rec, .control, &seq) catch |e| return mapTreeOpen(e);
        defer control.destroy();
        var kvs: std.ArrayList(tree.KeyValue) = .empty;
        defer tree.deinitKeyValues(gpa, &kvs);
        const scan = slot_state.scanRange();
        control.range(gpa, scan.start.slice(), scan.end.slice(), std.math.maxInt(usize), &kvs) catch |e| return mapTreeOpen(e);
        // Appender 0's page is the lessee's live home of the slots it
        // names (the two-homes law): its roots override tree 0's grant-
        // time words.
        var pages: std.ArrayList(appender_page.Page) = .empty;
        defer pages.deinit(gpa);
        if (try readAppender0Page(gpa, &dev, sb)) |p0| try pages.append(gpa, p0);
        var roots: std.ArrayList(slot_state.GuestRoot) = .empty;
        defer roots.deinit(gpa);
        slot_state.foldGuestRoots(gpa, kvs.items, pages.items, .projected, &roots) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Corrupt, error.Conflict => error.Unexpected,
        };
        for (roots.items) |r| {
            if (r.root.addr == 0) continue;
            try trees.append(gpa, tree.KvTree.open(gpa, cache, .{ .slot = key.guestForestSlot(r.slot) }, .{ .addr = r.root.addr, .seq = r.root.seq }, &seq) catch |e| return mapTreeOpen(e));
        }
    }
    for (cfg.data) |d| {
        var census: VolCensus = .{ .vol_tag = d.vol_tag, .references = 0, .blocks = 0 };
        const range = key.volumeRange(d.vol_tag);
        var blocks: std.ArrayList(u64) = .empty;
        defer blocks.deinit(gpa);
        for (trees.items) |t| {
            var cursor = range.lo;
            while (true) {
                var kvs: std.ArrayList(tree.KeyValue) = .empty;
                defer tree.deinitKeyValues(gpa, &kvs);
                t.range(gpa, cursor.slice(), range.hi.slice(), 512, &kvs) catch |e| return mapTreeOpen(e);
                var n: usize = 0;
                for (kvs.items) |kv| {
                    if (!key.lessThan(kv.key, range.hi.slice())) break;
                    const k = key.BlockRefKey.decode(kv.key) catch return error.Unexpected;
                    census.references += 1;
                    try blocks.append(gpa, k.blockIdx());
                    n += 1;
                }
                if (n < 512 or kvs.items.len == 0) break;
                cursor = key.sameKindSuccessor(kvs.items[kvs.items.len - 1].key) orelse break;
            }
        }
        std.mem.sort(u64, blocks.items, {}, std.sort.asc(u64));
        var last_block: ?u64 = null;
        for (blocks.items) |b| {
            if (last_block == null or last_block.? != b) census.blocks += 1;
            last_block = b;
        }
        try out.append(gpa, census);
    }
}

fn mapTreeOpen(e: anyerror) ClassifyError {
    return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        error.Io => error.Io,
        else => error.Unexpected,
    };
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

/// ONE control record of tree 0 at the newest checkpoint, read offline
/// over the node cache (caller frees): what `status` reads `set_config`
/// through, and what a joiner reads the `job:enroll` secret and the
/// manager's `endpoint:` record through BEFORE it holds any wire.
pub fn readControlRecord(gpa: std.mem.Allocator, io: Io, env: *const std.process.Environ.Map, pool: *uring_fs.Pool, path: []const u8, sb: superblock.Superblock, rec: *const ledger.LedgerRecord, k: []const u8) ClassifyError!?[]u8 {
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
    return t.lookup(gpa, k) catch |e| return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        error.Io => error.Io,
        else => error.Unexpected,
    };
}

/// Every control record of tree 0 inside `[start, end]` at the newest
/// checkpoint, read offline (caller frees each key and value): the
/// `member:` rows `clients` lists, the `dead_member:` records.
pub fn readControlRange(gpa: std.mem.Allocator, io: Io, env: *const std.process.Environ.Map, pool: *uring_fs.Pool, path: []const u8, sb: superblock.Superblock, rec: *const ledger.LedgerRecord, start: []const u8, end: []const u8, out: *std.ArrayList(tree.KeyValue)) ClassifyError!void {
    const mode = uring_fs.registerMetaDevice(gpa, env, path) catch return error.DeviceRefused;
    var dev: device_loader.DeviceLoader = .{ .gpa = gpa, .pool = pool, .path = path, .mode = mode, .ledger_base = sb.root_ledger.start, .bitmap_base = sb.alloc_bitmap.start };
    const layout = node.NodeLayout.new(sb.node_size, null) catch return error.Unexpected;
    const cache = try node_cache.NodeCache.init(gpa, io, .{ .layout = layout, .heap_base = sb.heap.start, .budget_bytes = status_cache_budget_bytes }, dev.loader());
    defer cache.deinit();
    cache.setDurableTail(rec.journal_tail_seq);
    var seq = node_seq.NodeSeqHandle.shared(rec.node_seq_watermark);
    const t = tree.KvTree.openFromLedger(gpa, cache, rec, .control, &seq) catch |e| return mapTreeOpen(e);
    defer t.destroy();
    t.range(gpa, start, end, std.math.maxInt(usize), out) catch |e| return mapTreeOpen(e);
}

/// Open the control tree offline over the node cache and look the record
/// up. `reason_buf` carries a refusal's text.
pub fn readSetConfig(gpa: std.mem.Allocator, io: Io, env: *const std.process.Environ.Map, pool: *uring_fs.Pool, path: []const u8, sb: superblock.Superblock, rec: *const ledger.LedgerRecord, reason_buf: []u8) ClassifyError!SetConfigRead {
    const value = (try readControlRecord(gpa, io, env, pool, path, sb, rec, set_config.CONTROL_KEY)) orelse return .absent;
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
        .legacy => |v| return w.print("{s}: a pre-v5 squeezefs format (version {d}), which this binary does not mount — reformat required\n", .{ meta_uri, v }),
        .refused => |reason| return w.print("{s}: superblock refused: {s}\n", .{ meta_uri, reason }),
        .foreign => |hit| return w.print("{s}: not formatted as SqueezeFS, but carries {f} — `squeezefs format` refuses it without --force\n", .{ meta_uri, hit }),
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

/// The `.stats` headline of a live mount: `fuse_ops`, `invariant_
/// tripwires`, `volume_failed`, the live inode population — read by key
/// name off the control inode (one LOOKUP + READ; the daemon renders).
pub fn writeMountHeadline(w: *Io.Writer, gpa: std.mem.Allocator, io: Io, mountpoint: []const u8) Io.Writer.Error!void {
    const path = std.fmt.allocPrint(gpa, "{s}/.stats", .{mountpoint}) catch return;
    defer gpa.free(path);
    const text = Io.Dir.cwd().readFileAlloc(io, path, gpa, .limited(16 << 20)) catch {
        try w.print("  live       {s}/.stats unreadable\n", .{mountpoint});
        return;
    };
    defer gpa.free(text);
    try w.print("  live       {s}: daemon pid {d}, fuse_ops {d}, in flight {d}, invariant_tripwires {d}, volume_failed {}, live inodes {d}, stats payload {d} B\n", .{
        mountpoint,
        json_scan.number(text, "pid") orelse 0,
        json_scan.number(text, "fuse_ops") orelse 0,
        json_scan.number(text, "fuse_ops_inflight") orelse 0,
        json_scan.number(text, "invariant_tripwires") orelse 0,
        json_scan.boolean(text, "volume_failed") orelse false,
        json_scan.number(text, "live_inodes") orelse 0,
        text.len,
    });
    // The holder's `df` rule (charter §5.5): used = the bitmap population
    // minus this writer's own unconsumed grant window; absent on a
    // `--single-writer` mount (both gauges read 0) and so not printed.
    const population = json_scan.number(text, "data_alloc_bitmap_population") orelse 0;
    if (population != 0) {
        const window = json_scan.number(text, "block_grant_window_remaining") orelse 0;
        try w.print("  alloc      bitmap population {d} block(s), own grant window {d} ⇒ df used {d} block(s)\n", .{ population, window, population -| window });
    }
}

pub fn run(p: cli.Process, s: args.Status) u8 {
    var resolved = target_mod.resolve(p.gpa, p.io, s.target) catch |e| {
        target_mod.writeRefusal(p.stderr, "status", s.target, e) catch {};
        return cli.exit_failed;
    };
    defer resolved.deinit(p.gpa);
    const meta_uri = resolved.meta_uri;
    var members: [uri.MAX_MEMBERS][]const u8 = undefined;
    var udiag: uri.Refusal = undefined;
    const meta = uri.parse(.meta, meta_uri, &members, &udiag) catch {
        p.stderr.print("squeezefs status: {f}\n", .{udiag}) catch {};
        return cli.exit_usage;
    };
    if (meta.paths.len != 1) {
        p.stderr.print("squeezefs status: {s} names {d} metadata volumes; this rung reports ONE\n", .{ meta_uri, meta.paths.len }) catch {};
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
    writeReport(p.stdout, meta_uri, snap, rec, cfg, census.items, writerGuardHeld(p.io, path)) catch return cli.exit_failed;
    if (snap.prior == .formatted) {
        // The registrations (the claim holder under the ONE staleness law).
        if (guard.gate.clients(p.gpa, p.io, p.env, pool, path, null)) |rows| {
            defer p.gpa.free(rows);
            if (rows.len == 0) {
                p.stdout.writeAll("  clients    none registered\n") catch {};
            } else for (rows) |r| {
                p.stdout.writeAll("  clients    ") catch {};
                r.writeText(p.stdout, path) catch {};
            }
        } else |e| p.stdout.print("  clients    unreadable ({s})\n", .{@errorName(e)}) catch {};
        if (resolved.via_mount) writeMountHeadline(p.stdout, p.gpa, p.io, s.target) catch {};
    }
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
    var conf = try set_config.birth(testing.allocator, sb.set_uuid, .{ .compression = .lz4, .block_size = 1 << 20 }, null, &vols);
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
    try testing.expect(!writerGuardHeld(testing.io, "/build/tmp/does-not-exist-status"));
    try testing.expect(!writerGuardHeld(testing.io, "/proc/self/status"));
}
