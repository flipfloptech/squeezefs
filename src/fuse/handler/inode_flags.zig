//! Inode attribute flags (charter §4 "inode flags", §5.1 `flags`): the
//! Linux `FS_IOC_GETFLAGS`/`FS_IOC_SETFLAGS` and `FS_IOC_FSGETXATTR`/
//! `FS_IOC_FSSETXATTR` pairs as a FUSE daemon receives them, the record
//! bits they name, and the SET verdict. Portable: no I/O, no allocation.
//!
//! What the kernel sends (linux 7.3 `fs/file_attr.c`, `fs/fuse/ioctl.c`
//! `fuse_fileattr_get`/`fuse_fileattr_set` → `fuse_priv_ioctl`): every
//! `ioctl(2)` of the four commands is caught by the VFS (`do_vfs_ioctl`),
//! never by the file's own ioctl op, and reaches the daemon as ONE
//! `FUSE_IOCTL` per call — `fh` a private open's handle (0 under zero-
//! message opens; a directory's is an OPENDIR/RELEASEDIR pair around the
//! call), `flags` = `FUSE_IOCTL_DIR` on a directory (`FUSE_IOCTL_32BIT` on
//! a 32-bit kernel), `cmd` the NATIVE command (a compat caller's
//! `FS_IOC32_*` is translated before the VFS dispatch), `arg` 0,
//! `in_size`/`out_size` the argument's byte size by the command's
//! direction (4 for the flags word, 28 for `struct fsxattr`), the in bytes
//! after the header. The reply is `fuse_ioctl_out{result 0, flags 0,
//! in_iovs 0, out_iovs 0}` followed by EXACTLY `out_size` bytes (the
//! kernel's `fuse_simple_request` refuses any other length; a
//! `FUSE_IOCTL_RETRY` there is -EIO); a refusal is the header-only
//! `-errno`, which `fuse_priv_ioctl` returns to the caller as is (ENOSYS
//! becomes ENOTTY: an unknown command). `vfs_fileattr_set` ALWAYS reads
//! the old state first through `fuse_fileattr_get` with `fsx_valid` — so a
//! `chattr +i` arrives as FSGETXATTR then SETFLAGS, and an `xfs_io chattr`
//! as FSGETXATTR then FSSETXATTR: both pairs are served and agree. The
//! kernel owns the capability checks before the ioctl reaches the daemon
//! (`inode_owner_or_capable`, `CAP_LINUX_IMMUTABLE` for a change of
//! IMMUTABLE or APPEND in `fileattr_set_prepare`).
//!
//! The law: IMMUTABLE and APPEND are the record's (`record.INODE_FLAG_*`);
//! every other bit (SYNC, NOATIME, NODUMP, NOCOW, DAX, PROJINHERIT, the
//! project id, the extent-size hints) is refused EOPNOTSUPP when a SET
//! asks for it — the record holds no field the kernel would honour for
//! them (a FUSE inode is `S_NOATIME` for the kernel whatever the daemon
//! says, `S_SYNC` is never set on one) — and a bit never set is never
//! reported. The kernel sets no `S_IMMUTABLE`/`S_APPEND` on a FUSE inode
//! (`fuse_change_attributes` reads only `FUSE_ATTR_SUBMOUNT`/`DAX` from
//! the attr's flags), so the daemon enforces both itself in every mutating
//! handler (`kv_fs`: the reference served a GDS ioctl only — decision 0002
//! row 553).

const std = @import("std");
const linux = std.os.linux;
const record = @import("../../meta/record.zig");

/// `_IOR('f', 1, long)` / `_IOW('f', 2, long)`: `chattr`/`lsattr`'s pair.
pub const FS_IOC_GETFLAGS: u32 = linux.IOCTL.IOR('f', 1, c_long);
pub const FS_IOC_SETFLAGS: u32 = linux.IOCTL.IOW('f', 2, c_long);
/// The 32-bit forms (`_IOR('f', 1, int)`): what a 32-bit kernel sends.
pub const FS_IOC32_GETFLAGS: u32 = linux.IOCTL.IOR('f', 1, c_int);
pub const FS_IOC32_SETFLAGS: u32 = linux.IOCTL.IOW('f', 2, c_int);
/// `_IOR('X', 31, struct fsxattr)` / `_IOW('X', 32, struct fsxattr)`:
/// `xfs_io`'s pair and the VFS's own old-state read.
pub const FS_IOC_FSGETXATTR: u32 = linux.IOCTL.IOR('X', 31, Fsxattr);
pub const FS_IOC_FSSETXATTR: u32 = linux.IOCTL.IOW('X', 32, Fsxattr);

