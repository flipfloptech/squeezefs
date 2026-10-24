//! `/proc/self/mountinfo` (proc(5)): the kernel's own view of what is
//! mounted where — the ONE source every unmount-side judgement reads. A
//! `stat` of a mountpoint is a request to the very daemon being judged
//! (and a GETATTR to ourselves from inside the daemon); the mount table is
//! not. Pure over bytes; the reader is a fixed-buffer `readFile` (procfs
//! reports size 0, so a size-driven read returns nothing).
//!
//! A line: `id parent major:minor root mountpoint options [optional
//! fields…] - fstype source super_options`. Mountpoints with spaces are
//! escaped `\040`; this port's mountpoints are its own paths (no spaces),
//! so the exact field compare stands. The LAST line naming a mountpoint
//! wins: a later mount over the same path shadows the earlier one.

const std = @import("std");
const fuzzkit = @import("fuzzkit.zig");
const Io = std.Io;

pub const path = "/proc/self/mountinfo";

/// The table's upper bound for one read (procfs size-0 law): a thousand
/// mounts at ~200 B each; a table past it reports what fits.
pub const READ_BYTES: usize = 1 << 20;

pub const Entry = struct {
    /// The mount's id: unique per mount instance (a remount at the same
    /// path is a new id; a detached mount keeps its id for its holders).
    id: u64,
    parent_id: u64,
    dev_major: u32,
    dev_minor: u32,
    mountpoint: []const u8,
    /// The per-mount options (`rw,nosuid,relatime`: the vfsmount's
    /// `MNT_*` flags — the atime posture lives here).
    options: []const u8,
    fstype: []const u8,
    /// The superblock's options (the last field; `ro` here is `SB_RDONLY`).
    super_options: []const u8,

    /// `fuse` or `fuse.<subtype>`.
    pub fn isFuse(self: Entry) bool {
        return std.mem.eql(u8, self.fstype, "fuse") or std.mem.startsWith(u8, self.fstype, "fuse.");
    }
};

/// One line → its entry; null for a line that is not a mount record.
pub fn parseLine(line: []const u8) ?Entry {
    var it = std.mem.tokenizeScalar(u8, line, ' ');
    const id = std.fmt.parseInt(u64, it.next() orelse return null, 10) catch return null;
    const parent = std.fmt.parseInt(u64, it.next() orelse return null, 10) catch return null;
    const majmin = it.next() orelse return null;
    const colon = std.mem.indexOfScalar(u8, majmin, ':') orelse return null;
    const major = std.fmt.parseInt(u32, majmin[0..colon], 10) catch return null;
    const minor = std.fmt.parseInt(u32, majmin[colon + 1 ..], 10) catch return null;
    _ = it.next() orelse return null;
    const mp = it.next() orelse return null;
    const options = it.next() orelse return null;
    // Skip the optional fields up to the separator.
    while (it.next()) |tok| {
        if (std.mem.eql(u8, tok, "-")) break;
    } else return null;
    const fstype = it.next() orelse return null;
    const super_options = if (it.next() != null) (it.next() orelse "") else "";
    return .{ .id = id, .parent_id = parent, .dev_major = major, .dev_minor = minor, .mountpoint = mp, .options = options, .fstype = fstype, .super_options = super_options };
}

/// The entry mounted at exactly `mountpoint` (the last line wins).
pub fn entryAt(text: []const u8, mountpoint: []const u8) ?Entry {
    var found: ?Entry = null;
    var lines = std.mem.splitScalar(u8, text, '\n');
    while (lines.next()) |line| {
        const e = parseLine(line) orelse continue;
        if (std.mem.eql(u8, e.mountpoint, mountpoint)) found = e;
    }
    return found;
}

/// The mount with this id (`statx` `STATX_MNT_ID` names a file's mount).
/// Its `major:minor` is the superblock's `s_dev` — what `/proc/locks`
/// prints — where statx reports btrfs's per-subvolume anonymous device.
pub fn entryOf(text: []const u8, id: u64) ?Entry {
    var lines = std.mem.splitScalar(u8, text, '\n');
    while (lines.next()) |line| {
        const e = parseLine(line) orelse continue;
        if (e.id == id) return e;
    }
    return null;
}

