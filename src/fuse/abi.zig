//! The FUSE kernel ABI, byte-exact (`include/uapi/linux/fuse.h` through 7.45
//! plus the sqz series' carve-outs; `docs/porting/fuse_abi_session.md` §1,
//! `fuse_uring_transport.md` §1.3, `fuse_kmbuf_zc.md` §1, §8.1; charter §12).
//!
//! Endianness: FUSE is a same-host protocol — the wire IS the kernel's native
//! struct layout, and every target of this product is little-endian (asserted
//! below), so native == little-endian and the structs are plain `extern
//! struct`s with integer fields exactly as the uapi declares them. Every
//! struct carries `comptime` size and offset assertions against the spec's
//! `type@offset` tables; a drift in either direction fails the build.
//!
//! Decoding is total and canonical: `decodeExact` admits exactly `@sizeOf(T)`
//! bytes, `decodePrefix` the struct as the prefix of a longer body (the
//! classical `op_in ‖ names/payload` form), both by `@memcpy` so an unaligned
//! read buffer never traps. Nothing here allocates.
//!
//! Field names are the uapi's (decision 0002 row — fuse_abi_session.md Q14):
//! a padding word the kernel later promoted to a capability (`total_extlen`,
//! `open_flags`, `backing_id`, `flags`) keeps its uapi name so a negotiation
//! that someday reads it is a one-line change, not a silently ignored byte.

const std = @import("std");
const builtin = @import("builtin");

comptime {
    // Native == little-endian is the one assumption every `extern struct`
    // below rests on (x86_64 and aarch64 are the product's targets; the
    // macOS dev box runs the portable tests on aarch64).
    std.debug.assert(builtin.cpu.arch.endian() == .little);
}

// ---------------------------------------------------------------------------
// Protocol version and fixed constants
// ---------------------------------------------------------------------------

pub const FUSE_KERNEL_VERSION: u32 = 7;
/// Exactly 36: the extended-init revision. The 31→36 audit found no
/// `fc->minor`-gated kernel behaviour in (23, 36]; raising it needs a fresh
/// audit of every `fc->minor` gate (fuse_abi_session.md §1.1).
pub const FUSE_KERNEL_MINOR_VERSION: u32 = 36;
/// The kernel's minimum read buffer (`FUSE_MIN_READ_BUFFER`): also the floor
/// of the ring's `max_payload_sz` mirror (geometry).
pub const FUSE_MIN_READ_BUFFER: u32 = 8192;
pub const FUSE_ROOT_ID: u64 = 1;
/// `fuse_init_out.time_gran` default: nanosecond timestamps.
pub const DEFAULT_TIME_GRAN: u32 = 1;
/// `fuse_init_out.map_alignment`: 0 is correct iff `FUSE_MAP_ALIGNMENT` is
/// never advertised, which it is not.
pub const DEFAULT_MAP_ALIGNMENT: u16 = 0;
/// Kernel request ids step by 2; bit 0 marks an INTERRUPT's reply id.
pub const FUSE_REQ_ID_STEP: u64 = 2;
pub const FUSE_INT_REQ_BIT: u64 = 1;
/// The kernel refuses a dirent name longer than this (`FUSE_NAME_MAX`).
pub const FUSE_NAME_MAX: usize = 1024;
/// `FUSE_URING_IN_OUT_HEADER_SZ` / `FUSE_URING_OP_IN_OUT_SZ`.
pub const FUSE_URING_IN_OUT_HEADER_SZ: usize = 128;
pub const FUSE_URING_OP_IN_OUT_SZ: usize = 128;

// ---------------------------------------------------------------------------
// INIT capability bits. The classical word is `fuse_init_in.flags` (bits
// 0..31); `flags2` carries bits 32..63 folded as `flags | flags2 << 32`
// (`KernelInit::new`, fuse_abi_session.md §1.3). Every constant is the
// folded u64 so one `has(word, BIT)` reads either half.
// ---------------------------------------------------------------------------

pub const FUSE_ASYNC_READ: u64 = 1 << 0;
/// Never echoed: POSIX advisory locks stay kernel-local (charter §4).
pub const FUSE_POSIX_LOCKS: u64 = 1 << 1;
pub const FUSE_FILE_OPS: u64 = 1 << 2;
pub const FUSE_ATOMIC_O_TRUNC: u64 = 1 << 3;
pub const FUSE_EXPORT_SUPPORT: u64 = 1 << 4;
pub const FUSE_BIG_WRITES: u64 = 1 << 5;
pub const FUSE_DONT_MASK: u64 = 1 << 6;
/// Never advertised (classical splice is not a path this product has).
pub const FUSE_SPLICE_WRITE: u64 = 1 << 7;
pub const FUSE_SPLICE_MOVE: u64 = 1 << 8;
pub const FUSE_SPLICE_READ: u64 = 1 << 9;
/// Never echoed (charter §4).
pub const FUSE_FLOCK_LOCKS: u64 = 1 << 10;
pub const FUSE_HAS_IOCTL_DIR: u64 = 1 << 11;
pub const FUSE_AUTO_INVAL_DATA: u64 = 1 << 12;
pub const FUSE_DO_READDIRPLUS: u64 = 1 << 13;
pub const FUSE_READDIRPLUS_AUTO: u64 = 1 << 14;
pub const FUSE_ASYNC_DIO: u64 = 1 << 15;
pub const FUSE_WRITEBACK_CACHE: u64 = 1 << 16;
pub const FUSE_NO_OPEN_SUPPORT: u64 = 1 << 17;
pub const FUSE_PARALLEL_DIROPS: u64 = 1 << 18;
pub const FUSE_HANDLE_KILLPRIV: u64 = 1 << 19;
/// Never echoed: every create would probe default ACLs — a blanket
/// EOPNOTSUPP regression (fuse_abi_session.md §2.5).
pub const FUSE_POSIX_ACL: u64 = 1 << 20;
pub const FUSE_ABORT_ERROR: u64 = 1 << 21;
pub const FUSE_MAX_PAGES: u64 = 1 << 22;
pub const FUSE_CACHE_SYMLINKS: u64 = 1 << 23;
pub const FUSE_NO_OPENDIR_SUPPORT: u64 = 1 << 24;
pub const FUSE_EXPLICIT_INVAL_DATA: u64 = 1 << 25;
pub const FUSE_MAP_ALIGNMENT: u64 = 1 << 26;
/// uapi 7.33: the 16-byte `fuse_setxattr_in`; never negotiated.
pub const FUSE_SETXATTR_EXT: u64 = 1 << 27;
/// Linux ≥ 5.11 (uapi 7.33): the daemon owns suid/sgid/caps clearing.
/// Measured motive: without it the kernel sends `GETXATTR("security.
/// capability")` once per `write(2)` — half of a write-syscall-bound
/// stream's requests (fuse_abi_session.md §13).
pub const FUSE_HANDLE_KILLPRIV_V2: u64 = 1 << 28;
/// uapi 7.36: `flags2` is valid in `fuse_init_in` and applied from
/// `fuse_init_out`. Without it the kernel DISCARDS our `flags2`.
pub const FUSE_INIT_EXT: u64 = 1 << 30;
pub const FUSE_INIT_RESERVED: u64 = 1 << 31;
// flags2 (bits 32..63)
pub const FUSE_SECURITY_CTX: u64 = 1 << 32;
pub const FUSE_HAS_INODE_DAX: u64 = 1 << 33;
pub const FUSE_CREATE_SUPP_GROUP: u64 = 1 << 34;
/// uapi 7.38: `fuse_notify_inval_entry_out.flags` may carry `FUSE_EXPIRE_ONLY`.
pub const FUSE_HAS_EXPIRE_ONLY: u64 = 1 << 35;
pub const FUSE_DIRECT_IO_ALLOW_MMAP: u64 = 1 << 36;
pub const FUSE_PASSTHROUGH: u64 = 1 << 37;
pub const FUSE_NO_EXPORT_SUPPORT: u64 = 1 << 38;
pub const FUSE_HAS_RESEND: u64 = 1 << 39;
pub const FUSE_ALLOW_IDMAP: u64 = 1 << 40;
/// Linux ≥ 6.14 (flags2 bit 9): the ring transport. ALWAYS in our reply —
/// "no opt-out" (charter §4).
pub const FUSE_OVER_IO_URING: u64 = 1 << 41;
pub const FUSE_REQUEST_TIMEOUT: u64 = 1 << 42;
/// sqz patch 0027 (flags2 bit 30): the daemon advertises its i64-ns
/// timestamp range in `fuse_init_out.time_min/time_max` so the VFS clamps
/// where the daemon does. The bit IS the probe (fuse_kmbuf_zc.md §2.4).
pub const FUSE_TIME_LIMITS: u64 = 1 << 62;

