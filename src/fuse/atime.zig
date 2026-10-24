//! The access-time posture (charter §4 "Access time"; fstests generic/003,
//! generic/192). On FUSE the kernel DECIDES whether a read should move
//! atime — `fs/inode.c` `touch_atime` applies the mount's `MNT_NOATIME` /
//! `MNT_NODIRATIME` / `MNT_RELATIME` and the relatime rule to its in-core
//! inode — but never TELLS the daemon: `FUSE_READ` carries no flag,
//! `fuse_flush_times` writes back `FATTR_MTIME | FATTR_CTIME` alone, and
//! every read-class request ends in `fuse_invalidate_atime`, so the next
//! `stat` is a `GETATTR` whose reply OVERWRITES the kernel's atime with the
//! daemon's (`fuse_change_attributes_common`; atime is never in the cache
//! mask). A FUSE filesystem that wants ext4's semantics therefore applies
//! the posture itself, on the daemon side of READ, READDIR/READDIRPLUS and
//! READLINK — this module is that rule, PORTABLE and pure.
//!
//! THE LAW: the posture is the KERNEL MOUNT's — the per-mount options of
//! the daemon's own entry in `/proc/self/mountinfo` (`relatime` is the
//! kernel's default; `strictatime` is their absence; `noatime`;
//! `nodiratime`; `ro` in either options field) — read at mount and again at
//! every change of the mount table (`-o remount,strictatime` is
//! `do_reconfigure_mnt` → `set_mount_attributes` → `touch_mnt_namespace`,
//! which wakes `poll(POLLPRI)` on `/proc/self/mountinfo`: the daemon's
//! watcher re-reads its entry then, and the kernel sends nothing else on a
//! FUSE flag remount). `SQUEEZEFS_ATIME` overrides the posture word
//! (`auto` = the mount's). `lazytime` changes nothing here: the daemon's
//! apply is ALWAYS lazy — RAM at once, durable at the next commit (§5.2).
//!
//! `needsUpdate` restates `atime_needs_update` / `relatime_need_update`
//! exactly: never on a read-only mount (`mnt_get_write_access` fails on
//! `MNT_READONLY`; `fuse_invalidate_atime` skips `IS_RDONLY`), never under
//! `noatime`, never for a directory under `nodiratime`; under `relatime`
//! only when mtime ≥ atime, ctime ≥ atime, or atime is a day old or more
//! (whole seconds, the kernel's `tv_sec` compare); always under
//! `strictatime`; and never when the new stamp equals the old (no change).

const std = @import("std");

pub const Posture = enum(u2) {
    relatime,
    strictatime,
    noatime,

    pub fn name(self: Posture) []const u8 {
        return @tagName(self);
    }
};

/// A mount's whole atime law.
pub const Law = packed struct(u8) {
    posture: Posture = .relatime,
    nodiratime: bool = false,
    /// `ro` in the per-mount or the superblock options: no atime moves.
    read_only: bool = false,
    _pad: u4 = 0,

    pub const kernel_default: Law = .{};

    pub fn eql(a: Law, b: Law) bool {
        return @as(u8, @bitCast(a)) == @as(u8, @bitCast(b));
    }
};

/// Linux's relatime horizon: an atime a day old is refreshed by a read.
pub const RELATIME_HORIZON_S: i64 = 24 * 60 * 60;

const ns_per_s: i64 = std.time.ns_per_s;

/// The times a read-class op judges (i64 ns as the record stores them).
pub const Times = struct { atime: u64, mtime: u64, ctime: u64 };

fn seconds(ns: u64) i64 {
    return @divFloor(@as(i64, @bitCast(ns)), ns_per_s);
}

fn ge(a: u64, b: u64) bool {
    return @as(i64, @bitCast(a)) >= @as(i64, @bitCast(b));
}

/// `atime_needs_update(path, inode)` for a read-class op at `now`: true
/// iff the op must stamp `now` as the inode's atime.
pub fn needsUpdate(law: Law, is_dir: bool, t: Times, now: u64) bool {
    if (law.read_only) return false;
    if (law.posture == .noatime) return false;
    if (is_dir and law.nodiratime) return false;
    if (law.posture == .relatime and !relatimeNeedsUpdate(t, now)) return false;
    return t.atime != now;
}

/// `relatime_need_update`: mtime or ctime at or past atime, or atime a
/// day old (whole seconds).
pub fn relatimeNeedsUpdate(t: Times, now: u64) bool {
    if (ge(t.mtime, t.atime)) return true;
    if (ge(t.ctime, t.atime)) return true;
    return seconds(now) - seconds(t.atime) >= RELATIME_HORIZON_S;
}