/// The `FS_*_FL` word's bits (`include/uapi/linux/fs.h`).
pub const FS_IMMUTABLE_FL: u32 = 0x00000010;
pub const FS_APPEND_FL: u32 = 0x00000020;
/// The `FS_XFLAG_*` word's bits (`struct fsxattr.fsx_xflags`).
pub const FS_XFLAG_IMMUTABLE: u32 = 0x00000008;
pub const FS_XFLAG_APPEND: u32 = 0x00000010;

/// `struct fsxattr` (28 B): the second pair's argument.
pub const Fsxattr = extern struct {
    fsx_xflags: u32,
    fsx_extsize: u32,
    /// Get only (the kernel ignores it on a set).
    fsx_nextents: u32,
    fsx_projid: u32,
    fsx_cowextsize: u32,
    fsx_pad: [8]u8,

    pub const zero: Fsxattr = .{ .fsx_xflags = 0, .fsx_extsize = 0, .fsx_nextents = 0, .fsx_projid = 0, .fsx_cowextsize = 0, .fsx_pad = @splat(0) };
};

comptime {
    std.debug.assert(@sizeOf(Fsxattr) == 28);
    std.debug.assert(@offsetOf(Fsxattr, "fsx_cowextsize") == 16);
}

/// The byte size of the flags word the first pair carries: the kernel
/// passes `sizeof(unsigned int)` whatever the command encodes (`long`).
pub const FLAGS_WORD_LEN: usize = @sizeOf(u32);

pub const Command = enum {
    get_flags,
    set_flags,
    get_xattr,
    set_xattr,

    pub fn isSet(self: Command) bool {
        return self == .set_flags or self == .set_xattr;
    }
};

/// The command a request's `cmd` names, or null (not ours: ENOTTY).
pub fn command(cmd: u32) ?Command {
    return switch (cmd) {
        FS_IOC_GETFLAGS, FS_IOC32_GETFLAGS => .get_flags,
        FS_IOC_SETFLAGS, FS_IOC32_SETFLAGS => .set_flags,
        FS_IOC_FSGETXATTR => .get_xattr,
        FS_IOC_FSSETXATTR => .set_xattr,
        else => null,
    };
}

/// Is `cmd` one of the two SET commands (the lifecycle's guard choice:
/// a flag change is a mutation of the inode's write law).
pub fn isSetCommand(cmd: u32) bool {
    return if (command(cmd)) |c| c.isSet() else false;
}

/// The record's attribute bits as the `FS_*_FL` word.
pub fn flWord(record_flags: u32) u32 {
    var w: u32 = 0;
    if (record_flags & record.INODE_FLAG_IMMUTABLE != 0) w |= FS_IMMUTABLE_FL;
    if (record_flags & record.INODE_FLAG_APPEND != 0) w |= FS_APPEND_FL;
    return w;
}

/// The record's attribute bits as the `fsx_xflags` word.
pub fn xflagsWord(record_flags: u32) u32 {
    var w: u32 = 0;
    if (record_flags & record.INODE_FLAG_IMMUTABLE != 0) w |= FS_XFLAG_IMMUTABLE;
    if (record_flags & record.INODE_FLAG_APPEND != 0) w |= FS_XFLAG_APPEND;
    return w;
}

/// The `struct fsxattr` a GET answers: the flags, nothing else (no project
/// quota, no extent-size hints, no extent count).
pub fn fsxattrOf(record_flags: u32) Fsxattr {
    var x = Fsxattr.zero;
    x.fsx_xflags = xflagsWord(record_flags);
    return x;
}

pub const GetError = error{
    /// `out_size` is not the command's argument size.
    InvalidArgument,
};