/// Is a mount with this id still in the table (attached somewhere)?
pub fn hasId(text: []const u8, id: u64) bool {
    return entryOf(text, id) != null;
}

/// Is a mount of this superblock (`major:minor`) attached somewhere? The
/// anonymous device a FUSE connection takes is reissued once its
/// superblock dies, so a connection id that is attached again under a
/// new mount is another connection's.
pub fn hasDev(text: []const u8, major: u32, minor: u32) bool {
    var lines = std.mem.splitScalar(u8, text, '\n');
    while (lines.next()) |line| {
        const e = parseLine(line) orelse continue;
        if (e.dev_major == major and e.dev_minor == minor) return true;
    }
    return false;
}

/// How many of a superblock's other mounts `othersOfSuperblock` names
/// (the rest are counted).
pub const OTHERS_NAMED = 4;

/// The mounts of one superblock other than a daemon's own.
pub const OtherMounts = struct {
    count: usize = 0,
    named: [OTHERS_NAMED][]const u8 = undefined,

    /// The mountpoints named (slices of the table), at most `OTHERS_NAMED`.
    pub fn names(self: *const OtherMounts) []const []const u8 {
        return self.named[0..@min(self.count, OTHERS_NAMED)];
    }
};

/// Every mount of the superblock `major:minor` except the daemon's own — the
/// entry `own_id` at `own_path`: a bind of it (anywhere, or stacked over its
/// own path), a propagated copy, the daemon's own mount `mount --move`d
/// elsewhere (its id at another path). Meaningful only while the daemon's own
/// mount is attached: its reference keeps the superblock alive, so the
/// number IS its connection's. Once its own `umount2` dropped the last
/// reference the kernel reissues the number at once, and an entry carrying
/// it is another connection's.
pub fn othersOfSuperblock(text: []const u8, own_id: u64, own_path: []const u8, major: u32, minor: u32) OtherMounts {
    var out: OtherMounts = .{};
    var lines = std.mem.splitScalar(u8, text, '\n');
    while (lines.next()) |line| {
        const e = parseLine(line) orelse continue;
        if (e.dev_major != major or e.dev_minor != minor) continue;
        if (e.id == own_id and std.mem.eql(u8, e.mountpoint, own_path)) continue;
        if (out.count < OTHERS_NAMED) out.named[out.count] = e.mountpoint;
        out.count += 1;
    }
    return out;
}

/// How many mounts stand on a dentry of the superblock `major:minor`: every
/// entry whose PARENT is a mount of that superblock — a bind or another
/// filesystem mounted inside its tree (under any of its mounts: a bind of
/// it elsewhere carries its dentries too), a propagated copy of one. The
/// FUSE daemon's whole-view invalidation would detach each at its next walk
/// (decision 0003 §BL.3). A mount of the superblock itself is not counted
/// by being one — only by standing on another.
pub fn mountsOnSuperblock(text: []const u8, major: u32, minor: u32) usize {
    var ids: [64]u64 = undefined;
    var n_ids: usize = 0;
    var overflow = false;
    var lines = std.mem.splitScalar(u8, text, '\n');
    while (lines.next()) |line| {
        const e = parseLine(line) orelse continue;
        if (e.dev_major != major or e.dev_minor != minor) continue;
        if (n_ids == ids.len) {
            overflow = true;
            break;
        }
        ids[n_ids] = e.id;
        n_ids += 1;
    }
    var count: usize = 0;
    lines = std.mem.splitScalar(u8, text, '\n');
    while (lines.next()) |line| {
        const e = parseLine(line) orelse continue;
        if (std.mem.indexOfScalar(u64, ids[0..n_ids], e.parent_id) != null) {
            count += 1;
            continue;
        }
        // Past the id buffer the rest are judged by the parent's device.
        if (overflow) if (entryOf(text, e.parent_id)) |p| {
            if (p.dev_major == major and p.dev_minor == minor) count += 1;
        };
    }
    return count;
}

