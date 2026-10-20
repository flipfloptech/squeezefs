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
    fstype: []const u8,

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
    // Skip the options and the optional fields up to the separator.
    while (it.next()) |tok| {
        if (std.mem.eql(u8, tok, "-")) break;
    } else return null;
    const fstype = it.next() orelse return null;
    return .{ .id = id, .parent_id = parent, .dev_major = major, .dev_minor = minor, .mountpoint = mp, .fstype = fstype };
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
    try testing.expect(parseLine("") == null);
    try testing.expect(parseLine("not a record") == null);
    try testing.expect(parseLine("90 22 0:45 / /m rw") == null);
    try testing.expect(parseLine("90 22 045 / /m rw - fuse x y") == null);
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

fn lineFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [512]u8 = undefined;
    const len = smith.slice(&buf);
    const text = buf[0..len];
    _ = parseLine(text);
    _ = entryAt(text, "/m");
    _ = hasId(text, 1);
    _ = hasDev(text, 0, 1);
    _ = firstChildOf(text, 1);
}

test "fuzz: the mountinfo line parser is total on arbitrary text" {
    try fuzzkit.fuzz({}, lineFuzz, .{ .corpus = &.{ fuzzkit.slice(sample), fuzzkit.slice("1 2 3:4 / /m o - t s") } });
}