/// The law of a mountinfo entry: `options` is the per-mount field
/// (`rw,nosuid,relatime`), `super_options` the superblock's (after the
/// source). `noatime` ⇒ noatime; `relatime` ⇒ relatime; neither ⇒
/// strictatime (`MS_STRICTATIME` clears both flags and the kernel prints
/// nothing for it); `ro` in either field is read-only.
pub fn lawOf(options: []const u8, super_options: []const u8) Law {
    var law: Law = .{ .posture = .strictatime };
    var it = std.mem.splitScalar(u8, options, ',');
    while (it.next()) |w| {
        if (std.mem.eql(u8, w, "noatime")) law.posture = .noatime;
        if (std.mem.eql(u8, w, "relatime") and law.posture != .noatime) law.posture = .relatime;
        if (std.mem.eql(u8, w, "nodiratime")) law.nodiratime = true;
        if (std.mem.eql(u8, w, "ro")) law.read_only = true;
    }
    var sit = std.mem.splitScalar(u8, super_options, ',');
    while (sit.next()) |w| {
        if (std.mem.eql(u8, w, "ro")) law.read_only = true;
    }
    return law;
}

/// The knob's word over the mount's law: `auto` keeps the mount's posture;
/// a name replaces the posture word alone (`nodiratime` and `ro` stay the
/// mount's).
pub fn override(mount: Law, knob: []const u8) Law {
    var law = mount;
    if (std.meta.stringToEnum(Posture, knob)) |p| law.posture = p;
    return law;
}

/// The live law a mount serves under: one byte, written by the watcher,
/// read once per read-class op (a relaxed load: a posture flip is
/// observed by the next op, never torn).
pub const Live = struct {
    word: std.atomic.Value(u8),

    pub fn init(law: Law) Live {
        return .{ .word = .init(@bitCast(law)) };
    }

    pub fn current(self: *const Live) Law {
        return @bitCast(self.word.load(.monotonic));
    }

    /// Install `law`; true iff it differs from the one standing.
    pub fn set(self: *Live, law: Law) bool {
        return self.word.swap(@bitCast(law), .monotonic) != @as(u8, @bitCast(law));
    }
};

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

const testing = std.testing;

const S: u64 = @intCast(ns_per_s);

test "relatime restates relatime_need_update: mtime ≥ atime or ctime ≥ atime refresh; both behind atime do not until a day has passed (whole seconds); equal stamps are 'at or past'" {
    const now: u64 = 1000 * S;
    const rel: Law = .{ .posture = .relatime };
    // Fresh file: atime = mtime = ctime (equal ⇒ refresh).
    try testing.expect(needsUpdate(rel, false, .{ .atime = 10 * S, .mtime = 10 * S, .ctime = 10 * S }, now));
    // Read once: atime ahead of both ⇒ no refresh.
    try testing.expect(!needsUpdate(rel, false, .{ .atime = 20 * S, .mtime = 10 * S, .ctime = 10 * S }, now));
    // A write moved mtime past atime ⇒ refresh; a chmod moved ctime alone ⇒ refresh.
    try testing.expect(needsUpdate(rel, false, .{ .atime = 20 * S, .mtime = 21 * S, .ctime = 10 * S }, now));
    try testing.expect(needsUpdate(rel, false, .{ .atime = 20 * S, .mtime = 10 * S, .ctime = 20 * S + 1 }, now));
    // A day, in WHOLE seconds (`now.tv_sec - atime.tv_sec`): 86399.999 s
    // later is not a day; 86400 s is; and an atime of 20.999 s read at
    // 86420.000 s is a day too (20 → 86420).
    const old: u64 = 20 * S;
    try testing.expect(!needsUpdate(rel, false, .{ .atime = old, .mtime = 1, .ctime = 1 }, old + 86400 * S - 1));
    try testing.expect(needsUpdate(rel, false, .{ .atime = old, .mtime = 1, .ctime = 1 }, old + 86400 * S));
    try testing.expect(needsUpdate(rel, false, .{ .atime = old + S - 1, .mtime = 1, .ctime = 1 }, old + 86400 * S));
    try testing.expect(!needsUpdate(rel, false, .{ .atime = old + S, .mtime = 1, .ctime = 1 }, old + 86400 * S + S - 1));
    // Signed: a pre-epoch mtime is behind a post-epoch atime.
    const neg: u64 = @bitCast(@as(i64, -5 * ns_per_s));
    try testing.expect(!needsUpdate(rel, false, .{ .atime = 20 * S, .mtime = neg, .ctime = neg }, now));
    try testing.expect(needsUpdate(rel, false, .{ .atime = neg, .mtime = neg, .ctime = neg }, now));
}

