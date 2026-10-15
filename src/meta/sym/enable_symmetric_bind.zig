//! `squeezefs volume enable-symmetric`'s device binding (Linux; the pure
//! half is `enable_symmetric.zig`): the writer door held for the verb's
//! life, the images read O_DIRECT, the stranded-link census, the plan
//! executed in its barrier order. The door is the D0 ladder's layers A and
//! B2 as REFUSALS — the exclusive `flock` on a dedicated fd (`EWOULDBLOCK`
//! = a live mount on this host) and the on-volume writer claim through the
//! gate's probe (heartbeat-fresh under another identity = a live mount on
//! another host; a stale or dead-pid claim is residue the next mount
//! reclaims and no bar to the conversion). The verb takes no claim of its
//! own: it writes no record a claim would cover, and the flock it holds is
//! exactly what a concurrent `mount` on this host meets as `Busy`.

const std = @import("std");
const builtin = @import("builtin");
const Io = std.Io;
const linux = std.os.linux;
const Allocator = std.mem.Allocator;
const log = @import("../../core/log.zig");
const clock = @import("../../core/clock.zig");
const uring_fs = @import("../../io/uring_fs.zig");
const align_probe = @import("../../io/align_probe.zig");
const derive = @import("../../core/derive.zig");
const device_loader = @import("../device_loader.zig");
const geometry = @import("../geometry.zig");
const superblock = @import("../superblock.zig");
const ledger = @import("../ledger.zig");
const alloc_core = @import("../alloc_core.zig");
const alloc_ext = @import("../alloc_ext.zig");
const guard = @import("../guard.zig");
const appender_dir = @import("appender_dir.zig");
const enable = @import("enable_symmetric.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("enable_symmetric_bind is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const Error = error{ DeviceRefused, NotFormatted, Mounted, Io } || enable.Error;

/// The door's refusals beside the plan's, rendered in ONE place.
pub const Refusal = union(enum) {
    /// Layer A: another process on this host holds the writer lock.
    mounted_here,
    /// Layer B2: a heartbeat-fresh writer claim under another identity.
    mounted_elsewhere: struct { claim: guard.claim.Claim, now_ns: i64 },
    plan: enable.Refusal,

    pub fn write(self: Refusal, w: *std.Io.Writer) std.Io.Writer.Error!void {
        switch (self) {
            .mounted_here => try w.writeAll("the volume is MOUNTED — another squeezefs process on this host holds its writer lock; unmount it first (the conversion needs the volume quiesced with an empty replay window)"),
            .mounted_elsewhere => |m| {
                try w.writeAll("the volume is MOUNTED on another host — its writer claim is heartbeat-fresh (");
                try m.claim.writeSummary(w, m.now_ns);
                try w.writeAll("); unmount it there first");
            },
            .plan => |p| try p.write(w),
        }
    }
};

pub const Options = struct {
    env: *const std.process.Environ.Map,
    pool: ?*uring_fs.Pool = null,
};

pub const Outcome = struct {
    link: superblock.ExtentRef,
    resumed: bool,
    ledger_seq: ?u64,
    /// The ring proper behind the four slots.
    ring_bytes: u64,
    journal_len: u64,
};

/// The dedicated door fd: `flock(LOCK_EX | LOCK_NB)`, held until `close`
/// (a shared holder — udev's probe — waited out: `guard.proc.flockExclusive`).
fn takeDoor(io: Io, path: []const u8) Error!linux.fd_t {
    var zbuf: [linux.PATH_MAX]u8 = undefined;
    const z = align_probe.pathZ(&zbuf, path) catch return error.DeviceRefused;
    const rc = linux.open(z, .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) return error.DeviceRefused;
    const fd: linux.fd_t = @intCast(rc);
    errdefer _ = linux.close(fd);
    return switch (guard.proc.flockExclusive(io, fd, null)) {
        .SUCCESS => fd,
        .AGAIN => error.Mounted,
        else => error.DeviceRefused,
    };
}

const PageReader = struct {
    dev: *device_loader.DeviceLoader,
    gpa: Allocator,

    fn read(ctx: *anyopaque, offset: u64, out: *[appender_dir.PAGE_LEN]u8) error{Io}!void {
        const self: *PageReader = @ptrCast(@alignCast(ctx));
        const img = self.dev.readRange(self.gpa, offset, appender_dir.PAGE_LEN) catch return error.Io;
        defer self.gpa.free(img);
        @memcpy(out, img[0..appender_dir.PAGE_LEN]);
    }

    fn reader(self: *PageReader) appender_dir.PageReader {
        return .{ .ctx = self, .read = &read };
    }
};

/// The verb over one volume (module doc). `io` is the executor's (the
/// claim probe replays the ring into private trees through the cache).
pub fn enableSymmetric(gpa: Allocator, io: Io, path: []const u8, opts: Options, diag: ?*Refusal) Error!Outcome {
    const door = takeDoor(io, path) catch |e| {
        if (e == error.Mounted) if (diag) |d| {
            d.* = .mounted_here;
        };
        return e;
    };
    defer _ = linux.close(door);
    const mode = uring_fs.registerMetaDevice(gpa, opts.env, path) catch return error.DeviceRefused;
    const pool = opts.pool orelse (uring_fs.Pool.init(gpa, uring_fs.Config.resolve(opts.env, derive.Facts.probe())) catch return error.DeviceRefused);
    defer if (opts.pool == null) pool.deinit();
    const volume_len = device_loader.volumeLength(path) catch return error.DeviceRefused;
    var dev: device_loader.DeviceLoader = .{ .gpa = gpa, .pool = pool, .path = path, .mode = mode, .ledger_base = 0, .bitmap_base = 0 };

    // The superblock, both copies; the stamp's generation continues the
    // newest valid one (DUR-5's newest-valid-wins).
    const sector0 = try dev.readRange(gpa, 0, superblock.sector_len);
    defer gpa.free(sector0);
    const backup: ?[]u8 = if (superblock.backupOffsetFor(volume_len)) |off| try dev.readRange(gpa, off, superblock.sector_len) else null;
    defer if (backup) |b| gpa.free(b);
    var sb_diag: superblock.Refusal = undefined;
    const classified = superblock.classifyVolumeImages(sector0, backup, volume_len, &sb_diag) catch return error.Corrupt;
    const sb = switch (classified.format) {
        .v5 => |s| s,
        else => return error.NotFormatted,
    };
    if (sb.symmetricPlaneStamped()) {
        if (diag) |d| d.* = .{ .plan = .{ .already_symmetric = .{ .dir = sb.appender_dir } } };
        return error.AlreadySymmetric;
    }

    // Layer B2: a live writer elsewhere.
    {
        const raw = guard.gate.probeClaim(gpa, io, opts.env, pool, path) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.DeviceRefused => error.DeviceRefused,
            error.NotFormatted => error.NotFormatted,
            error.Corrupt => error.Corrupt,
            error.Io => error.Io,
        };
        defer if (raw) |r| gpa.free(r);
        const now = clock.realtimeCoarseNs();
        switch (guard.claim.classify(raw, guard.proc.localIdentity(), now, guard.proc.probe)) {
            .fresh_foreign => |f| {
                if (diag) |d| d.* = .{ .mounted_elsewhere = .{ .claim = f.claim, .now_ns = now } };
                return error.Mounted;
            },
            .reclaimable, .stale_foreign => {},
        }
    }

    // The images.
    const ledger_img = try dev.readRange(gpa, sb.root_ledger.start, @intCast(sb.root_ledger.len));
    defer gpa.free(ledger_img);
    var mounted = (try ledger.newestValid(gpa, ledger_img)) orelse return error.Corrupt;
    defer mounted.deinit(gpa);
    const ring = try dev.readRange(gpa, sb.journal.start, @intCast(sb.journal.len));
    defer gpa.free(ring);
    const bitmap = try dev.readRange(gpa, sb.alloc_bitmap.start, @intCast(sb.alloc_bitmap.len));
    defer gpa.free(bitmap);

    // The resume census over the claimed extents.
    const stranded = blk: {
        const total = sb.totalExtents();
        var alloc = alloc_ext.ExtentAllocator.load(gpa, bitmap, total, alloc_core.compactionReserveExtents(total), alloc_ext.builder_pending_cap, mounted.journal_tail_seq, &.{}, null) catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            error.MalformedDelta => return error.Corrupt,
        };
        defer alloc.deinit();
        var pr: PageReader = .{ .dev = &dev, .gpa = gpa };
        break :blk try enable.findStrandedLink(pr.reader(), &alloc, sb);
    };

    var pdiag: enable.Refusal = undefined;
    var p = enable.plan(gpa, .{
        .sb = sb,
        .mounted = mounted,
        .ring = ring,
        .grain = mode.grain(),
        .bitmap = bitmap,
        .stranded_link = stranded,
        .sb_generation = superblock.nextSuperblockGeneration(sector0, backup),
    }, &pdiag) catch |e| {
        if (e != error.OutOfMemory) if (diag) |d| {
            d.* = .{ .plan = pdiag };
        };
        return e;
    };
    defer p.deinit();
    try dev.writeSteps(p.steps);
    const ring_proper = geometry.fixedRingExtent(p.superblock);
    log.info("{s}: SYMMETRIC_PLANE enabled — appender directory at heap extent {d} ({s}), appender 0's page slots at the fixed ring's first {d} B, ring proper {d} B of a {d} B journal, ledger seq {d} ({s})", .{ path, (p.link.start - sb.heap.start) / sb.node_size, if (p.resumed) "a crashed run's link resumed" else "claimed", geometry.appender0_reserved_bytes, ring_proper.len, sb.journal.len, p.ledger_seq orelse mounted.seq, if (p.ledger_seq != null) "restated at the claim's generation" else "unchanged" });
    return .{ .link = p.link, .resumed = p.resumed, .ledger_seq = p.ledger_seq, .ring_bytes = ring_proper.len, .journal_len = sb.journal.len };
}