/// Encode a GET's reply bytes into `out` (the reply window): exactly
/// `out_size` bytes, the command's argument.
pub fn encodeGet(cmd: Command, record_flags: u32, out_size: u32, out: []u8) GetError!usize {
    switch (cmd) {
        .get_flags => {
            if (out_size != FLAGS_WORD_LEN or out.len < FLAGS_WORD_LEN) return error.InvalidArgument;
            std.mem.writeInt(u32, out[0..FLAGS_WORD_LEN], flWord(record_flags), .little);
            return FLAGS_WORD_LEN;
        },
        .get_xattr => {
            if (out_size != @sizeOf(Fsxattr) or out.len < @sizeOf(Fsxattr)) return error.InvalidArgument;
            out[0..@sizeOf(Fsxattr)].* = std.mem.toBytes(fsxattrOf(record_flags));
            return @sizeOf(Fsxattr);
        },
        .set_flags, .set_xattr => return error.InvalidArgument,
    }
}

pub const SetError = error{
    /// The in bytes are not the command's argument.
    InvalidArgument,
    /// A bit the record cannot hold (or a project id / extent-size hint)
    /// asked for: EOPNOTSUPP, the kernel's answer for a flag a filesystem
    /// does not implement.
    NotSupported,
};

/// The record flags a SET asks for, over the current ones: the attribute
/// bits replaced by the request's, the format's bits (STRIPE) untouched.
/// A bit outside IMMUTABLE/APPEND that the request SETS refuses; one it
/// leaves clear is nothing (a `chattr -i` sends the word it read back).
pub fn resolveSet(cmd: Command, current_record_flags: u32, in: []const u8) SetError!u32 {
    var want: u32 = 0;
    switch (cmd) {
        .set_flags => {
            if (in.len != FLAGS_WORD_LEN) return error.InvalidArgument;
            const fl = std.mem.readInt(u32, in[0..FLAGS_WORD_LEN], .little);
            if (fl & ~(FS_IMMUTABLE_FL | FS_APPEND_FL) != 0) return error.NotSupported;
            if (fl & FS_IMMUTABLE_FL != 0) want |= record.INODE_FLAG_IMMUTABLE;
            if (fl & FS_APPEND_FL != 0) want |= record.INODE_FLAG_APPEND;
        },
        .set_xattr => {
            if (in.len != @sizeOf(Fsxattr)) return error.InvalidArgument;
            const x = std.mem.bytesToValue(Fsxattr, in[0..@sizeOf(Fsxattr)]);
            if (x.fsx_xflags & ~(FS_XFLAG_IMMUTABLE | FS_XFLAG_APPEND) != 0) return error.NotSupported;
            if (x.fsx_extsize != 0 or x.fsx_projid != 0 or x.fsx_cowextsize != 0) return error.NotSupported;
            if (x.fsx_xflags & FS_XFLAG_IMMUTABLE != 0) want |= record.INODE_FLAG_IMMUTABLE;
            if (x.fsx_xflags & FS_XFLAG_APPEND != 0) want |= record.INODE_FLAG_APPEND;
        },
        .get_flags, .get_xattr => return error.InvalidArgument,
    }
    return (current_record_flags & ~record.INODE_FLAGS_ATTR) | want;
}

// ---------------------------------------------------------------------------
// The enforcement law (one place; `kv_fs` calls these with the record).
// ---------------------------------------------------------------------------

/// A WRITE to `v` at `offset` under the file's open flags `o_flags` and
/// its current EOF `size`: refused iff immutable, or append-only without
/// `O_APPEND` (the kernel's `may_open` law, restated at the WRITE under
/// zero-message opens) or at any offset but EOF (an `O_APPEND` write the
/// kernel positioned at its own `i_size`; a stale one never lands inside
/// the file).
pub fn writeRefused(v: record.InodeValue, o_flags: u32, offset: u64, size: u64) bool {
    if (v.isImmutable()) return true;
    if (!v.isAppendOnly()) return false;
    const o_append: u32 = @bitCast(linux.O{ .APPEND = true });
    return o_flags & o_append == 0 or offset != size;
}