/// The flags2 half of a folded capability word.
pub inline fn flags2Of(folded: u64) u32 {
    return @truncate(folded >> 32);
}
/// The classical half.
pub inline fn flagsOf(folded: u64) u32 {
    return @truncate(folded);
}
pub inline fn fold(flags: u32, flags2: u32) u64 {
    return @as(u64, flags) | (@as(u64, flags2) << 32);
}

/// `FUSE_TIME_LIMITS` range the daemon advertises: the whole-second interior
/// of the i64-nanosecond word, conservative by one second on the floor
/// (exact floor −9_223_372_037 s + 145_224_192 ns) so every kernel-clamped
/// `(sec, nsec = 0)` round-trips exactly (fuse_kmbuf_zc.md §8.1).
pub const TIME_LIMITS_MIN_SEC: i64 = -9_223_372_036;
pub const TIME_LIMITS_MAX_SEC: i64 = 9_223_372_036;

// ---------------------------------------------------------------------------
// Per-op flag bits
// ---------------------------------------------------------------------------

/// `fuse_setattr_in.valid`.
pub const FATTR_MODE: u32 = 1 << 0;
pub const FATTR_UID: u32 = 1 << 1;
pub const FATTR_GID: u32 = 1 << 2;
pub const FATTR_SIZE: u32 = 1 << 3;
pub const FATTR_ATIME: u32 = 1 << 4;
pub const FATTR_MTIME: u32 = 1 << 5;
pub const FATTR_FH: u32 = 1 << 6;
pub const FATTR_ATIME_NOW: u32 = 1 << 7;
pub const FATTR_MTIME_NOW: u32 = 1 << 8;
pub const FATTR_LOCKOWNER: u32 = 1 << 9;
pub const FATTR_CTIME: u32 = 1 << 10;
/// uapi 7.33 (KILLPRIV_V2): clear suid/sgid as part of this SETATTR.
pub const FATTR_KILL_SUIDGID: u32 = 1 << 11;

/// `fuse_open_out.open_flags`.
pub const FOPEN_DIRECT_IO: u32 = 1 << 0;
pub const FOPEN_KEEP_CACHE: u32 = 1 << 1;
pub const FOPEN_NONSEEKABLE: u32 = 1 << 2;
pub const FOPEN_CACHE_DIR: u32 = 1 << 3;
pub const FOPEN_STREAM: u32 = 1 << 4;
/// uapi 7.35 / Linux ≥ 5.16: no FLUSH on this handle.
pub const FOPEN_NOFLUSH: u32 = 1 << 5;
/// uapi 7.38 / Linux ≥ 6.2.
pub const FOPEN_PARALLEL_DIRECT_WRITES: u32 = 1 << 6;
pub const FOPEN_PASSTHROUGH: u32 = 1 << 7;

/// `fuse_release_in.release_flags`.
pub const FUSE_RELEASE_FLUSH: u32 = 1 << 0;
pub const FUSE_RELEASE_FLOCK_UNLOCK: u32 = 1 << 1;
/// `fuse_getattr_in.getattr_flags`.
pub const FUSE_GETATTR_FH: u32 = 1 << 0;
/// `fuse_lk_in.lk_flags`.
pub const FUSE_LK_FLOCK: u32 = 1 << 0;
/// `fuse_write_in.write_flags`.
pub const FUSE_WRITE_CACHE: u32 = 1 << 0;
pub const FUSE_WRITE_LOCKOWNER: u32 = 1 << 1;
/// uapi 7.33 (KILLPRIV_V2).
pub const FUSE_WRITE_KILL_SUIDGID: u32 = 1 << 2;
/// `fuse_read_in.read_flags`.
pub const FUSE_READ_LOCKOWNER: u32 = 1 << 1;
/// `fuse_open_in.open_flags` (uapi 7.33, KILLPRIV_V2): an O_TRUNC open
/// that must clear suid/sgid.
pub const FUSE_OPEN_KILL_SUIDGID: u32 = 1 << 0;
/// `fuse_setxattr_in.setxattr_flags` in the 16-byte (`FUSE_SETXATTR_EXT`)
/// form, never negotiated; named so the ledger can say why.
pub const FUSE_SETXATTR_ACL_KILL_SGID: u32 = 1 << 0;
/// `fuse_fsync_in.fsync_flags` bit 0: datasync.
pub const FUSE_FSYNC_FDATASYNC: u32 = 1 << 0;
/// `fuse_ioctl_in.flags`.
pub const FUSE_IOCTL_COMPAT: u32 = 1 << 0;
pub const FUSE_IOCTL_UNRESTRICTED: u32 = 1 << 1;
pub const FUSE_IOCTL_RETRY: u32 = 1 << 2;
pub const FUSE_IOCTL_32BIT: u32 = 1 << 3;
pub const FUSE_IOCTL_DIR: u32 = 1 << 4;
pub const FUSE_IOCTL_MAX_IOV: u32 = 256;
/// `fuse_poll_in.flags`.
pub const FUSE_POLL_SCHEDULE_NOTIFY: u32 = 1 << 0;
/// `fuse_attr.flags` (uapi 7.32/7.33): always 0 from this daemon.
pub const FUSE_ATTR_SUBMOUNT: u32 = 1 << 0;
pub const FUSE_ATTR_DAX: u32 = 1 << 1;
/// `fuse_notify_inval_entry_out.flags` (uapi 7.38): expire, do not invalidate.
pub const FUSE_EXPIRE_ONLY: u32 = 1 << 0;

// ---------------------------------------------------------------------------
// Opcodes (through uapi 7.45, so a 7.45 kernel's request logs by name; the
// reply for anything the daemon does not serve is a header-only -ENOSYS and
// the kernel latches `fc->no_<op>`). Non-exhaustive: an unknown opcode
// decodes without error and `isKnown()` says so.
// ---------------------------------------------------------------------------

pub const Opcode = enum(u32) {
    lookup = 1,
    forget = 2,
    getattr = 3,
    setattr = 4,
    readlink = 5,
    symlink = 6,
    mknod = 8,
    mkdir = 9,
    unlink = 10,
    rmdir = 11,
    rename = 12,
    link = 13,
    open = 14,
    read = 15,
    write = 16,
    statfs = 17,
    release = 18,
    fsync = 20,
    setxattr = 21,
    getxattr = 22,
    listxattr = 23,
    removexattr = 24,
    flush = 25,
    init = 26,
    opendir = 27,
    readdir = 28,
    releasedir = 29,
    fsyncdir = 30,
    getlk = 31,
    setlk = 32,
    setlkw = 33,
    access = 34,
    create = 35,
    interrupt = 36,
    bmap = 37,
    destroy = 38,
    ioctl = 39,
    poll = 40,
    notify_reply = 41,
    batch_forget = 42,
    fallocate = 43,
    readdirplus = 44,
    rename2 = 45,
    lseek = 46,
    copy_file_range = 47,
    setupmapping = 48,
    removemapping = 49,
    syncfs = 50,
    tmpfile = 51,
    statx = 52,
    _,

    pub fn fromInt(v: u32) Opcode {
        return @fromBackingInt(@intCast(v));
    }

    pub fn isKnown(self: Opcode) bool {
        return switch (self) {
            _ => false,
            else => true,
        };
    }

    /// The uapi name for a log line; "unknown" past 7.45.
    pub fn name(self: Opcode) []const u8 {
        return switch (self) {
            _ => "unknown",
            inline else => |t| @tagName(t),
        };
    }
};