/// The first mount whose parent is `id` (a mount stacked inside it makes
/// the plain `umount2` of `id` EBUSY).
pub fn firstChildOf(text: []const u8, id: u64) ?Entry {
    var lines = std.mem.splitScalar(u8, text, '\n');
    while (lines.next()) |line| {
        const e = parseLine(line) orelse continue;
        if (e.parent_id == id) return e;
    }
    return null;
}

/// The table into `buf` (owned by the caller, `READ_BYTES` long for the
/// full view); empty on a host without procfs.
pub fn read(io: Io, buf: []u8) []const u8 {
    return Io.Dir.cwd().readFile(io, path, buf) catch "";
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

const testing = std.testing;

const sample =
    \\22 1 0:21 / / rw,relatime - ext4 /dev/root rw
    \\90 22 0:45 / /build/tmp/mnt-7 rw,nosuid,nodev,relatime shared:12 - fuse squeezefs rw,user_id=0,group_id=0
    \\91 22 0:46 / /build/tmp/mnt-77 rw,relatime - fuse.sshfs x rw
    \\92 22 0:47 / /build/tmp/mnt-8 rw,relatime - tmpfs tmpfs rw
    \\93 90 0:48 / /build/tmp/mnt-7/sub rw,relatime - tmpfs tmpfs rw
    \\
;

test "parseLine: id, parent, major:minor, mountpoint and fstype past the optional fields; a non-record line is null" {
    const e = parseLine("90 22 0:45 / /build/tmp/mnt-7 rw,nosuid,nodev,relatime shared:12 - fuse squeezefs rw,user_id=0").?;
    try testing.expectEqual(@as(u64, 90), e.id);
    try testing.expectEqual(@as(u64, 22), e.parent_id);
    try testing.expectEqual(@as(u32, 0), e.dev_major);
    try testing.expectEqual(@as(u32, 45), e.dev_minor);
    try testing.expectEqualStrings("/build/tmp/mnt-7", e.mountpoint);
    try testing.expectEqualStrings("fuse", e.fstype);
    try testing.expect(e.isFuse());
    try testing.expectEqualStrings("rw,nosuid,nodev,relatime", e.options);
    try testing.expectEqualStrings("rw,user_id=0", e.super_options);
    try testing.expect(parseLine("") == null);
    try testing.expect(parseLine("not a record") == null);
    try testing.expect(parseLine("90 22 0:45 / /m rw") == null);
    try testing.expect(parseLine("90 22 045 / /m rw - fuse x y") == null);
}

test "parseLine: the two options fields — the per-mount flags (ro, noatime, nodiratime; no relatime word = strictatime) and the superblock's; a line ending at the fstype has empty super options" {
    const strict = parseLine("95 22 0:50 / /build/tmp/mnt-9 rw,nodiratime - fuse.squeezefs sqmeta rw,user_id=0,group_id=0,default_permissions,allow_other").?;
    try testing.expectEqualStrings("rw,nodiratime", strict.options);
    try testing.expectEqualStrings("rw,user_id=0,group_id=0,default_permissions,allow_other", strict.super_options);
    const ro = parseLine("96 22 0:51 / /build/tmp/mnt-10 ro,noatime shared:40 master:3 - fuse.squeezefs sqmeta ro,user_id=0").?;
    try testing.expectEqualStrings("ro,noatime", ro.options);
    try testing.expectEqualStrings("ro,user_id=0", ro.super_options);
    const bare = parseLine("97 22 0:52 / /m rw,relatime - fuse").?;
    try testing.expectEqualStrings("rw,relatime", bare.options);
    try testing.expectEqualStrings("", bare.super_options);
}

test "entryAt: the exact mountpoint, the last line wins, a prefix is not a match; fuse and fuse.<subtype> are FUSE, tmpfs is not" {
    try testing.expect(entryAt(sample, "/build/tmp/mnt-7").?.isFuse());
    try testing.expect(entryAt(sample, "/build/tmp/mnt-77").?.isFuse());
    try testing.expect(!entryAt(sample, "/build/tmp/mnt-8").?.isFuse());
    try testing.expect(entryAt(sample, "/build/tmp/mnt") == null);
    try testing.expectEqual(@as(u64, 22), entryAt(sample, "/").?.id);
    const shadowed = sample ++ "94 22 0:49 / /build/tmp/mnt-7 rw,relatime - tmpfs tmpfs rw\n";
    try testing.expect(!entryAt(shadowed, "/build/tmp/mnt-7").?.isFuse());
    try testing.expectEqual(@as(u64, 94), entryAt(shadowed, "/build/tmp/mnt-7").?.id);
}

test "entryOf, hasId, hasDev and firstChildOf: the mount by id carries its device; a detached mount's id is gone; a device is found under any path; the tmpfs under mnt-7 is its child" {
    const m = entryOf(sample, 92).?;
    try testing.expectEqual(@as(u32, 0), m.dev_major);
    try testing.expectEqual(@as(u32, 47), m.dev_minor);
    try testing.expect(entryOf(sample, 9000) == null);
    try testing.expect(hasId(sample, 90));
    try testing.expect(!hasId(sample, 9000));
    try testing.expect(hasDev(sample, 0, 45));
    try testing.expect(hasDev(sample, 0, 48));
    try testing.expect(!hasDev(sample, 0, 49));
    try testing.expect(!hasDev(sample, 8, 45));
    try testing.expectEqualStrings("/build/tmp/mnt-7/sub", firstChildOf(sample, 90).?.mountpoint);
    try testing.expect(firstChildOf(sample, 91) == null);
}

test "othersOfSuperblock: the daemon's own entry (its id AT its path) is excluded; a bind elsewhere, a bind stacked over its path and its own mount moved count and are named up to OTHERS_NAMED; another device is not its business; after its own unmount a fresh mount carrying the reissued number is counted — the caller must not ask then" {
    // Our mount: id 90, 0:45 at mnt-7. A bind of it at mnt-7-bind (id 95) and
    // one stacked over our own path (id 96); a tmpfs under it (0:48) is not ours.
    const binds = sample ++
        \\95 22 0:45 / /build/tmp/mnt-7-bind rw,relatime - fuse squeezefs rw
        \\96 22 0:45 / /build/tmp/mnt-7 rw,relatime - fuse squeezefs rw
        \\
    ;
    const own = othersOfSuperblock(sample, 90, "/build/tmp/mnt-7", 0, 45);
    try testing.expectEqual(@as(usize, 0), own.count);
    try testing.expectEqual(@as(usize, 0), own.names().len);
    const held = othersOfSuperblock(binds, 90, "/build/tmp/mnt-7", 0, 45);
    try testing.expectEqual(@as(usize, 2), held.count);
    try testing.expectEqualStrings("/build/tmp/mnt-7-bind", held.names()[0]);
    try testing.expectEqualStrings("/build/tmp/mnt-7", held.names()[1]);
    // Our own mount moved: its id stands at another path — a mount of our
    // superblock the orderly stop cannot unmount by path, counted and named.
    const moved = "22 1 0:21 / / rw,relatime - ext4 /dev/root rw\n90 22 0:45 / /build/tmp/elsewhere rw,relatime - fuse squeezefs rw\n";
    const m = othersOfSuperblock(moved, 90, "/build/tmp/mnt-7", 0, 45);
    try testing.expectEqual(@as(usize, 1), m.count);
    try testing.expectEqualStrings("/build/tmp/elsewhere", m.names()[0]);
    // The names are capped; the count is not.
    var many: std.Io.Writer.Allocating = .init(testing.allocator);
    defer many.deinit();
    try many.writer.writeAll(sample);
    for (0..OTHERS_NAMED + 3) |i| try many.writer.print("{d} 22 0:45 / /build/tmp/b{d} rw - fuse squeezefs rw\n", .{ 100 + i, i });
    const capped = othersOfSuperblock(many.written(), 90, "/build/tmp/mnt-7", 0, 45);
    try testing.expectEqual(OTHERS_NAMED + 3, capped.count);
    try testing.expectEqual(OTHERS_NAMED, capped.names().len);
    try testing.expectEqualStrings("/build/tmp/b0", capped.names()[0]);
    // After the daemon's own unmount the table has no entry of its mount and
    // the kernel has reissued 0:45 to a fresh mount of another daemon (gate
    // 94: shard 1/16's daemon aborted shard 8/16's connection by it): the
    // verdict counts it — which is why the session asks BEFORE its unmount.
    const reissued = "22 1 0:21 / / rw,relatime - ext4 /dev/root rw\n97 22 0:45 / /build/tmp/mnt-1718481 rw,relatime - fuse squeezefs rw\n";
    try testing.expectEqual(@as(usize, 1), othersOfSuperblock(reissued, 90, "/build/tmp/mnt-7", 0, 45).count);
}

test "mountsOnSuperblock: a mount whose parent is a mount of the superblock — on its own mount, on a bind of it elsewhere — counts; the superblock's own mounts, a mount beside it and one on another superblock do not; past the id buffer the parent's device judges" {
    // 0:45 at mnt-7 (id 90) carries the tmpfs 93 at mnt-7/sub.
    try testing.expectEqual(@as(usize, 1), mountsOnSuperblock(sample, 0, 45));
    try testing.expectEqual(@as(usize, 0), mountsOnSuperblock(sample, 0, 46));
    try testing.expectEqual(@as(usize, 0), mountsOnSuperblock(sample, 0, 48));
    // generic/410's shape: the head 0:45 at H; A (0:50) at H/mpA on it and
    // its bind at H/mpB; B (0:51) at H/mpA/dir and its propagated copy at
    // H/mpB/dir — on A's dentry `dir` through both of A's mounts.
    const g410 =
        \\22 1 0:21 / / rw - ext4 /dev/root rw
        \\90 22 0:45 / /H rw - fuse squeezefs rw
        \\100 90 0:50 / /H/mpA rw shared:1 - fuse squeezefs rw
        \\101 90 0:50 / /H/mpB rw shared:1 - fuse squeezefs rw
        \\102 100 0:51 / /H/mpA/dir rw shared:2 - fuse squeezefs rw
        \\103 101 0:51 / /H/mpB/dir rw shared:2 - fuse squeezefs rw
        \\
    ;
    try testing.expectEqual(@as(usize, 2), mountsOnSuperblock(g410, 0, 45));
    try testing.expectEqual(@as(usize, 2), mountsOnSuperblock(g410, 0, 50));
    try testing.expectEqual(@as(usize, 0), mountsOnSuperblock(g410, 0, 51));
    // More mounts of the superblock than the id buffer holds: the ones past
    // it still carry what stands on them.
    var many: std.Io.Writer.Allocating = .init(testing.allocator);
    defer many.deinit();
    try many.writer.writeAll("22 1 0:21 / / rw - ext4 /dev/root rw\n");
    for (0..70) |i| try many.writer.print("{d} 22 0:45 / /b{d} rw - fuse squeezefs rw\n", .{ 100 + i, i });
    try many.writer.writeAll("500 169 0:60 / /b69/sub rw - tmpfs tmpfs rw\n");
    try testing.expectEqual(@as(usize, 1), mountsOnSuperblock(many.written(), 0, 45));
}

fn lineFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [512]u8 = undefined;
    const len = smith.slice(&buf);
    const text = buf[0..len];
    _ = parseLine(text);
    _ = entryAt(text, "/m");
    _ = hasId(text, 1);
    _ = hasDev(text, 0, 1);
    _ = othersOfSuperblock(text, 1, "/m", 0, 1);
    _ = mountsOnSuperblock(text, 0, 1);
    _ = firstChildOf(text, 1);
}

test "fuzz: the mountinfo line parser is total on arbitrary text" {
    try fuzzkit.fuzz({}, lineFuzz, .{ .corpus = &.{ fuzzkit.slice(sample), fuzzkit.slice("1 2 3:4 / /m o - t s") } });
}