/// An OPEN of `v` (served opens only): write intent on an immutable file,
/// write intent without `O_APPEND` or any `O_TRUNC` on an append-only one
/// (`may_open`).
pub fn openRefused(v: record.InodeValue, o_flags: u32) bool {
    const o_accmode: u32 = 0b11; // O_ACCMODE: RDONLY 0, WRONLY 1, RDWR 2
    const o_append: u32 = @bitCast(linux.O{ .APPEND = true });
    const o_trunc: u32 = @bitCast(linux.O{ .TRUNC = true });
    const write_intent = o_flags & o_accmode != 0;
    if (v.isImmutable()) return write_intent or o_flags & o_trunc != 0;
    if (!v.isAppendOnly()) return false;
    return (write_intent and o_flags & o_append == 0) or o_flags & o_trunc != 0;
}

/// A SETATTR of `v`: everything on an immutable inode (`may_setattr`:
/// mode/owner/explicit times, plus `ATTR_TOUCH` and the size — `vfs_
/// truncate`'s `inode_permission(MAY_WRITE)`); on an append-only one the
/// mode, owner, size (`vfs_truncate`/`do_sys_ftruncate`) and EXPLICIT
/// times; a `utimes(NULL)`/`touch` (the `*_NOW` forms) and a ctime echo
/// pass.
pub fn setattrRefused(v: record.InodeValue, has_mode_owner: bool, has_size: bool, times_set: bool) bool {
    if (v.isImmutable()) return true;
    if (!v.isAppendOnly()) return false;
    return has_mode_owner or has_size or times_set;
}

/// A FALLOCATE of `v` with `mode`: immutable refuses; append-only admits
/// space preallocation alone (`vfs_fallocate`: `mode & ~KEEP_SIZE` on an
/// append-only inode is EPERM).
pub fn fallocateRefused(v: record.InodeValue, mode: u32) bool {
    const keep_size: u32 = 0x01; // FALLOC_FL_KEEP_SIZE
    if (v.isImmutable()) return true;
    return v.isAppendOnly() and mode & ~keep_size != 0;
}

/// An entry made inside directory `dir` (create, mkdir, mknod, symlink,
/// link's new name, rename's destination name): `may_create` →
/// `inode_permission(MAY_WRITE)` — an immutable directory refuses; an
/// append-only one admits.
pub fn createInRefused(dir: record.InodeValue) bool {
    return dir.isImmutable();
}

/// An entry removed from directory `dir` (unlink, rmdir, rename's source
/// name, a replaced destination): `may_delete` — an append-only OR an
/// immutable directory refuses.
pub fn removeFromRefused(dir: record.InodeValue) bool {
    return dir.isRestricted();
}

const testing = std.testing;

test "the four commands are the kernel's numbers (fs.h: 'f'/1 and 2 with a long, the 32-bit int forms, 'X'/31 and 32 with the 28 B fsxattr); anything else is not ours" {
    try testing.expectEqual(@as(u32, 0x80086601), FS_IOC_GETFLAGS);
    try testing.expectEqual(@as(u32, 0x40086602), FS_IOC_SETFLAGS);
    try testing.expectEqual(@as(u32, 0x80046601), FS_IOC32_GETFLAGS);
    try testing.expectEqual(@as(u32, 0x40046602), FS_IOC32_SETFLAGS);
    try testing.expectEqual(@as(u32, 0x801c581f), FS_IOC_FSGETXATTR);
    try testing.expectEqual(@as(u32, 0x401c5820), FS_IOC_FSSETXATTR);
    try testing.expectEqual(Command.get_flags, command(FS_IOC_GETFLAGS).?);
    try testing.expectEqual(Command.get_flags, command(FS_IOC32_GETFLAGS).?);
    try testing.expectEqual(Command.set_flags, command(FS_IOC_SETFLAGS).?);
    try testing.expectEqual(Command.get_xattr, command(FS_IOC_FSGETXATTR).?);
    try testing.expectEqual(Command.set_xattr, command(FS_IOC_FSSETXATTR).?);
    try testing.expectEqual(@as(?Command, null), command(linux.IOCTL.IOR('f', 11, u64)));
    try testing.expect(isSetCommand(FS_IOC_SETFLAGS) and isSetCommand(FS_IOC_FSSETXATTR) and !isSetCommand(FS_IOC_GETFLAGS) and !isSetCommand(0));
}