/// Notify codes ride in `fuse_out_header.error` (positive) with `unique = 0`.
pub const NotifyCode = enum(u32) {
    poll = 1,
    inval_inode = 2,
    inval_entry = 3,
    store = 4,
    retrieve = 5,
    delete = 6,
    resend = 7,
    inc_epoch = 8,
    /// uapi 7.45: the ONE way a `FUSE_WRITEBACK_CACHE` kernel adopts a peer
    /// mount's size/mtime/ctime.
    prune = 9,
};

// ---------------------------------------------------------------------------
// Headers and attribute structs (§1.2)
// ---------------------------------------------------------------------------

pub const fuse_in_header = extern struct {
    len: u32,
    opcode: u32,
    unique: u64,
    nodeid: u64,
    uid: u32,
    gid: u32,
    pid: u32,
    /// uapi ≥ 7.40 splits the old padding: 0 unless `FUSE_SECURITY_CTX` /
    /// `FUSE_CREATE_SUPP_GROUP` is negotiated (neither is).
    total_extlen: u16,
    padding: u16,

    pub fn op(self: fuse_in_header) Opcode {
        return Opcode.fromInt(self.opcode);
    }
};

pub const fuse_out_header = extern struct {
    len: u32,
    /// `-errno` on an error reply, 0 on success, the notify code (positive)
    /// on a notification. The kernel refuses `error > 0` on a request reply.
    @"error": i32,
    unique: u64,
};

pub const fuse_attr = extern struct {
    ino: u64,
    size: u64,
    blocks: u64,
    /// Seconds as the kernel's `time_t` (i64) bit pattern.
    atime: u64,
    mtime: u64,
    ctime: u64,
    atimensec: u32,
    mtimensec: u32,
    ctimensec: u32,
    mode: u32,
    nlink: u32,
    uid: u32,
    gid: u32,
    rdev: u32,
    blksize: u32,
    /// `FUSE_ATTR_SUBMOUNT` / `FUSE_ATTR_DAX`: always 0.
    flags: u32,
};

pub const fuse_kstatfs = extern struct {
    blocks: u64,
    bfree: u64,
    bavail: u64,
    files: u64,
    ffree: u64,
    bsize: u32,
    namelen: u32,
    frsize: u32,
    padding: u32,
    spare: [6]u32,
};

pub const fuse_entry_out = extern struct {
    /// 0 = a negative entry ("same as -ENOENT, but with a valid timeout").
    nodeid: u64,
    generation: u64,
    entry_valid: u64,
    attr_valid: u64,
    entry_valid_nsec: u32,
    attr_valid_nsec: u32,
    attr: fuse_attr,
};

pub const fuse_attr_out = extern struct {
    attr_valid: u64,
    attr_valid_nsec: u32,
    dummy: u32,
    attr: fuse_attr,
};

pub const fuse_statfs_out = extern struct {
    st: fuse_kstatfs,
};

pub const fuse_file_lock = extern struct {
    start: u64,
    end: u64,
    type: u32,
    pid: u32,
};

// ---------------------------------------------------------------------------
// INIT (§1.3)
// ---------------------------------------------------------------------------

/// 16 bytes classical, 64 extended (≥ 7.36): `flags2` is valid iff `flags &
/// FUSE_INIT_EXT` and the body carried ≥ 20 bytes — the codec zero-fills the
/// rest so a 16-byte INIT decodes into this one struct.
pub const fuse_init_in = extern struct {
    major: u32,
    minor: u32,
    max_readahead: u32,
    flags: u32,
    flags2: u32,
    unused: [11]u32,

    pub const CLASSICAL_LEN: usize = 16;
    /// Bytes needed for `flags2` to be present.
    pub const FLAGS2_END: usize = 20;

    pub fn folded(self: fuse_init_in) u64 {
        return fold(self.flags, self.flags2);
    }
};

/// 64 bytes. `time_min/time_max` are the sqz carve-out of upstream's
/// `unused[11]` → `unused[3] u16 ‖ i64 ‖ i64`, naturally aligned at 48/56.
pub const fuse_init_out = extern struct {
    major: u32,
    minor: u32,
    max_readahead: u32,
    flags: u32,
    max_background: u16,
    congestion_threshold: u16,
    max_write: u32,
    time_gran: u32,
    max_pages: u16,
    map_alignment: u16,
    flags2: u32,
    max_stack_depth: u32,
    request_timeout: u16,
    unused: [3]u16,
    time_min: i64,
    time_max: i64,

    pub fn folded(self: fuse_init_out) u64 {
        return fold(self.flags, self.flags2);
    }
};

// ---------------------------------------------------------------------------
// Per-op request structs (§1.4)
// ---------------------------------------------------------------------------

pub const fuse_forget_in = extern struct { nlookup: u64 };
pub const fuse_forget_one = extern struct { nodeid: u64, nlookup: u64 };
/// Followed by `count × fuse_forget_one`.
pub const fuse_batch_forget_in = extern struct { count: u32, dummy: u32 };
pub const fuse_getattr_in = extern struct { getattr_flags: u32, dummy: u32, fh: u64 };
/// Name follows.
pub const fuse_mknod_in = extern struct { mode: u32, rdev: u32, umask: u32, padding: u32 };
/// Name follows.
pub const fuse_mkdir_in = extern struct { mode: u32, umask: u32 };
/// `oldname\0newname\0` follow.
pub const fuse_rename_in = extern struct { newdir: u64 };
pub const fuse_rename2_in = extern struct { newdir: u64, flags: u32, padding: u32 };
/// New name follows.
pub const fuse_link_in = extern struct { oldnodeid: u64 };
pub const fuse_setattr_in = extern struct {
    valid: u32,
    padding: u32,
    fh: u64,
    size: u64,
    lock_owner: u64,
    atime: u64,
    mtime: u64,
    ctime: u64,
    atimensec: u32,
    mtimensec: u32,
    ctimensec: u32,
    mode: u32,
    unused4: u32,
    uid: u32,
    gid: u32,
    unused5: u32,
};
/// `open_flags` (uapi ≥ 7.33) carries `FUSE_OPEN_KILL_SUIDGID`.
pub const fuse_open_in = extern struct { flags: u32, open_flags: u32 };
/// Name follows. `open_flags` (uapi ≥ 7.33) carries `FUSE_OPEN_KILL_SUIDGID`.
pub const fuse_create_in = extern struct { flags: u32, mode: u32, umask: u32, open_flags: u32 };
pub const fuse_release_in = extern struct { fh: u64, flags: u32, release_flags: u32, lock_owner: u64 };
pub const fuse_flush_in = extern struct { fh: u64, unused: u32, padding: u32, lock_owner: u64 };
/// `flags` = the file's open flags (the O_DIRECT class).
pub const fuse_read_in = extern struct { fh: u64, offset: u64, size: u32, read_flags: u32, lock_owner: u64, flags: u32, padding: u32 };
/// Payload (`size` bytes) follows.
pub const fuse_write_in = extern struct { fh: u64, offset: u64, size: u32, write_flags: u32, lock_owner: u64, flags: u32, padding: u32 };
pub const fuse_fsync_in = extern struct { fh: u64, fsync_flags: u32, padding: u32 };
/// The 8-byte form (`FUSE_SETXATTR_EXT` never negotiated); `name\0value` follow.
pub const fuse_setxattr_in = extern struct { size: u32, flags: u32 };
/// Name follows for GETXATTR, nothing for LISTXATTR.
pub const fuse_getxattr_in = extern struct { size: u32, padding: u32 };
pub const fuse_lk_in = extern struct { fh: u64, owner: u64, lk: fuse_file_lock, lk_flags: u32, padding: u32 };
pub const fuse_access_in = extern struct { mask: u32, padding: u32 };
/// `unique` of the interrupted request.
pub const fuse_interrupt_in = extern struct { unique: u64 };
pub const fuse_bmap_in = extern struct { block: u64, blocksize: u32, padding: u32 };
pub const fuse_ioctl_in = extern struct { fh: u64, flags: u32, cmd: u32, arg: u64, in_size: u32, out_size: u32 };
pub const fuse_poll_in = extern struct { fh: u64, kh: u64, flags: u32, events: u32 };
pub const fuse_fallocate_in = extern struct { fh: u64, offset: u64, length: u64, mode: u32, padding: u32 };
/// "matches the size of fuse_write_in"; `size` bytes of data follow.
pub const fuse_notify_retrieve_in = extern struct { dummy1: u64, offset: u64, size: u32, dummy2: u32, dummy3: u64, dummy4: u64 };
pub const fuse_lseek_in = extern struct { fh: u64, offset: u64, whence: u32, padding: u32 };
pub const fuse_copy_file_range_in = extern struct { fh_in: u64, off_in: u64, nodeid_out: u64, fh_out: u64, off_out: u64, len: u64, flags: u64 };