// ---------------------------------------------------------------------------
// Tests (Linux; a file-backed flat set under /build/tmp over the real
// backend): the conversion end to end, the three refusals, the resume.
// ---------------------------------------------------------------------------

const testing = std.testing;
const meta_set = @import("../meta_set.zig");
const key = @import("../key.zig");
const record = @import("../record.zig");
const join = @import("join.zig");
const page = @import("appender_page.zig");

const iv_value = (record.InodeValue{ .mode = 0o100644, .nlink = 1 }).encode();

fn refusalText(buf: []u8, r: Refusal) []const u8 {
    var w: std.Io.Writer = .fixed(buf);
    r.write(&w) catch unreachable;
    return w.buffered();
}

/// `n` inode records in the native keyspace through the flat writer
/// (committed; the close checkpoints and leaves the window empty).
fn writeTree(s: *meta_set.TestSet, first: u64, n: u64, keys: *std.ArrayList([]const u8)) !void {
    const set = try s.open();
    defer set.close();
    const be = set.volumes[0];
    try testing.expect(be.sym == null);
    var i: u64 = 0;
    while (i < n) : (i += 1) {
        const t = try be.begin(@src());
        const k = try testing.allocator.dupe(u8, &key.inodeKey(first + i));
        try keys.append(testing.allocator, k);
        try t.stagePut(key.TREE_INODES, k, &iv_value);
        try be.commit(t);
    }
}