test "GET: the record's two bits as the FS_*_FL word (4 B LE) and as fsxattr (28 B, xflags alone); the reply is exactly out_size bytes or EINVAL; the STRIPE bit is never reported" {
    var buf: [64]u8 = undefined;
    const both = record.INODE_FLAG_IMMUTABLE | record.INODE_FLAG_APPEND | record.INODE_FLAG_STRIPE;
    try testing.expectEqual(@as(usize, 4), try encodeGet(.get_flags, both, 4, &buf));
    try testing.expectEqual(FS_IMMUTABLE_FL | FS_APPEND_FL, std.mem.readInt(u32, buf[0..4], .little));
    try testing.expectEqual(@as(usize, 4), try encodeGet(.get_flags, record.INODE_FLAG_APPEND, 4, &buf));
    try testing.expectEqual(FS_APPEND_FL, std.mem.readInt(u32, buf[0..4], .little));
    try testing.expectEqual(@as(usize, 4), try encodeGet(.get_flags, 0, 4, &buf));
    try testing.expectEqual(@as(u32, 0), std.mem.readInt(u32, buf[0..4], .little));
    try testing.expectEqual(@as(usize, 28), try encodeGet(.get_xattr, record.INODE_FLAG_IMMUTABLE, 28, &buf));
    const x = std.mem.bytesToValue(Fsxattr, buf[0..28]);
    try testing.expectEqual(FS_XFLAG_IMMUTABLE, x.fsx_xflags);
    try testing.expectEqual(@as(u32, 0), x.fsx_projid | x.fsx_extsize | x.fsx_nextents | x.fsx_cowextsize);
    try testing.expectError(error.InvalidArgument, encodeGet(.get_flags, 0, 8, &buf));
    try testing.expectError(error.InvalidArgument, encodeGet(.get_xattr, 0, 4, &buf));
    try testing.expectError(error.InvalidArgument, encodeGet(.set_flags, 0, 0, &buf));
}

test "SET: the attribute bits replaced, STRIPE kept; a bit outside IMMUTABLE/APPEND, a project id or an extent-size hint asked for is EOPNOTSUPP; a wrong argument length EINVAL; the word read back unchanged is a no-op" {
    var w: [4]u8 = undefined;
    std.mem.writeInt(u32, &w, FS_IMMUTABLE_FL, .little);
    try testing.expectEqual(record.INODE_FLAG_STRIPE | record.INODE_FLAG_IMMUTABLE, try resolveSet(.set_flags, record.INODE_FLAG_STRIPE, &w));
    std.mem.writeInt(u32, &w, FS_APPEND_FL, .little);
    try testing.expectEqual(record.INODE_FLAG_APPEND, try resolveSet(.set_flags, record.INODE_FLAG_IMMUTABLE, &w));
    std.mem.writeInt(u32, &w, 0, .little);
    try testing.expectEqual(@as(u32, 0), try resolveSet(.set_flags, record.INODE_FLAG_IMMUTABLE | record.INODE_FLAG_APPEND, &w));
    std.mem.writeInt(u32, &w, FS_IMMUTABLE_FL | 0x00000040, .little); // + FS_NODUMP_FL
    try testing.expectError(error.NotSupported, resolveSet(.set_flags, 0, &w));
    std.mem.writeInt(u32, &w, 0x00000080, .little); // FS_NOATIME_FL alone
    try testing.expectError(error.NotSupported, resolveSet(.set_flags, 0, &w));
    try testing.expectError(error.InvalidArgument, resolveSet(.set_flags, 0, w[0..2]));
    var x = Fsxattr.zero;
    x.fsx_xflags = FS_XFLAG_APPEND;
    try testing.expectEqual(record.INODE_FLAG_APPEND, try resolveSet(.set_xattr, 0, &std.mem.toBytes(x)));
    x.fsx_xflags = FS_XFLAG_IMMUTABLE | 0x00000800; // + FS_XFLAG_NODUMP
    try testing.expectError(error.NotSupported, resolveSet(.set_xattr, 0, &std.mem.toBytes(x)));
    x.fsx_xflags = 0;
    x.fsx_projid = 7;
    try testing.expectError(error.NotSupported, resolveSet(.set_xattr, 0, &std.mem.toBytes(x)));
    x.fsx_projid = 0;
    x.fsx_extsize = 4096;
    try testing.expectError(error.NotSupported, resolveSet(.set_xattr, 0, &std.mem.toBytes(x)));
    x.fsx_extsize = 0;
    x.fsx_nextents = 3; // get-only: ignored on a set, as the kernel does
    try testing.expectEqual(@as(u32, 0), try resolveSet(.set_xattr, record.INODE_FLAG_IMMUTABLE, &std.mem.toBytes(x)));
    try testing.expectError(error.InvalidArgument, resolveSet(.set_xattr, 0, &w));
    try testing.expectError(error.InvalidArgument, resolveSet(.get_flags, 0, &w));
}