// ---------------------------------------------------------------------------
// Per-op reply structs (§1.5)
// ---------------------------------------------------------------------------

/// `backing_id` (uapi ≥ 7.40, PASSTHROUGH): always 0.
pub const fuse_open_out = extern struct { fh: u64, open_flags: u32, backing_id: i32 };
pub const fuse_write_out = extern struct { size: u32, padding: u32 };
pub const fuse_getxattr_out = extern struct { size: u32, padding: u32 };
pub const fuse_lk_out = extern struct { lk: fuse_file_lock };
pub const fuse_bmap_out = extern struct { block: u64 };
pub const fuse_ioctl_out = extern struct { result: i32, flags: u32, in_iovs: u32, out_iovs: u32 };
pub const fuse_ioctl_iovec = extern struct { base: u64, len: u64 };
pub const fuse_poll_out = extern struct { revents: u32, padding: u32 };
pub const fuse_lseek_out = extern struct { offset: u64 };

/// 24-byte header; `name[namelen]` follows and the record is zero-padded to
/// `(24 + namelen + 7) & ~7`. `type = mode >> 12`; `off` = the cookie of
/// the NEXT entry (what the kernel hands back as `fuse_read_in.offset`).
pub const fuse_dirent = extern struct {
    ino: u64,
    off: u64,
    namelen: u32,
    type: u32,

    pub const NAME_OFFSET: usize = @sizeOf(fuse_dirent);

    /// `(24 + namelen + 7) & ~7` — the 8-byte alignment law (helper.rs:50–55).
    pub fn recordLen(namelen: usize) usize {
        return (NAME_OFFSET + namelen + 7) & ~@as(usize, 7);
    }
};

/// `fuse_entry_out ‖ fuse_dirent` = 152, then the name, padded to 8.
pub const fuse_direntplus = extern struct {
    entry_out: fuse_entry_out,
    dirent: fuse_dirent,

    pub const NAME_OFFSET: usize = @sizeOf(fuse_direntplus);

    pub fn recordLen(namelen: usize) usize {
        return (NAME_OFFSET + namelen + 7) & ~@as(usize, 7);
    }
};

// ---------------------------------------------------------------------------
// Notify out structs (§1.6): frame = `fuse_out_header{len, error = code,
// unique = 0} ‖ struct ‖ tail`, `len` covering the tail.
// ---------------------------------------------------------------------------

pub const fuse_notify_poll_wakeup_out = extern struct { kh: u64 };
/// `off < 0` = attrs only; `(0, 0)` = all pages to EOF.
pub const fuse_notify_inval_inode_out = extern struct { ino: u64, off: i64, len: i64 };
/// `name\0` follows; `flags` (uapi ≥ 7.38) may carry `FUSE_EXPIRE_ONLY`.
pub const fuse_notify_inval_entry_out = extern struct { parent: u64, namelen: u32, flags: u32 };
/// `size` bytes of data follow.
pub const fuse_notify_store_out = extern struct { nodeid: u64, offset: u64, size: u32, padding: u32 };
pub const fuse_notify_retrieve_out = extern struct { notify_unique: u64, nodeid: u64, offset: u64, size: u32, padding: u32 };
/// `name\0` follows.
pub const fuse_notify_delete_out = extern struct { parent: u64, child: u64, namelen: u32, padding: u32 };
/// uapi 7.45; `count × u64` nodeids follow and `oh.len` MUST cover them
/// (`fuse_dev_do_write` refuses `oh.len != nbytes`).
pub const fuse_notify_prune_out = extern struct { count: u32, padding: u32, spare: u64 };

// ---------------------------------------------------------------------------
// FUSE-over-io_uring (§1.9; fuse_uring_transport.md §1.3; fuse_kmbuf_zc.md §1)
// ---------------------------------------------------------------------------

pub const FUSE_IO_URING_CMD_REGISTER: u32 = 1;
pub const FUSE_IO_URING_CMD_COMMIT_AND_FETCH: u32 = 2;
/// sqz patch 0029 ("uapi 7.47"): ends a retained slot; doubles as the
/// retention PROBE opcode (RELEASE with an impossible commit id: -ENOTCONN /
/// -ENOENT ⇒ present, anything else ⇒ absent).
pub const FUSE_IO_URING_CMD_RELEASE_PAYLOAD: u32 = 3;

/// `fuse_uring_cmd_req.init.flags` (REGISTER).
pub const FUSE_URING_BUF_RING: u16 = 1 << 0;
/// Requires `BUF_RING` and CAP_SYS_ADMIN.
pub const FUSE_URING_ZERO_COPY: u16 = 1 << 1;
/// Valid ONLY with `ZERO_COPY` — the kernel answers EINVAL otherwise.
pub const FUSE_URING_PAYLOAD_RETENTION: u16 = 1 << 2;
/// `fuse_uring_cmd_req.commit.flags` (COMMIT_AND_FETCH).
pub const FUSE_URING_COMMIT_RETAIN: u16 = 1 << 0;
/// The buffer-group id FUSE hardcodes for the kernel-managed ring.
pub const FUSE_URING_RINGBUF_GROUP: u16 = 0;
/// Fixed-table index of the header buffers (bufring); `+ queue_depth` under zc.
pub const FUSE_URING_FIXED_HEADERS_OFFSET: u16 = 0;

/// The kmbuf register/unregister opcode PAIR differs per kernel track and is
/// resolved by the probe ladder, never by version (charter §3).
pub const KmbufRegisterOpcodes = struct {
    register: u32,
    unregister: u32,
};
/// 6.19-sqz.
pub const KMBUF_OPCODES_6_19: KmbufRegisterOpcodes = .{ .register = 37, .unregister = 38 };
/// 7.1/7.2-sqz: upstream 7.1 took 37 for `IORING_REGISTER_BPF_FILTER`.
pub const KMBUF_OPCODES_7_1: KmbufRegisterOpcodes = .{ .register = 38, .unregister = 39 };
/// mmap offset = `IORING_OFF_KMBUF_RING | bgid << IORING_OFF_KMBUF_SHIFT`.
pub const IORING_OFF_KMBUF_RING: u64 = 0x8800_0000;
pub const IORING_OFF_KMBUF_SHIFT: u6 = 16;
pub const IORING_CQE_F_BUFFER: u32 = 1 << 0;
pub const IORING_CQE_BUFFER_SHIFT: u5 = 16;

/// SQE128 placement of the FUSE uring command (fuse_uring_transport.md §1.5).
pub const SQE128_LEN: usize = 128;
/// The 80-byte command area starts at SQE offset 48.
pub const SQE128_CMD_OFFSET: usize = 48;
pub const SQE128_CMD_LEN: usize = 80;
/// `buf_index` u16 (kmbuf fixed-buffer id).
pub const SQE_BUF_INDEX_OFFSET: usize = 40;
/// User-ent REGISTER: `addr` u64 = iovec array pointer, `len` u32 = 2.
pub const SQE_ADDR_OFFSET: usize = 16;
pub const SQE_LEN_OFFSET: usize = 24;
pub const REGISTER_IOVEC_COUNT: u32 = 2;

pub const fuse_uring_ent_in_out = extern struct {
    flags: u64,
    /// The request's `unique` on delivery; echoed on COMMIT_AND_FETCH.
    commit_id: u64,
    /// Bytes the kernel placed in the payload (delivery) / bytes we placed
    /// (commit). Pre-set to the ent's payload length at REGISTER.
    payload_sz: u32,
    padding: u32,
    reserved: u64,
};