test "strictatime always moves; noatime never; nodiratime spares directories alone; ro never; a stamp equal to the one standing is no update" {
    const now: u64 = 1000 * S;
    const fresh: Times = .{ .atime = 10 * S, .mtime = 10 * S, .ctime = 10 * S };
    const read: Times = .{ .atime = 20 * S, .mtime = 10 * S, .ctime = 10 * S };
    const strict: Law = .{ .posture = .strictatime };
    try testing.expect(needsUpdate(strict, false, fresh, now));
    try testing.expect(needsUpdate(strict, false, read, now));
    try testing.expect(needsUpdate(strict, true, read, now));
    try testing.expect(!needsUpdate(strict, false, .{ .atime = now, .mtime = 1, .ctime = 1 }, now));
    const no: Law = .{ .posture = .noatime };
    try testing.expect(!needsUpdate(no, false, fresh, now));
    try testing.expect(!needsUpdate(no, true, fresh, now));
    const nodir: Law = .{ .posture = .relatime, .nodiratime = true };
    try testing.expect(needsUpdate(nodir, false, fresh, now));
    try testing.expect(!needsUpdate(nodir, true, fresh, now));
    const strict_nodir: Law = .{ .posture = .strictatime, .nodiratime = true };
    try testing.expect(!needsUpdate(strict_nodir, true, read, now));
    try testing.expect(needsUpdate(strict_nodir, false, read, now));
    const ro: Law = .{ .posture = .strictatime, .read_only = true };
    try testing.expect(!needsUpdate(ro, false, fresh, now));
}

test "lawOf reads the mountinfo fields: relatime (the default), its absence is strictatime, noatime, nodiratime beside either, ro in the per-mount or the superblock field" {
    try testing.expect(lawOf("rw,nosuid,nodev,relatime", "rw,user_id=0,group_id=0").eql(.{ .posture = .relatime }));
    try testing.expect(lawOf("rw,nosuid,nodev", "rw,user_id=0").eql(.{ .posture = .strictatime }));
    try testing.expect(lawOf("rw,noatime", "rw").eql(.{ .posture = .noatime }));
    try testing.expect(lawOf("rw,relatime,nodiratime", "rw").eql(.{ .posture = .relatime, .nodiratime = true }));
    try testing.expect(lawOf("rw,nodiratime", "rw").eql(.{ .posture = .strictatime, .nodiratime = true }));
    try testing.expect(lawOf("ro,relatime", "rw").eql(.{ .posture = .relatime, .read_only = true }));
    try testing.expect(lawOf("rw,relatime", "ro,user_id=0").eql(.{ .posture = .relatime, .read_only = true }));
    try testing.expect(lawOf("ro", "ro").eql(.{ .posture = .strictatime, .read_only = true }));
    // A token that merely contains the word is not the word.
    try testing.expect(lawOf("rw,norelatime", "rw").eql(.{ .posture = .strictatime }));
}

test "the knob's override replaces the posture word alone; auto (or any unknown word) keeps the mount's" {
    const mount: Law = .{ .posture = .relatime, .nodiratime = true, .read_only = false };
    try testing.expect(override(mount, "auto").eql(mount));
    try testing.expect(override(mount, "strictatime").eql(.{ .posture = .strictatime, .nodiratime = true }));
    try testing.expect(override(mount, "noatime").eql(.{ .posture = .noatime, .nodiratime = true }));
    try testing.expect(override(.{ .posture = .noatime, .read_only = true }, "relatime").eql(.{ .posture = .relatime, .read_only = true }));
}

test "Live: one byte, the default is the kernel's relatime; set reports a change and the next load sees it" {
    var live: Live = .init(Law.kernel_default);
    try testing.expect(live.current().eql(.{ .posture = .relatime }));
    try testing.expect(live.set(.{ .posture = .strictatime, .nodiratime = true }));
    try testing.expect(!live.set(.{ .posture = .strictatime, .nodiratime = true }));
    try testing.expect(live.current().eql(.{ .posture = .strictatime, .nodiratime = true }));
    try testing.expect(live.set(.{ .posture = .noatime }));
    try testing.expectEqual(Posture.noatime, live.current().posture);
}