test "the law: immutable refuses every write, write-intent open, setattr, fallocate; append-only admits only O_APPEND writes at EOF, NOW-times and a ctime echo, space preallocation; a directory's law by the kernel's may_create/may_delete" {
    const plain: record.InodeValue = .{ .mode = 0o100644 };
    const imm: record.InodeValue = .{ .mode = 0o100644, .flags = record.INODE_FLAG_IMMUTABLE };
    const app: record.InodeValue = .{ .mode = 0o100644, .flags = record.INODE_FLAG_APPEND };
    const o_append: u32 = @bitCast(linux.O{ .APPEND = true, .ACCMODE = .WRONLY });
    const o_wronly: u32 = @bitCast(linux.O{ .ACCMODE = .WRONLY });
    const o_rdonly: u32 = @bitCast(linux.O{ .ACCMODE = .RDONLY });
    const o_trunc_rd: u32 = @bitCast(linux.O{ .ACCMODE = .RDONLY, .TRUNC = true });
    // Writes.
    try testing.expect(!writeRefused(plain, o_wronly, 0, 100));
    try testing.expect(writeRefused(imm, o_append, 100, 100));
    try testing.expect(!writeRefused(app, o_append, 100, 100));
    try testing.expect(writeRefused(app, o_wronly, 100, 100));
    try testing.expect(writeRefused(app, o_append, 0, 100));
    try testing.expect(writeRefused(app, o_append, 101, 100));
    // Opens (served arm).
    try testing.expect(!openRefused(imm, o_rdonly) and !openRefused(app, o_rdonly));
    try testing.expect(openRefused(imm, o_wronly) and openRefused(imm, o_append) and openRefused(imm, o_trunc_rd));
    try testing.expect(openRefused(app, o_wronly) and !openRefused(app, o_append) and openRefused(app, o_append | o_trunc_rd));
    try testing.expect(!openRefused(plain, o_append | o_trunc_rd));
    // Setattr.
    try testing.expect(setattrRefused(imm, false, false, false));
    try testing.expect(!setattrRefused(app, false, false, false));
    try testing.expect(setattrRefused(app, true, false, false) and setattrRefused(app, false, true, false) and setattrRefused(app, false, false, true));
    try testing.expect(!setattrRefused(plain, true, true, true));
    // Fallocate.
    try testing.expect(fallocateRefused(imm, 0) and fallocateRefused(imm, 1));
    try testing.expect(!fallocateRefused(app, 0) and !fallocateRefused(app, 1) and fallocateRefused(app, 0x02 | 0x01) and fallocateRefused(app, 0x10));
    try testing.expect(!fallocateRefused(plain, 0x02 | 0x01));
    // Directories.
    const idir: record.InodeValue = .{ .mode = 0o040755, .flags = record.INODE_FLAG_IMMUTABLE };
    const adir: record.InodeValue = .{ .mode = 0o040755, .flags = record.INODE_FLAG_APPEND };
    const pdir: record.InodeValue = .{ .mode = 0o040755, .flags = record.INODE_FLAG_STRIPE };
    try testing.expect(createInRefused(idir) and !createInRefused(adir) and !createInRefused(pdir));
    try testing.expect(removeFromRefused(idir) and removeFromRefused(adir) and !removeFromRefused(pdir));
}