/// 288 bytes: `in_out` holds `fuse_in_header` on delivery and
/// `fuse_out_header` on commit; `op_in` holds `in_args[0]` (the per-op
/// struct) on delivery and the out-struct on commit.
pub const fuse_uring_req_header = extern struct {
    in_out: [FUSE_URING_IN_OUT_HEADER_SZ]u8,
    op_in: [FUSE_URING_OP_IN_OUT_SZ]u8,
    ring_ent_in_out: fuse_uring_ent_in_out,
};

/// 24 bytes in the 80-byte SQE128 command area. `init_flags` at @18 is the
/// uapi union: `init.flags` on REGISTER ≡ `commit.flags` on
/// COMMIT_AND_FETCH; `queue_depth` (@20) is read only under zero-copy. The
/// sqz series repurposes 4 of upstream's 6 padding bytes ("uapi 7.46").
pub const fuse_uring_cmd_req = extern struct {
    flags: u64,
    commit_id: u64,
    qid: u16,
    init_flags: u16,
    queue_depth: u16,
    padding: [2]u8,

    /// The union member's other name.
    pub fn commitFlags(self: fuse_uring_cmd_req) u16 {
        return self.init_flags;
    }
};

/// The series' `io_uring_buf_reg` (40 B): `union { ring_addr u64; buf_size
/// u32 }` at 0 — kmbuf uses `buf_size` (page-aligned, non-zero).
pub const io_uring_buf_reg = extern struct {
    ring_addr: u64,
    ring_entries: u32,
    bgid: u16,
    flags: u16,
    resv: [3]u64,

    pub fn bufSize(self: io_uring_buf_reg) u32 {
        return @truncate(self.ring_addr);
    }
};

// ---------------------------------------------------------------------------
// Size and offset assertions: the spec's `type@offset` tables, verbatim.
// ---------------------------------------------------------------------------

fn assertLayout(comptime T: type, comptime size: usize, comptime offsets: anytype) void {
    std.debug.assert(@sizeOf(T) == size);
    inline for (offsets) |o| std.debug.assert(@offsetOf(T, o[0]) == o[1]);
}