fn expectAllPresent(set: *meta_set.MetaSet, keys: []const []const u8) !void {
    for (keys) |k| {
        const got = try set.lookup(testing.allocator, key.TREE_INODES, k);
        defer if (got) |g| testing.allocator.free(g);
        try testing.expect(got != null);
    }
}

fn freeKeys(keys: *std.ArrayList([]const u8)) void {
    for (keys.items) |k| testing.allocator.free(k);
    keys.deinit(testing.allocator);
}

/// A byte copy of the live image (a kill: the process state abandoned).
fn snapshotImage(src: []const u8, dst: []const u8) !void {
    var sz: [linux.PATH_MAX]u8 = undefined;
    var dz: [linux.PATH_MAX]u8 = undefined;
    const sfd_rc = linux.open(try align_probe.pathZ(&sz, src), .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
    if (linux.errno(sfd_rc) != .SUCCESS) return error.CannotOpen;
    const sfd: linux.fd_t = @intCast(sfd_rc);
    defer _ = linux.close(sfd);
    const dfd_rc = linux.open(try align_probe.pathZ(&dz, dst), .{ .ACCMODE = .WRONLY, .CLOEXEC = true, .CREAT = true, .TRUNC = true }, 0o644);
    if (linux.errno(dfd_rc) != .SUCCESS) return error.CannotCreate;
    const dfd: linux.fd_t = @intCast(dfd_rc);
    defer _ = linux.close(dfd);
    var buf: [1 << 20]u8 = undefined;
    var off: u64 = 0;
    while (true) {
        const n = linux.pread(sfd, &buf, buf.len, @intCast(off));
        if (linux.errno(n) != .SUCCESS) return error.ReadFailed;
        if (n == 0) break;
        const w = linux.pwrite(dfd, &buf, n, @intCast(off));
        if (linux.errno(w) != .SUCCESS or w != n) return error.WriteFailed;
        off += n;
    }
    if (linux.errno(linux.fsync(dfd)) != .SUCCESS) return error.SyncFailed;
}

fn unlinkPath(path: []const u8) void {
    var zbuf: [linux.PATH_MAX]u8 = undefined;
    if (align_probe.pathZ(&zbuf, path)) |z| {
        _ = linux.unlink(z);
    } else |_| {}
}

test "linux: enable-symmetric end to end — a --single-writer volume with a tree, unmounted clean → the verb (the link claimed, the ring re-based, the stamp) → the SYMMETRIC mount: the manager's region 0 Live on the converted page slots, every record present, a second writer JOINS in-process and appends into its own slot, a remount after the join holds everything; the verb refuses the converted volume as AlreadySymmetric" {
    const gpa = testing.allocator;
    const s = try meta_set.TestSet.createClass(gpa, "enable-sym", 1, .single_writer);
    defer s.destroy();
    var keys: std.ArrayList([]const u8) = .empty;
    defer freeKeys(&keys);
    try writeTree(s, 100, 24, &keys);
    var diag: Refusal = undefined;
    const out = try enableSymmetric(gpa, s.io(), s.paths[0], .{ .env = &s.env }, &diag);
    try testing.expect(!out.resumed);
    try testing.expect(out.ledger_seq != null);
    try testing.expectEqual(out.journal_len - geometry.appender0_reserved_bytes, out.ring_bytes);
    {
        const set = try s.open();
        defer set.close();
        const be = set.volumes[0];
        try testing.expect(be.sb.symmetricPlaneStamped());
        try testing.expectEqual(out.link, be.sb.appender_dir);
        const aset = be.sym orelse return error.TestUnexpectedResult;
        try testing.expect(be.replay.clean_unmount);
        try expectAllPresent(set, keys.items);
        try testing.expect(aset.manager.lease == .held);
        // The native slot is leased by region 0; a write lands.
        {
            const t = try be.begin(@src());
            const k = try gpa.dupe(u8, &key.inodeKey(500));
            try keys.append(gpa, k);
            try t.stagePut(key.TREE_INODES, k, &iv_value);
            try be.commit(t);
        }
        // A second writer joins in-process and appends into a slot of its
        // own (the first touch acquires it).
        const r1 = try aset.joinInProcess(join.HARNESS_MOUNT_SLOT_BASE + 3);
        _ = try be.mintGuestTree(3);
        var i: u64 = 0;
        while (i < 5) : (i += 1) {
            const k = try gpa.dupe(u8, &key.inodeKey(key.guestLocalIno(3, 10 + i)));
            try keys.append(gpa, k);
            var tries: u32 = 0;
            while (tries < 2000) : (tries += 1) {
                if (aset.appendPut(r1, key.TREE_INODES, k, &iv_value)) |_| break else |e| switch (e) {
                    error.SlotBusy, error.Deferred => {},
                    else => return e,
                }
            }
            try testing.expect(tries < 2000);
        }
        try be.checkpointNow();
        try expectAllPresent(set, keys.items);
        try aset.leaveInProcess(r1);
    }
    // The remount: everything stands; appender 0's page is Live again at
    // term 2 on the converted slots.
    {
        const set = try s.open();
        defer set.close();
        const be = set.volumes[0];
        try testing.expect(be.replay.clean_unmount);
        try expectAllPresent(set, keys.items);
        const offs = page.appender0PageOffsets(be.sb.journal.start);
        var bufs: [page.PAGE_SLOTS][page.PAGE_LEN]u8 = undefined;
        var imgs: [page.PAGE_SLOTS]?[]const u8 = .{ null, null, null, null };
        for (offs, 0..) |off, i| {
            const img = try be.dev.readRange(gpa, off, page.PAGE_LEN);
            defer gpa.free(img);
            @memcpy(&bufs[i], img[0..page.PAGE_LEN]);
            imgs[i] = &bufs[i];
        }
        const p0 = (try page.newestValid(imgs)).?;
        try testing.expect(p0.state == .live and p0.is_manager and p0.appender_id == 0);
        try testing.expectEqual(@as(u64, 2), p0.term);
    }
    try testing.expectError(error.AlreadySymmetric, enableSymmetric(gpa, s.io(), s.paths[0], .{ .env = &s.env }, &diag));
    var buf: [512]u8 = undefined;
    try testing.expect(std.mem.indexOf(u8, refusalText(&buf, diag), "already carries SYMMETRIC_PLANE") != null);
}

test "linux: enable-symmetric refuses a MOUNTED volume (the flock) and a NON-EMPTY replay window (a kill image with acked records the mount never replayed), each typed and loud; the kill image mounted and unmounted clean converts" {
    const gpa = testing.allocator;
    const s = try meta_set.TestSet.createClass(gpa, "enable-sym-refuse", 1, .single_writer);
    defer s.destroy();
    var kill_buf: [160]u8 = undefined;
    const kill = try std.fmt.bufPrint(&kill_buf, "{s}.kill", .{s.paths[0]});
    defer unlinkPath(kill);
    var keys: std.ArrayList([]const u8) = .empty;
    defer freeKeys(&keys);
    var diag: Refusal = undefined;
    var buf: [512]u8 = undefined;
    {
        const set = try s.open();
        defer set.close();
        const be = set.volumes[0];
        try be.checkpointNow();
        for (0..6) |i| {
            const t = try be.begin(@src());
            const k = try gpa.dupe(u8, &key.inodeKey(200 + i));
            try keys.append(gpa, k);
            try t.stagePut(key.TREE_INODES, k, &iv_value);
            try be.commit(t);
        }
        try be.fsync();
        // Mounted: the flock refuses before any read.
        try testing.expectError(error.Mounted, enableSymmetric(gpa, s.io(), s.paths[0], .{ .env = &s.env }, &diag));
        try testing.expect(std.mem.indexOf(u8, refusalText(&buf, diag), "MOUNTED") != null);
        try testing.expect(diag == .mounted_here);
        // The kill image: six acked records in the window, no checkpoint.
        try snapshotImage(s.paths[0], kill);
    }
    // The window is not empty on the kill image: refused, nothing written.
    try testing.expectError(error.WindowNotEmpty, enableSymmetric(gpa, s.io(), kill, .{ .env = &s.env }, &diag));
    try testing.expectEqual(@as(u64, 6), diag.plan.window_not_empty.entries);
    try testing.expect(std.mem.indexOf(u8, refusalText(&buf, diag), "not clean") != null);
    // The kill image mounted (the replay) and unmounted clean: converts,
    // and every acked record of the window stands on the symmetric mount.
    {
        const set = try meta_set.MetaSet.open(gpa, s.io(), &.{kill}, .{ .env = &s.env, .cache_budget_bytes = 64 << 20, .cpus = 4 });
        defer set.close();
        try testing.expect(!set.volumes[0].replay.clean_unmount);
        try expectAllPresent(set, keys.items);
    }
    const out = try enableSymmetric(gpa, s.io(), kill, .{ .env = &s.env }, &diag);
    try testing.expect(!out.resumed);
    {
        const set = try meta_set.MetaSet.open(gpa, s.io(), &.{kill}, .{ .env = &s.env, .cache_budget_bytes = 64 << 20, .cpus = 4 });
        defer set.close();
        try testing.expect(set.volumes[0].sym != null);
        try testing.expect(set.volumes[0].replay.clean_unmount);
        try expectAllPresent(set, keys.items);
    }
}

test "linux: enable-symmetric's resume over the device — a run killed after the claim (every step but the stamp) leaves a flat volume whose claimed KVAD extent the re-run finds by content and stamps, the ledger seq unchanged by the re-run; the mount is the symmetric class with every record" {
    const gpa = testing.allocator;
    const s = try meta_set.TestSet.createClass(gpa, "enable-sym-resume", 1, .single_writer);
    defer s.destroy();
    var keys: std.ArrayList([]const u8) = .empty;
    defer freeKeys(&keys);
    try writeTree(s, 300, 10, &keys);
    // The crash: the plan's steps up to the stamp, written by hand.
    var link: superblock.ExtentRef = undefined;
    var seq_after_claim: u64 = 0;
    {
        const path = s.paths[0];
        const mode = try uring_fs.registerMetaDevice(gpa, &s.env, path);
        const pool = try uring_fs.Pool.init(gpa, uring_fs.Config.resolve(&s.env, derive.Facts.probe()));
        defer pool.deinit();
        const volume_len = try device_loader.volumeLength(path);
        var dev: device_loader.DeviceLoader = .{ .gpa = gpa, .pool = pool, .path = path, .mode = mode, .ledger_base = 0, .bitmap_base = 0 };
        const sector0 = try dev.readRange(gpa, 0, superblock.sector_len);
        defer gpa.free(sector0);
        const sb = (try superblock.classifyVolumeImages(sector0, null, volume_len, null)).format.v5;
        const ledger_img = try dev.readRange(gpa, sb.root_ledger.start, @intCast(sb.root_ledger.len));
        defer gpa.free(ledger_img);
        var mounted = (try ledger.newestValid(gpa, ledger_img)).?;
        defer mounted.deinit(gpa);
        const ring = try dev.readRange(gpa, sb.journal.start, @intCast(sb.journal.len));
        defer gpa.free(ring);
        const bitmap = try dev.readRange(gpa, sb.alloc_bitmap.start, @intCast(sb.alloc_bitmap.len));
        defer gpa.free(bitmap);
        var p = try enable.plan(gpa, .{ .sb = sb, .mounted = mounted, .ring = ring, .grain = mode.grain(), .bitmap = bitmap, .stranded_link = null, .sb_generation = superblock.nextSuperblockGeneration(sector0, null) }, null);
        defer p.deinit();
        link = p.link;
        seq_after_claim = p.ledger_seq.?;
        try dev.writeSteps(p.steps[0 .. p.steps.len - 2]);
    }
    var diag: Refusal = undefined;
    const out = try enableSymmetric(gpa, s.io(), s.paths[0], .{ .env = &s.env }, &diag);
    try testing.expect(out.resumed);
    try testing.expectEqual(link, out.link);
    try testing.expectEqual(@as(?u64, null), out.ledger_seq);
    const set = try s.open();
    defer set.close();
    const be = set.volumes[0];
    try testing.expect(be.sym != null);
    try testing.expectEqual(link, be.sb.appender_dir);
    try testing.expectEqual(seq_after_claim, be.mounted.seq);
    try testing.expect(be.replay.clean_unmount);
    try expectAllPresent(set, keys.items);
}
