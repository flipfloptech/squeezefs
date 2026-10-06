//! `squeezefs status sqmeta://<path>` (charter §8): the superblock, the
//! newest ledger record and a one-line summary, read O_DIRECT through the
//! io pool without the writer guard (a read-only snapshot beside a live
//! daemon is legal: both superblock copies and every ledger slot are
//! whole-sector, checksummed units). Also the preflight classifier
//! `format` shares. Linux only.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const cli = @import("cli.zig");
const args = @import("args.zig");
const uri = @import("uri.zig");
const superblock = @import("../meta/superblock.zig");
const ledger = @import("../meta/ledger.zig");
const device_loader = @import("../meta/device_loader.zig");
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

/// The report (public so the smoke can read it from a buffer).
pub fn writeReport(w: *Io.Writer, meta_uri: []const u8, snap: Snapshot, rec: ?ledger.LedgerRecord, mounted: bool) Io.Writer.Error!void {
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
    if (snap.sb) |sb| {
        rec = newestLedger(p.gpa, p.env, pool, path, sb) catch |e| {
            p.stderr.print("squeezefs status: cannot read the ledger of {s}: {s}\n", .{ path, @errorName(e) }) catch {};
            return cli.exit_failed;
        };
    }
    writeReport(p.stdout, s.meta_uri, snap, rec, writerGuardHeld(path)) catch return cli.exit_failed;
    p.stdout.flush() catch {};
    return if (snap.prior == .formatted) cli.exit_ok else cli.exit_failed;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the report: a blank volume is one line; a v4 volume prints the superblock, the ledger and a summary naming the mount state" {
    var out: Io.Writer.Allocating = .init(testing.allocator);
    defer out.deinit();
    try writeReport(&out.writer, "sqmeta:///x", .{ .prior = .blank, .sb = null, .slot = .primary, .volume_len = 1 << 20 }, null, false);
    try testing.expect(std.mem.indexOf(u8, out.written(), "not formatted") != null);
    out.clearRetainingCapacity();
    const sb = superblock.testFixture();
    var roots = [_]ledger.TreeRoot{ .{ .id = .control, .node_addr = sb.heap.start, .node_seq = 7 }, .{ .id = .native_slot, .node_addr = sb.heap.start + sb.node_size, .node_seq = 8 } };
    const rec = ledger.bootstrapRecord(&roots, 42, 9, ledger.MembershipStamp.oneMember(sb.set_uuid));
    try writeReport(&out.writer, "sqmeta:///x", .{ .prior = .formatted, .sb = sb, .slot = .primary, .volume_len = sb.volume_len }, rec, true);
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