comptime {
    assertLayout(fuse_in_header, 40, .{ .{ "len", 0 }, .{ "opcode", 4 }, .{ "unique", 8 }, .{ "nodeid", 16 }, .{ "uid", 24 }, .{ "gid", 28 }, .{ "pid", 32 }, .{ "total_extlen", 36 }, .{ "padding", 38 } });
    assertLayout(fuse_out_header, 16, .{ .{ "len", 0 }, .{ "error", 4 }, .{ "unique", 8 } });
    assertLayout(fuse_attr, 88, .{ .{ "ino", 0 }, .{ "size", 8 }, .{ "blocks", 16 }, .{ "atime", 24 }, .{ "mtime", 32 }, .{ "ctime", 40 }, .{ "atimensec", 48 }, .{ "mtimensec", 52 }, .{ "ctimensec", 56 }, .{ "mode", 60 }, .{ "nlink", 64 }, .{ "uid", 68 }, .{ "gid", 72 }, .{ "rdev", 76 }, .{ "blksize", 80 }, .{ "flags", 84 } });
    assertLayout(fuse_kstatfs, 80, .{ .{ "blocks", 0 }, .{ "bfree", 8 }, .{ "bavail", 16 }, .{ "files", 24 }, .{ "ffree", 32 }, .{ "bsize", 40 }, .{ "namelen", 44 }, .{ "frsize", 48 }, .{ "padding", 52 }, .{ "spare", 56 } });
    assertLayout(fuse_entry_out, 128, .{ .{ "nodeid", 0 }, .{ "generation", 8 }, .{ "entry_valid", 16 }, .{ "attr_valid", 24 }, .{ "entry_valid_nsec", 32 }, .{ "attr_valid_nsec", 36 }, .{ "attr", 40 } });
    assertLayout(fuse_attr_out, 104, .{ .{ "attr_valid", 0 }, .{ "attr_valid_nsec", 8 }, .{ "dummy", 12 }, .{ "attr", 16 } });
    assertLayout(fuse_statfs_out, 80, .{.{ "st", 0 }});
    assertLayout(fuse_file_lock, 24, .{ .{ "start", 0 }, .{ "end", 8 }, .{ "type", 16 }, .{ "pid", 20 } });
    assertLayout(fuse_init_in, 64, .{ .{ "major", 0 }, .{ "minor", 4 }, .{ "max_readahead", 8 }, .{ "flags", 12 }, .{ "flags2", 16 }, .{ "unused", 20 } });
    assertLayout(fuse_init_out, 64, .{ .{ "major", 0 }, .{ "minor", 4 }, .{ "max_readahead", 8 }, .{ "flags", 12 }, .{ "max_background", 16 }, .{ "congestion_threshold", 18 }, .{ "max_write", 20 }, .{ "time_gran", 24 }, .{ "max_pages", 28 }, .{ "map_alignment", 30 }, .{ "flags2", 32 }, .{ "max_stack_depth", 36 }, .{ "request_timeout", 40 }, .{ "unused", 42 }, .{ "time_min", 48 }, .{ "time_max", 56 } });

    assertLayout(fuse_forget_in, 8, .{.{ "nlookup", 0 }});
    assertLayout(fuse_forget_one, 16, .{ .{ "nodeid", 0 }, .{ "nlookup", 8 } });
    assertLayout(fuse_batch_forget_in, 8, .{ .{ "count", 0 }, .{ "dummy", 4 } });
    assertLayout(fuse_getattr_in, 16, .{ .{ "getattr_flags", 0 }, .{ "dummy", 4 }, .{ "fh", 8 } });
    assertLayout(fuse_mknod_in, 16, .{ .{ "mode", 0 }, .{ "rdev", 4 }, .{ "umask", 8 }, .{ "padding", 12 } });
    assertLayout(fuse_mkdir_in, 8, .{ .{ "mode", 0 }, .{ "umask", 4 } });
    assertLayout(fuse_rename_in, 8, .{.{ "newdir", 0 }});
    assertLayout(fuse_rename2_in, 16, .{ .{ "newdir", 0 }, .{ "flags", 8 }, .{ "padding", 12 } });
    assertLayout(fuse_link_in, 8, .{.{ "oldnodeid", 0 }});
    assertLayout(fuse_setattr_in, 88, .{ .{ "valid", 0 }, .{ "padding", 4 }, .{ "fh", 8 }, .{ "size", 16 }, .{ "lock_owner", 24 }, .{ "atime", 32 }, .{ "mtime", 40 }, .{ "ctime", 48 }, .{ "atimensec", 56 }, .{ "mtimensec", 60 }, .{ "ctimensec", 64 }, .{ "mode", 68 }, .{ "unused4", 72 }, .{ "uid", 76 }, .{ "gid", 80 }, .{ "unused5", 84 } });
    assertLayout(fuse_open_in, 8, .{ .{ "flags", 0 }, .{ "open_flags", 4 } });
    assertLayout(fuse_create_in, 16, .{ .{ "flags", 0 }, .{ "mode", 4 }, .{ "umask", 8 }, .{ "open_flags", 12 } });
    assertLayout(fuse_release_in, 24, .{ .{ "fh", 0 }, .{ "flags", 8 }, .{ "release_flags", 12 }, .{ "lock_owner", 16 } });
    assertLayout(fuse_flush_in, 24, .{ .{ "fh", 0 }, .{ "unused", 8 }, .{ "padding", 12 }, .{ "lock_owner", 16 } });
    assertLayout(fuse_read_in, 40, .{ .{ "fh", 0 }, .{ "offset", 8 }, .{ "size", 16 }, .{ "read_flags", 20 }, .{ "lock_owner", 24 }, .{ "flags", 32 }, .{ "padding", 36 } });
    assertLayout(fuse_write_in, 40, .{ .{ "fh", 0 }, .{ "offset", 8 }, .{ "size", 16 }, .{ "write_flags", 20 }, .{ "lock_owner", 24 }, .{ "flags", 32 }, .{ "padding", 36 } });
    assertLayout(fuse_fsync_in, 16, .{ .{ "fh", 0 }, .{ "fsync_flags", 8 }, .{ "padding", 12 } });
    assertLayout(fuse_setxattr_in, 8, .{ .{ "size", 0 }, .{ "flags", 4 } });
    assertLayout(fuse_getxattr_in, 8, .{ .{ "size", 0 }, .{ "padding", 4 } });
    assertLayout(fuse_lk_in, 48, .{ .{ "fh", 0 }, .{ "owner", 8 }, .{ "lk", 16 }, .{ "lk_flags", 40 }, .{ "padding", 44 } });
    assertLayout(fuse_access_in, 8, .{ .{ "mask", 0 }, .{ "padding", 4 } });
    assertLayout(fuse_interrupt_in, 8, .{.{ "unique", 0 }});
    assertLayout(fuse_bmap_in, 16, .{ .{ "block", 0 }, .{ "blocksize", 8 }, .{ "padding", 12 } });
    assertLayout(fuse_ioctl_in, 32, .{ .{ "fh", 0 }, .{ "flags", 8 }, .{ "cmd", 12 }, .{ "arg", 16 }, .{ "in_size", 24 }, .{ "out_size", 28 } });
    assertLayout(fuse_poll_in, 24, .{ .{ "fh", 0 }, .{ "kh", 8 }, .{ "flags", 16 }, .{ "events", 20 } });
    assertLayout(fuse_fallocate_in, 32, .{ .{ "fh", 0 }, .{ "offset", 8 }, .{ "length", 16 }, .{ "mode", 24 }, .{ "padding", 28 } });
    assertLayout(fuse_notify_retrieve_in, 40, .{ .{ "dummy1", 0 }, .{ "offset", 8 }, .{ "size", 16 }, .{ "dummy2", 20 }, .{ "dummy3", 24 }, .{ "dummy4", 32 } });
    assertLayout(fuse_lseek_in, 24, .{ .{ "fh", 0 }, .{ "offset", 8 }, .{ "whence", 16 }, .{ "padding", 20 } });
    assertLayout(fuse_copy_file_range_in, 56, .{ .{ "fh_in", 0 }, .{ "off_in", 8 }, .{ "nodeid_out", 16 }, .{ "fh_out", 24 }, .{ "off_out", 32 }, .{ "len", 40 }, .{ "flags", 48 } });

    assertLayout(fuse_open_out, 16, .{ .{ "fh", 0 }, .{ "open_flags", 8 }, .{ "backing_id", 12 } });
    assertLayout(fuse_write_out, 8, .{ .{ "size", 0 }, .{ "padding", 4 } });
    assertLayout(fuse_getxattr_out, 8, .{ .{ "size", 0 }, .{ "padding", 4 } });
    assertLayout(fuse_lk_out, 24, .{.{ "lk", 0 }});
    assertLayout(fuse_bmap_out, 8, .{.{ "block", 0 }});
    assertLayout(fuse_ioctl_out, 16, .{ .{ "result", 0 }, .{ "flags", 4 }, .{ "in_iovs", 8 }, .{ "out_iovs", 12 } });
    assertLayout(fuse_ioctl_iovec, 16, .{ .{ "base", 0 }, .{ "len", 8 } });
    assertLayout(fuse_poll_out, 8, .{ .{ "revents", 0 }, .{ "padding", 4 } });
    assertLayout(fuse_lseek_out, 8, .{.{ "offset", 0 }});
    assertLayout(fuse_dirent, 24, .{ .{ "ino", 0 }, .{ "off", 8 }, .{ "namelen", 16 }, .{ "type", 20 } });
    assertLayout(fuse_direntplus, 152, .{ .{ "entry_out", 0 }, .{ "dirent", 128 } });

    assertLayout(fuse_notify_poll_wakeup_out, 8, .{.{ "kh", 0 }});
    assertLayout(fuse_notify_inval_inode_out, 24, .{ .{ "ino", 0 }, .{ "off", 8 }, .{ "len", 16 } });
    assertLayout(fuse_notify_inval_entry_out, 16, .{ .{ "parent", 0 }, .{ "namelen", 8 }, .{ "flags", 12 } });
    assertLayout(fuse_notify_store_out, 24, .{ .{ "nodeid", 0 }, .{ "offset", 8 }, .{ "size", 16 }, .{ "padding", 20 } });
    assertLayout(fuse_notify_retrieve_out, 32, .{ .{ "notify_unique", 0 }, .{ "nodeid", 8 }, .{ "offset", 16 }, .{ "size", 24 }, .{ "padding", 28 } });
    assertLayout(fuse_notify_delete_out, 24, .{ .{ "parent", 0 }, .{ "child", 8 }, .{ "namelen", 16 }, .{ "padding", 20 } });
    assertLayout(fuse_notify_prune_out, 16, .{ .{ "count", 0 }, .{ "padding", 4 }, .{ "spare", 8 } });

    assertLayout(fuse_uring_ent_in_out, 32, .{ .{ "flags", 0 }, .{ "commit_id", 8 }, .{ "payload_sz", 16 }, .{ "padding", 20 }, .{ "reserved", 24 } });
    assertLayout(fuse_uring_req_header, 288, .{ .{ "in_out", 0 }, .{ "op_in", 128 }, .{ "ring_ent_in_out", 256 } });
    assertLayout(fuse_uring_cmd_req, 24, .{ .{ "flags", 0 }, .{ "commit_id", 8 }, .{ "qid", 16 }, .{ "init_flags", 18 }, .{ "queue_depth", 20 }, .{ "padding", 22 } });
    assertLayout(io_uring_buf_reg, 40, .{ .{ "ring_addr", 0 }, .{ "ring_entries", 8 }, .{ "bgid", 12 }, .{ "flags", 14 }, .{ "resv", 16 } });
    // The command area holds the request with room to spare; the in_out
    // region holds either header.
    std.debug.assert(@sizeOf(fuse_uring_cmd_req) <= SQE128_CMD_LEN);
    std.debug.assert(SQE128_CMD_OFFSET + SQE128_CMD_LEN == SQE128_LEN);
    std.debug.assert(@sizeOf(fuse_in_header) <= FUSE_URING_IN_OUT_HEADER_SZ);
    std.debug.assert(@sizeOf(fuse_out_header) <= FUSE_URING_IN_OUT_HEADER_SZ);
    // Every per-op request struct fits `op_in` (the largest is setattr_in).
    std.debug.assert(@sizeOf(fuse_setattr_in) <= FUSE_URING_OP_IN_OUT_SZ);
    std.debug.assert(@sizeOf(fuse_init_in) <= FUSE_URING_OP_IN_OUT_SZ);
    // The folded bits sit where the spec says.
    std.debug.assert(FUSE_OVER_IO_URING == @as(u64, 1 << 9) << 32);
    std.debug.assert(FUSE_TIME_LIMITS == @as(u64, 1 << 30) << 32);
}

// ---------------------------------------------------------------------------
// Total codecs
// ---------------------------------------------------------------------------

/// `T` from the first `@sizeOf(T)` bytes by copy: any alignment, no trap.
inline fn readStruct(comptime T: type, bytes: []const u8) T {
    var v: T = undefined;
    @memcpy(std.mem.asBytes(&v), bytes[0..@sizeOf(T)]);
    return v;
}

/// Canonical total decode: exactly `@sizeOf(T)` bytes.
pub fn decodeExact(comptime T: type, bytes: []const u8) ?T {
    if (bytes.len != @sizeOf(T)) return null;
    return readStruct(T, bytes);
}

/// `T` as the prefix of a longer body (`op_in ‖ names/payload`); null when
/// the body is shorter than the struct.
pub fn decodePrefix(comptime T: type, bytes: []const u8) ?T {
    if (bytes.len < @sizeOf(T)) return null;
    return readStruct(T, bytes);
}

/// The struct's wire bytes.
pub fn encode(value: anytype) [@sizeOf(@TypeOf(value))]u8 {
    return std.mem.toBytes(value);
}

/// Zero-initialised struct (every padding word 0 — the kernel checks some).
pub fn zeroed(comptime T: type) T {
    return std.mem.zeroes(T);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

const testing = std.testing;

/// Every wire struct, for the totality sweeps.
const all_structs = .{
    fuse_in_header,              fuse_out_header,             fuse_attr,                   fuse_kstatfs,            fuse_entry_out,
    fuse_attr_out,               fuse_statfs_out,             fuse_file_lock,              fuse_init_in,            fuse_init_out,
    fuse_forget_in,              fuse_forget_one,             fuse_batch_forget_in,        fuse_getattr_in,         fuse_mknod_in,
    fuse_mkdir_in,               fuse_rename_in,              fuse_rename2_in,             fuse_link_in,            fuse_setattr_in,
    fuse_open_in,                fuse_create_in,              fuse_release_in,             fuse_flush_in,           fuse_read_in,
    fuse_write_in,               fuse_fsync_in,               fuse_setxattr_in,            fuse_getxattr_in,        fuse_lk_in,
    fuse_access_in,              fuse_interrupt_in,           fuse_bmap_in,                fuse_ioctl_in,           fuse_poll_in,
    fuse_fallocate_in,           fuse_notify_retrieve_in,     fuse_lseek_in,               fuse_copy_file_range_in, fuse_open_out,
    fuse_write_out,              fuse_getxattr_out,           fuse_lk_out,                 fuse_bmap_out,           fuse_ioctl_out,
    fuse_ioctl_iovec,            fuse_poll_out,               fuse_lseek_out,              fuse_dirent,             fuse_direntplus,
    fuse_notify_poll_wakeup_out, fuse_notify_inval_inode_out, fuse_notify_inval_entry_out, fuse_notify_store_out,   fuse_notify_retrieve_out,
    fuse_notify_delete_out,      fuse_notify_prune_out,       fuse_uring_ent_in_out,       fuse_uring_req_header,   fuse_uring_cmd_req,
    io_uring_buf_reg,
};

test "the wire is little-endian native: a u32 at offset 0 lands low byte first" {
    var h = zeroed(fuse_in_header);
    h.len = 0x1122_3344;
    h.unique = 0x0102_0304_0506_0708;
    const b = encode(h);
    try testing.expectEqualSlices(u8, &.{ 0x44, 0x33, 0x22, 0x11 }, b[0..4]);
    try testing.expectEqualSlices(u8, &.{ 8, 7, 6, 5, 4, 3, 2, 1 }, b[8..16]);
}

test "spec table pins: in_header 40 (total_extlen@36), out_header 16, attr 88 (flags@84), entry_out 128, attr_out 104, init_out 64 (time_min@48, time_max@56), open_in open_flags@4" {
    // The comptime block is the law; this restates the rows the specs pin by name.
    try testing.expectEqual(@as(usize, 40), @sizeOf(fuse_in_header));
    try testing.expectEqual(@as(usize, 36), @offsetOf(fuse_in_header, "total_extlen"));
    try testing.expectEqual(@as(usize, 16), @sizeOf(fuse_out_header));
    try testing.expectEqual(@as(usize, 88), @sizeOf(fuse_attr));
    try testing.expectEqual(@as(usize, 84), @offsetOf(fuse_attr, "flags"));
    try testing.expectEqual(@as(usize, 128), @sizeOf(fuse_entry_out));
    try testing.expectEqual(@as(usize, 104), @sizeOf(fuse_attr_out));
    try testing.expectEqual(@as(usize, 64), @sizeOf(fuse_init_out));
    try testing.expectEqual(@as(usize, 48), @offsetOf(fuse_init_out, "time_min"));
    try testing.expectEqual(@as(usize, 56), @offsetOf(fuse_init_out, "time_max"));
    try testing.expectEqual(@as(usize, 4), @offsetOf(fuse_open_in, "open_flags"));
    try testing.expectEqual(@as(usize, 12), @offsetOf(fuse_create_in, "open_flags"));
    try testing.expectEqual(@as(usize, 12), @offsetOf(fuse_open_out, "backing_id"));
}

test "uring ABI pins: req header 288 (op_in@128, ring_ent@256), cmd_req 24 (qid@16, union u16@18, queue_depth@20), ent_in_out 32, buf_reg 40, SQE128 placement" {
    try testing.expectEqual(@as(usize, 288), @sizeOf(fuse_uring_req_header));
    try testing.expectEqual(@as(usize, 128), @offsetOf(fuse_uring_req_header, "op_in"));
    try testing.expectEqual(@as(usize, 256), @offsetOf(fuse_uring_req_header, "ring_ent_in_out"));
    try testing.expectEqual(@as(usize, 24), @sizeOf(fuse_uring_cmd_req));
    try testing.expectEqual(@as(usize, 16), @offsetOf(fuse_uring_cmd_req, "qid"));
    try testing.expectEqual(@as(usize, 18), @offsetOf(fuse_uring_cmd_req, "init_flags"));
    try testing.expectEqual(@as(usize, 20), @offsetOf(fuse_uring_cmd_req, "queue_depth"));
    try testing.expectEqual(@as(usize, 32), @sizeOf(fuse_uring_ent_in_out));
    try testing.expectEqual(@as(usize, 40), @sizeOf(io_uring_buf_reg));
    // The union u16 sits at SQE offset 48 + 18 = 66 (pinned by the reference's wire test).
    try testing.expectEqual(@as(usize, 66), SQE128_CMD_OFFSET + @offsetOf(fuse_uring_cmd_req, "init_flags"));
    var req = zeroed(fuse_uring_cmd_req);
    req.init_flags = FUSE_URING_COMMIT_RETAIN;
    try testing.expectEqual(FUSE_URING_COMMIT_RETAIN, req.commitFlags());
    var reg = zeroed(io_uring_buf_reg);
    reg.ring_addr = 4096;
    try testing.expectEqual(@as(u32, 4096), reg.bufSize());
}

test "capability bits: OVER_IO_URING = 1<<41 = flags2 bit 9; TIME_LIMITS = 1<<62 = flags2 bit 30; INIT_EXT 1<<30; KILLPRIV_V2 1<<28; fold/split round-trip" {
    try testing.expectEqual(@as(u64, 1) << 41, FUSE_OVER_IO_URING);
    try testing.expectEqual(@as(u32, 1 << 9), flags2Of(FUSE_OVER_IO_URING));
    try testing.expectEqual(@as(u64, 1) << 62, FUSE_TIME_LIMITS);
    try testing.expectEqual(@as(u32, 1 << 30), flags2Of(FUSE_TIME_LIMITS));
    try testing.expectEqual(@as(u64, 1 << 30), FUSE_INIT_EXT);
    try testing.expectEqual(@as(u64, 1 << 28), FUSE_HANDLE_KILLPRIV_V2);
    try testing.expectEqual(@as(u64, 1 << 19), FUSE_HANDLE_KILLPRIV);
    try testing.expectEqual(@as(u64, 1 << 1), FUSE_POSIX_LOCKS);
    try testing.expectEqual(@as(u64, 1 << 10), FUSE_FLOCK_LOCKS);
    try testing.expectEqual(@as(u64, 1 << 20), FUSE_POSIX_ACL);
    const w = fold(0xDEAD_BEEF, 0x0000_0201);
    try testing.expectEqual(@as(u32, 0xDEAD_BEEF), flagsOf(w));
    try testing.expectEqual(@as(u32, 0x201), flags2Of(w));
    try testing.expectEqual(w, (fuse_init_in{ .major = 7, .minor = 44, .max_readahead = 0, .flags = 0xDEAD_BEEF, .flags2 = 0x201, .unused = @splat(0) }).folded());
}

test "killpriv v2 bits pinned together: FATTR_KILL_SUIDGID 1<<11, WRITE_KILL_SUIDGID 1<<2, OPEN_KILL_SUIDGID 1<<0; FOPEN bits; release/getattr/read/lk bits" {
    try testing.expectEqual(@as(u32, 1 << 11), FATTR_KILL_SUIDGID);
    try testing.expectEqual(@as(u32, 1 << 2), FUSE_WRITE_KILL_SUIDGID);
    try testing.expectEqual(@as(u32, 1 << 0), FUSE_OPEN_KILL_SUIDGID);
    try testing.expectEqual(@as(u32, 1 << 0), FOPEN_DIRECT_IO);
    try testing.expectEqual(@as(u32, 1 << 1), FOPEN_KEEP_CACHE);
    try testing.expectEqual(@as(u32, 1 << 5), FOPEN_NOFLUSH);
    try testing.expectEqual(@as(u32, 1 << 6), FOPEN_PARALLEL_DIRECT_WRITES);
    try testing.expectEqual(@as(u32, 1), FUSE_RELEASE_FLUSH);
    try testing.expectEqual(@as(u32, 1), FUSE_GETATTR_FH);
    try testing.expectEqual(@as(u32, 2), FUSE_READ_LOCKOWNER);
    try testing.expectEqual(@as(u32, 1), FUSE_LK_FLOCK);
    try testing.expectEqual(@as(u32, 1), FUSE_EXPIRE_ONLY);
    try testing.expectEqual(@as(u32, 256), FUSE_IOCTL_MAX_IOV);
}

test "uring command and init-flag constants: REGISTER 1, COMMIT_AND_FETCH 2, RELEASE_PAYLOAD 3; BUF_RING/ZERO_COPY/RETENTION 1/2/4; COMMIT_RETAIN 1; kmbuf opcode pairs 37/38 and 38/39; mmap offset law" {
    try testing.expectEqual(@as(u32, 1), FUSE_IO_URING_CMD_REGISTER);
    try testing.expectEqual(@as(u32, 2), FUSE_IO_URING_CMD_COMMIT_AND_FETCH);
    try testing.expectEqual(@as(u32, 3), FUSE_IO_URING_CMD_RELEASE_PAYLOAD);
    try testing.expectEqual(@as(u16, 1), FUSE_URING_BUF_RING);
    try testing.expectEqual(@as(u16, 2), FUSE_URING_ZERO_COPY);
    try testing.expectEqual(@as(u16, 4), FUSE_URING_PAYLOAD_RETENTION);
    try testing.expectEqual(@as(u16, 1), FUSE_URING_COMMIT_RETAIN);
    try testing.expectEqual(@as(u32, 37), KMBUF_OPCODES_6_19.register);
    try testing.expectEqual(@as(u32, 38), KMBUF_OPCODES_6_19.unregister);
    try testing.expectEqual(@as(u32, 38), KMBUF_OPCODES_7_1.register);
    try testing.expectEqual(@as(u32, 39), KMBUF_OPCODES_7_1.unregister);
    try testing.expectEqual(@as(u64, 0x8800_0000), IORING_OFF_KMBUF_RING | (@as(u64, FUSE_URING_RINGBUF_GROUP) << IORING_OFF_KMBUF_SHIFT));
    try testing.expectEqual(@as(u64, 0x8803_0000), IORING_OFF_KMBUF_RING | (@as(u64, 3) << IORING_OFF_KMBUF_SHIFT));
    try testing.expectEqual(@as(u32, 1), IORING_CQE_F_BUFFER);
}

test "opcodes are named through uapi 7.45; an unknown decodes without error and says so; notify codes" {
    try testing.expectEqualStrings("lookup", Opcode.fromInt(1).name());
    try testing.expectEqualStrings("copy_file_range", Opcode.fromInt(47).name());
    try testing.expectEqualStrings("statx", Opcode.fromInt(52).name());
    try testing.expect(Opcode.fromInt(52).isKnown());
    try testing.expect(!Opcode.fromInt(7).isKnown());
    try testing.expect(!Opcode.fromInt(99).isKnown());
    try testing.expectEqualStrings("unknown", Opcode.fromInt(99).name());
    try testing.expectEqual(@as(u32, 26), @backingInt(Opcode.init));
    try testing.expectEqual(@as(u32, 42), @backingInt(Opcode.batch_forget));
    try testing.expectEqual(@as(u32, 2), @backingInt(NotifyCode.inval_inode));
    try testing.expectEqual(@as(u32, 9), @backingInt(NotifyCode.prune));
}

test "time limits: the whole-second interior of the i64-ns word, conservative by one second on the floor" {
    const ns_per_s: i128 = std.time.ns_per_s;
    try testing.expect(@as(i128, TIME_LIMITS_MAX_SEC) * ns_per_s <= std.math.maxInt(i64));
    try testing.expect((@as(i128, TIME_LIMITS_MAX_SEC) + 1) * ns_per_s > std.math.maxInt(i64));
    try testing.expect(@as(i128, TIME_LIMITS_MIN_SEC) * ns_per_s >= std.math.minInt(i64));
    // The exact floor is −9_223_372_037 s + 145_224_192 ns; we stop one whole second above it.
    try testing.expect((@as(i128, TIME_LIMITS_MIN_SEC) - 1) * ns_per_s < std.math.minInt(i64));
    try testing.expectEqual(-TIME_LIMITS_MAX_SEC, TIME_LIMITS_MIN_SEC);
}

test "dirent record length law: (24 + namelen + 7) & ~7; direntplus 152 + name" {
    try testing.expectEqual(@as(usize, 24), fuse_dirent.recordLen(0));
    try testing.expectEqual(@as(usize, 32), fuse_dirent.recordLen(1));
    try testing.expectEqual(@as(usize, 32), fuse_dirent.recordLen(8));
    try testing.expectEqual(@as(usize, 40), fuse_dirent.recordLen(9));
    try testing.expectEqual(@as(usize, 280), fuse_dirent.recordLen(255));
    try testing.expectEqual(@as(usize, 152), fuse_direntplus.recordLen(0));
    try testing.expectEqual(@as(usize, 160), fuse_direntplus.recordLen(5));
    for (0..300) |n| try testing.expectEqual(@as(usize, 0), fuse_dirent.recordLen(n) % 8);
}

test "codec: exact decode admits only @sizeOf(T); prefix decode admits ≥; encode∘decode = id; unaligned sources never trap" {
    inline for (all_structs) |T| {
        const n = @sizeOf(T);
        var raw: [n + 9]u8 = undefined;
        for (&raw, 0..) |*b, i| b.* = @truncate(i * 37 + 11);
        // Exact: n only.
        try testing.expect(decodeExact(T, raw[0..n]) != null);
        try testing.expect(decodeExact(T, raw[0 .. n - 1]) == null);
        try testing.expect(decodeExact(T, raw[0 .. n + 1]) == null);
        try testing.expect(decodeExact(T, raw[0..0]) == null);
        // Prefix: ≥ n.
        try testing.expect(decodePrefix(T, raw[0..n]) != null);
        try testing.expect(decodePrefix(T, raw[0 .. n + 9]) != null);
        try testing.expect(decodePrefix(T, raw[0 .. n - 1]) == null);
        // Round trip through an odd (unaligned) offset.
        const v = decodePrefix(T, raw[1 .. n + 1]).?;
        try testing.expectEqualSlices(u8, raw[1 .. n + 1], &encode(v));
        try testing.expectEqual(v, decodeExact(T, &encode(v)).?);
    }
}

fn checkDecodersTotal(bytes: []const u8) void {
    inline for (all_structs) |T| {
        _ = decodeExact(T, bytes);
        _ = decodePrefix(T, bytes);
    }
    _ = Opcode.fromInt(if (bytes.len >= 4) std.mem.readInt(u32, bytes[0..4], .little) else 0).name();
}

test "totality: every decoder returns or null on every length 0..300 (deterministic)" {
    var buf: [300]u8 = undefined;
    var prng = std.Random.DefaultPrng.init(0xF05E);
    prng.random().bytes(&buf);
    for (0..buf.len + 1) |len| checkDecodersTotal(buf[0..len]);
}

fn decodersTotalFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [512]u8 = undefined;
    const len = smith.slice(&buf);
    checkDecodersTotal(buf[0..len]);
}

test "fuzz: every decoder is total on arbitrary bytes" {
    try std.testing.fuzz({}, decodersTotalFuzz, .{});
}
