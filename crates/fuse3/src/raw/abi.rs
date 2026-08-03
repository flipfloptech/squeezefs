//! FUSE kernel interface.
//!
//! Types and definitions used for communication between the kernel driver and the userspace
//! part of a FUSE filesystem. Since the kernel driver may be installed independently, the ABI
//! interface is versioned and capabilities are exchanged during the initialization (mounting)
//! of a filesystem.
//!
//! [OSXFUSE (macOS)](https://github.com/osxfuse/fuse/blob/master/include/fuse_kernel.h)
//! - supports ABI 7.8 in OSXFUSE 2.x
//! - supports ABI 7.19 since OSXFUSE 3.0.0
//!
//! [libfuse (Linux/BSD)](https://github.com/libfuse/libfuse/blob/master/include/fuse_kernel.h)
//! - supports ABI 7.8 since FUSE 2.6.0
//! - supports ABI 7.12 since FUSE 2.8.0
//! - supports ABI 7.18 since FUSE 2.9.0
//! - supports ABI 7.19 since FUSE 2.9.1
//! - supports ABI 7.26 since FUSE 3.0.0
//!
//! Items without a version annotation are valid with ABI 7.8 and later

// Protocol-completeness exception to the no-dead-code law (AGENTS.md): this
// module mirrors the kernel's fuse_kernel.h ABI surface verbatim — opcodes,
// structs, and fields exist because the wire defines them, not because the
// daemon consumes each one yet. One documented module-level allow replaces
// the former 17 item-level `#[allow(dead_code)]` attributes (pre-rc spec
// §10 ENG-14).
#![allow(dead_code)]

use std::error::Error;
use std::fmt::{self, Debug, Display, Formatter};
use std::mem;

use serde::{Deserialize, Serialize};

/// The min size of read buffer. In Linux kernel the `FUSE_MIN_READ_BUFFER` is
///
/// ```c
/// /* The read buffer is required to be at least 8k, but may be much larger */
/// #define FUSE_MIN_READ_BUFFER 8192
/// ```
pub const FUSE_MIN_READ_BUFFER_SIZE: usize = 8 * 1024;

pub const FUSE_KERNEL_VERSION: u32 = 7;

/// FUSE-1 (pre-rc spec §4): 36 EXACTLY — the extended-init (`FUSE_INIT_EXT`
/// / `flags2`) protocol revision, the minimum under which mainline
/// `process_init_reply()` honors the reply's `flags2` fold. The 31→36 audit
/// (fix/fuse-init-ext) verified no `fc->minor`-gated kernel behavior exists
/// in (23, 36] — everything 7.32–7.36 rides INIT flags the daemon does not
/// echo. Raising this further requires a fresh audit of every `fc->minor`
/// gate in the target kernels; do not chase newer minors speculatively
/// (capabilities ride INIT flags, not the version).
pub const FUSE_KERNEL_MINOR_VERSION: u32 = 36;

pub const DEFAULT_TIME_GRAN: u32 = 1;

// DEFAULT_MAX_PAGES (the blanket u16::MAX advertisement) was deleted by
// the 2026-08-04 geometry campaign: over-uring INIT replies advertise the
// max_pages the TransportGeometry plan stands on (a blanket u16::MAX let
// the kernel's REGISTER bound exceed the registered ents on raised
// fs.fuse.max_pages_limit boxes — refused REGISTERs, failed mounts).

// TODO find valid value
pub const DEFAULT_MAP_ALIGNMENT: u16 = 0;

// Bitmasks for fuse_setattr_in.valid
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
/// FUSE_HANDLE_KILLPRIV_V2: the daemon must clear suid / group-exec
/// sgid / security.capability alongside this SETATTR (size-changing
/// truncates by non-CAP_FSETID callers; non-directory chowns).
pub const FATTR_KILL_SUIDGID: u32 = 1 << 11;

#[cfg(target_os = "macos")]
pub const FATTR_CRTIME: u32 = 1 << 28;
#[cfg(target_os = "macos")]
pub const FATTR_CHGTIME: u32 = 1 << 29;
#[cfg(target_os = "macos")]
pub const FATTR_BKUPTIME: u32 = 1 << 30;
#[cfg(target_os = "macos")]
pub const FATTR_FLAGS: u32 = 1 << 31;

// Init request/reply flags
/// asynchronous read requests
pub const FUSE_ASYNC_READ: u32 = 1 << 0;

// Referenced only by the INIT-negotiation pins (the capability is
// deliberately never echoed — kernel-local POSIX locks; the `file-lock`
// plumbing itself never reads it).
#[cfg(test)]
/// locking for POSIX file locks
pub const FUSE_POSIX_LOCKS: u32 = 1 << 1;

/// kernel sends file handle for fstat, etc...
pub const FUSE_FILE_OPS: u32 = 1 << 2;

/// handles the O_TRUNC open flag in the filesystem
pub const FUSE_ATOMIC_O_TRUNC: u32 = 1 << 3;

/// filesystem handles lookups of "." and ".."
pub const FUSE_EXPORT_SUPPORT: u32 = 1 << 4;

/// filesystem can handle write size larger than 4kB
pub const FUSE_BIG_WRITES: u32 = 1 << 5;

/// don't apply umask to file mode on create operations
pub const FUSE_DONT_MASK: u32 = 1 << 6;

// FUSE_SPLICE_WRITE (1<<7) / FUSE_SPLICE_MOVE (1<<8) / FUSE_SPLICE_READ
// (1<<9) were deleted with the reply-path splice machinery (L3 lever A):
// the daemon implements no splice path and must not advertise one.

/// locking for BSD style file locks
pub const FUSE_FLOCK_LOCKS: u32 = 1 << 10;

/// kernel supports ioctl on directories
pub const FUSE_HAS_IOCTL_DIR: u32 = 1 << 11;

/// automatically invalidate cached pages
pub const FUSE_AUTO_INVAL_DATA: u32 = 1 << 12;

/// do READDIRPLUS (READDIR+LOOKUP in one)
pub const FUSE_DO_READDIRPLUS: u32 = 1 << 13;

/// adaptive readdirplus
pub const FUSE_READDIRPLUS_AUTO: u32 = 1 << 14;

/// asynchronous direct I/O submission
pub const FUSE_ASYNC_DIO: u32 = 1 << 15;

/// use writeback cache for buffered writes
pub const FUSE_WRITEBACK_CACHE: u32 = 1 << 16;

/// kernel supports zero-message opens
pub const FUSE_NO_OPEN_SUPPORT: u32 = 1 << 17;

/// allow parallel lookups and readdir
pub const FUSE_PARALLEL_DIROPS: u32 = 1 << 18;

/// fs handles killing suid/sgid/cap on write/chown/trunc
pub const FUSE_HANDLE_KILLPRIV: u32 = 1 << 19;

// Referenced only by the INIT-negotiation pins (the capability is
// deliberately never echoed — the no-ACL posture).
#[cfg(test)]
/// filesystem supports posix acls
pub const FUSE_POSIX_ACL: u32 = 1 << 20;

/// reading the device after abort returns ECONNABORTED
pub const FUSE_ABORT_ERROR: u32 = 1 << 21;

/// init_out.max_pages contains the max number of req pages
pub const FUSE_MAX_PAGES: u32 = 1 << 22;

/// cache READLINK responses
pub const FUSE_CACHE_SYMLINKS: u32 = 1 << 23;

/// kernel supports zero-message opendir
pub const FUSE_NO_OPENDIR_SUPPORT: u32 = 1 << 24;

/// only invalidate cached pages on explicit request
pub const FUSE_EXPLICIT_INVAL_DATA: u32 = 1 << 25;

/// map_alignment field is valid
pub const FUSE_MAP_ALIGNMENT: u32 = 1 << 26;

/// fs kills suid/sgid/cap on write/chown/trunc — v2 (Linux ≥ 5.11):
/// unlike v1's unconditional kill, the kernel only flags requests whose
/// caller lacks CAP_FSETID (write/trunc) and the daemon must preserve
/// sgid on non-group-exec files (the mandatory-locking-marker class).
/// Negotiating this deletes the kernel's per-write(2)
/// GETXATTR("security.capability") killpriv probe (the OQ-1 §4 finding:
/// half of every write-syscall-bound stream's requests).
pub const FUSE_HANDLE_KILLPRIV_V2: u32 = 1 << 28;

/// extended fuse_init_in request (fuse ≥ 7.36): `flags2` at byte offset
/// 16 carries init flag bits 32..63 (`FUSE_OVER_IO_URING` = bit 41 lives
/// there). `handle_init` folds it into the published `KernelInit`
/// capability word.
pub const FUSE_INIT_EXT: u32 = 1 << 30;

/// sqz-kernel private INIT capability (kernel-sqz patch 0027 —
/// `docker/kernel-sqz/V2-CANDIDATES.md` candidate 5): folded capability
/// bit 62 = flags2 bit 30, deliberately far above upstream's bit-42
/// watermark so an upstream collision is a recompile, not an ABI trap.
/// When the kernel offers it and the daemon echoes it with a nonzero
/// `time_max`, `fuse_init_out.time_{min,max}` become the superblock's
/// `s_time_min`/`s_time_max`, making VFS `timestamp_truncate()` clamp
/// incore exactly where the daemon clamps durable state (the fstests
/// generic/634 finite-timestamp-range class). Stock kernels never offer
/// the bit and never read the fields.
pub const FUSE_TIME_LIMITS: u64 = 1 << 62;

#[cfg(target_os = "macos")]
pub const FUSE_ALLOCATE: u32 = 1 << 27;
#[cfg(target_os = "macos")]
pub const FUSE_EXCHANGE_DATA: u32 = 1 << 28;
#[cfg(target_os = "macos")]
pub const FUSE_CASE_INSENSITIVE: u32 = 1 << 29;
#[cfg(target_os = "macos")]
pub const FUSE_VOL_RENAME: u32 = 1 << 30;
#[cfg(target_os = "macos")]
pub const FUSE_XTIMES: u32 = 1 << 31;

// CUSE init request/reply flags
// use unrestricted ioctl
// pub const CUSE_UNRESTRICTED_IOCTL: u32 = 1 << 0;

// Release flags
pub const FUSE_RELEASE_FLUSH: u32 = 1 << 0;

pub const FUSE_RELEASE_FLOCK_UNLOCK: u32 = 1 << 1;

// Getattr flags
pub const FUSE_GETATTR_FH: u32 = 1 << 0;

// Lock flags, this is BSD file lock
pub const FUSE_LK_FLOCK: u32 = 1 << 0;

// Write flags
/// delayed write from page cache, file handle is guessed
pub const FUSE_WRITE_CACHE: u32 = 1 << 0;

/// lock_owner field is valid
pub const FUSE_WRITE_LOCKOWNER: u32 = 1 << 1;

/// FUSE_HANDLE_KILLPRIV_V2: kill suid + group-exec sgid + the
/// security.capability xattr for this WRITE (caller lacks CAP_FSETID).
pub const FUSE_WRITE_KILL_SUIDGID: u32 = 1 << 2;

// Open flags (fuse_open_in.open_flags)
/// FUSE_HANDLE_KILLPRIV_V2: kill suid + group-exec sgid + the
/// security.capability xattr on this O_TRUNC open (caller lacks
/// CAP_FSETID).
pub const FUSE_OPEN_KILL_SUIDGID: u32 = 1 << 0;

// Read flags
pub const FUSE_READ_LOCKOWNER: u32 = 1 << 1;

// IOCTL flags
/// 32bit compat ioctl on 64bit machine
pub const FUSE_IOCTL_COMPAT: u32 = 1 << 0;

/// not restricted to well-formed ioctls, retry allowed
pub const FUSE_IOCTL_UNRESTRICTED: u32 = 1 << 1;

/// retry with new iovecs
pub const FUSE_IOCTL_RETRY: u32 = 1 << 2;

/// 32bit ioctl
pub const FUSE_IOCTL_32BIT: u32 = 1 << 3;

/// is a directory
pub const FUSE_IOCTL_DIR: u32 = 1 << 4;

/// maximum of in_iovecs + out_iovecs
pub const FUSE_IOCTL_MAX_IOV: u32 = 256;

// Poll flags
/// request poll notify
pub const FUSE_POLL_SCHEDULE_NOTIFY: u32 = 1 << 0;

#[derive(Debug, Serialize)]
#[allow(non_camel_case_types)]
pub struct fuse_attr {
    pub ino: u64,
    pub size: u64,
    pub blocks: u64,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    #[cfg(target_os = "macos")]
    pub crtime: u64,
    pub atimensec: u32,
    pub mtimensec: u32,
    pub ctimensec: u32,
    #[cfg(target_os = "macos")]
    pub crtimensec: u32,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u32,
    #[cfg(target_os = "macos")]
    // see chflags(2)
    pub flags: u32,
    pub blksize: u32,
    pub(crate) _padding: u32,
}

#[derive(Debug, Serialize)]
#[allow(non_camel_case_types)]
pub struct fuse_kstatfs {
    // Total blocks (in units of frsize)
    pub blocks: u64,
    // Free blocks
    pub bfree: u64,
    // Free blocks for unprivileged users
    pub bavail: u64,
    // Total inodes
    pub files: u64,
    // Free inodes
    pub ffree: u64,
    // Filesystem block size
    pub bsize: u32,
    // Maximum filename length
    pub namelen: u32,
    // Fundamental file system block size
    pub frsize: u32,
    pub(crate) _padding: u32,
    pub spare: [u32; 6],
}

#[derive(Debug, Serialize, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_file_lock {
    pub start: u64,
    pub end: u64,
    pub r#type: u32,
    pub pid: u32,
}

/// Invalid opcode error.
#[derive(Debug)]
pub struct UnknownOpcodeError(pub u32);

#[derive(Debug, Ord, PartialOrd, Eq, PartialEq)]
#[allow(non_camel_case_types, clippy::upper_case_acronyms)]
pub enum fuse_opcode {
    FUSE_LOOKUP = 1,
    // no reply
    FUSE_FORGET = 2,
    FUSE_GETATTR = 3,
    FUSE_SETATTR = 4,
    FUSE_READLINK = 5,
    FUSE_SYMLINK = 6,
    FUSE_MKNOD = 8,
    FUSE_MKDIR = 9,
    FUSE_UNLINK = 10,
    FUSE_RMDIR = 11,
    FUSE_RENAME = 12,
    FUSE_LINK = 13,
    FUSE_OPEN = 14,
    FUSE_READ = 15,
    FUSE_WRITE = 16,
    FUSE_STATFS = 17,
    FUSE_RELEASE = 18,
    FUSE_FSYNC = 20,
    FUSE_SETXATTR = 21,
    FUSE_GETXATTR = 22,
    FUSE_LISTXATTR = 23,
    FUSE_REMOVEXATTR = 24,
    FUSE_FLUSH = 25,
    FUSE_INIT = 26,
    FUSE_OPENDIR = 27,
    FUSE_READDIR = 28,
    FUSE_RELEASEDIR = 29,
    FUSE_FSYNCDIR = 30,
    #[cfg(feature = "file-lock")]
    FUSE_GETLK = 31,
    #[cfg(feature = "file-lock")]
    FUSE_SETLK = 32,
    #[cfg(feature = "file-lock")]
    FUSE_SETLKW = 33,
    FUSE_ACCESS = 34,
    FUSE_CREATE = 35,
    FUSE_INTERRUPT = 36,
    FUSE_BMAP = 37,
    FUSE_DESTROY = 38,
    // TODO implement it after get enough info about it
    FUSE_IOCTL = 39,
    FUSE_POLL = 40,
    FUSE_NOTIFY_REPLY = 41,
    FUSE_BATCH_FORGET = 42,
    FUSE_FALLOCATE = 43,
    FUSE_READDIRPLUS = 44,
    FUSE_RENAME2 = 45,
    FUSE_LSEEK = 46,
    FUSE_COPY_FILE_RANGE = 47,
    // FUSE_SETUPMAPPING = 48,
    // FUSE_REMOVEMAPPING = 49,
    #[cfg(target_os = "macos")]
    FUSE_SETVOLNAME = 61,
    #[cfg(target_os = "macos")]
    FUSE_GETXTIMES = 62,
    #[cfg(target_os = "macos")]
    FUSE_EXCHANGE = 63,
    // CUSE_INIT = 4096,
}

impl Display for fuse_opcode {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Debug::fmt(self, f)
    }
}

impl TryFrom<u32> for fuse_opcode {
    type Error = UnknownOpcodeError;

    fn try_from(n: u32) -> Result<Self, Self::Error> {
        match n {
            1 => Ok(fuse_opcode::FUSE_LOOKUP),
            2 => Ok(fuse_opcode::FUSE_FORGET),
            3 => Ok(fuse_opcode::FUSE_GETATTR),
            4 => Ok(fuse_opcode::FUSE_SETATTR),
            5 => Ok(fuse_opcode::FUSE_READLINK),
            6 => Ok(fuse_opcode::FUSE_SYMLINK),
            8 => Ok(fuse_opcode::FUSE_MKNOD),
            9 => Ok(fuse_opcode::FUSE_MKDIR),
            10 => Ok(fuse_opcode::FUSE_UNLINK),
            11 => Ok(fuse_opcode::FUSE_RMDIR),
            12 => Ok(fuse_opcode::FUSE_RENAME),
            13 => Ok(fuse_opcode::FUSE_LINK),
            14 => Ok(fuse_opcode::FUSE_OPEN),
            15 => Ok(fuse_opcode::FUSE_READ),
            16 => Ok(fuse_opcode::FUSE_WRITE),
            17 => Ok(fuse_opcode::FUSE_STATFS),
            18 => Ok(fuse_opcode::FUSE_RELEASE),
            20 => Ok(fuse_opcode::FUSE_FSYNC),
            21 => Ok(fuse_opcode::FUSE_SETXATTR),
            22 => Ok(fuse_opcode::FUSE_GETXATTR),
            23 => Ok(fuse_opcode::FUSE_LISTXATTR),
            24 => Ok(fuse_opcode::FUSE_REMOVEXATTR),
            25 => Ok(fuse_opcode::FUSE_FLUSH),
            26 => Ok(fuse_opcode::FUSE_INIT),
            27 => Ok(fuse_opcode::FUSE_OPENDIR),
            28 => Ok(fuse_opcode::FUSE_READDIR),
            29 => Ok(fuse_opcode::FUSE_RELEASEDIR),
            30 => Ok(fuse_opcode::FUSE_FSYNCDIR),
            #[cfg(feature = "file-lock")]
            31 => Ok(fuse_opcode::FUSE_GETLK),
            #[cfg(feature = "file-lock")]
            32 => Ok(fuse_opcode::FUSE_SETLK),
            #[cfg(feature = "file-lock")]
            33 => Ok(fuse_opcode::FUSE_SETLKW),
            34 => Ok(fuse_opcode::FUSE_ACCESS),
            35 => Ok(fuse_opcode::FUSE_CREATE),
            36 => Ok(fuse_opcode::FUSE_INTERRUPT),
            37 => Ok(fuse_opcode::FUSE_BMAP),
            38 => Ok(fuse_opcode::FUSE_DESTROY),
            39 => Ok(fuse_opcode::FUSE_IOCTL),
            40 => Ok(fuse_opcode::FUSE_POLL),
            41 => Ok(fuse_opcode::FUSE_NOTIFY_REPLY),
            42 => Ok(fuse_opcode::FUSE_BATCH_FORGET),
            43 => Ok(fuse_opcode::FUSE_FALLOCATE),
            44 => Ok(fuse_opcode::FUSE_READDIRPLUS),
            45 => Ok(fuse_opcode::FUSE_RENAME2),
            46 => Ok(fuse_opcode::FUSE_LSEEK),
            47 => Ok(fuse_opcode::FUSE_COPY_FILE_RANGE),
            // 48 => Ok(fuse_opcode::FUSE_SETUPMAPPING),
            // 49 => Ok(fuse_opcode::FUSE_REMOVEMAPPING),
            #[cfg(target_os = "macos")]
            61 => Ok(fuse_opcode::FUSE_SETVOLNAME),
            #[cfg(target_os = "macos")]
            62 => Ok(fuse_opcode::FUSE_GETXTIMES),
            #[cfg(target_os = "macos")]
            63 => Ok(fuse_opcode::FUSE_EXCHANGE),

            // 4096 => Ok(fuse_opcode::CUSE_INIT),
            opcode => Err(UnknownOpcodeError(opcode)),
        }
    }
}

/// Invalid notify code error.
#[derive(Debug)]
pub struct InvalidNotifyCodeError(u32);

impl Display for InvalidNotifyCodeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "InvalidNotifyCodeError({})", self.0)
    }
}

impl Error for InvalidNotifyCodeError {}

#[derive(Debug)]
#[allow(non_camel_case_types, clippy::upper_case_acronyms)]
pub enum fuse_notify_code {
    /// notify kernel that a poll waiting for IO on a file handle should wake up.
    FUSE_POLL = 1,

    /// notify kernel that an inode should be invalidated.
    FUSE_NOTIFY_INVAL_INODE = 2,

    /// notify kernel that a directory entry should be invalidated.
    FUSE_NOTIFY_INVAL_ENTRY = 3,

    /// store data into kernel cache of an inode
    FUSE_NOTIFY_STORE = 4,

    /// retrieve data from kernel cache of an inode
    FUSE_NOTIFY_RETRIEVE = 5,

    /// notify kernel that a directory entry has been deleted
    FUSE_NOTIFY_DELETE = 6,
}

impl TryFrom<u32> for fuse_notify_code {
    type Error = InvalidNotifyCodeError;

    fn try_from(n: u32) -> Result<Self, Self::Error> {
        match n {
            1 => Ok(fuse_notify_code::FUSE_POLL),

            2 => Ok(fuse_notify_code::FUSE_NOTIFY_INVAL_INODE),

            3 => Ok(fuse_notify_code::FUSE_NOTIFY_INVAL_ENTRY),

            4 => Ok(fuse_notify_code::FUSE_NOTIFY_STORE),

            5 => Ok(fuse_notify_code::FUSE_NOTIFY_RETRIEVE),

            6 => Ok(fuse_notify_code::FUSE_NOTIFY_DELETE),

            invalid_code => Err(InvalidNotifyCodeError(invalid_code)),
        }
    }
}

pub const FUSE_ENTRY_OUT_SIZE: usize = mem::size_of::<fuse_entry_out>();

#[derive(Debug, Serialize)]
#[allow(non_camel_case_types)]
pub struct fuse_entry_out {
    pub nodeid: u64,
    pub generation: u64,
    pub entry_valid: u64,
    pub attr_valid: u64,
    pub entry_valid_nsec: u32,
    pub attr_valid_nsec: u32,
    pub attr: fuse_attr,
}

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_forget_in {
    pub nlookup: u64,
}

pub const FUSE_FORGET_ONE_SIZE: usize = mem::size_of::<fuse_forget_one>();

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_forget_one {
    pub nodeid: u64,
    /// FUSE-3k: the per-entry lookup count. It was parsed and thrown away
    /// (`_nlookup`), which is what made `BATCH_FORGET` an unconditional
    /// eviction per entry.
    pub nlookup: u64,
}

pub const FUSE_BATCH_FORGET_IN_SIZE: usize = mem::size_of::<fuse_batch_forget_in>();

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_batch_forget_in {
    pub count: u32,
    pub(crate) _dummy: u32,
}

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_getattr_in {
    pub getattr_flags: u32,
    pub dummy: u32,
    pub fh: u64,
}

pub const FUSE_ATTR_OUT_SIZE: usize = mem::size_of::<fuse_attr_out>();

#[derive(Debug, Serialize)]
#[allow(non_camel_case_types)]
pub struct fuse_attr_out {
    pub attr_valid: u64,
    pub attr_valid_nsec: u32,
    pub dummy: u32,
    pub attr: fuse_attr,
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
#[allow(non_camel_case_types)]
pub struct fuse_getxtimes_out {
    pub bkuptime: u64,
    pub crtime: u64,
    pub bkuptimensec: u32,
    pub crtimensec: u32,
}

pub const FUSE_MKNOD_IN_SIZE: usize = mem::size_of::<fuse_mknod_in>();

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_mknod_in {
    pub mode: u32,
    pub rdev: u32,
    pub(crate) _umask: u32,
    _padding: u32,
}

pub const FUSE_MKDIR_IN_SIZE: usize = mem::size_of::<fuse_mkdir_in>();

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_mkdir_in {
    pub mode: u32,
    pub umask: u32,
}

pub const FUSE_RENAME_IN_SIZE: usize = mem::size_of::<fuse_rename_in>();

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_rename_in {
    pub newdir: u64,
}

pub const FUSE_RENAME2_IN_SIZE: usize = mem::size_of::<fuse_rename2_in>();

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_rename2_in {
    pub newdir: u64,
    pub flags: u32,
    _padding: u32,
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
#[allow(non_camel_case_types)]
pub struct fuse_exchange_in {
    pub olddir: u64,
    pub newdir: u64,
    pub options: u64,
}

pub const FUSE_LINK_IN_SIZE: usize = mem::size_of::<fuse_link_in>();

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_link_in {
    pub oldnodeid: u64,
}

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_setattr_in {
    pub valid: u32,
    _padding: u32,
    pub fh: u64,
    pub size: u64,
    pub lock_owner: u64,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub atimensec: u32,
    pub mtimensec: u32,
    pub ctimensec: u32,
    pub mode: u32,
    pub unused4: u32,
    pub uid: u32,
    pub gid: u32,
    pub unused5: u32,
    #[cfg(target_os = "macos")]
    pub bkuptime: u64,
    #[cfg(target_os = "macos")]
    pub chgtime: u64,
    #[cfg(target_os = "macos")]
    pub crtime: u64,
    #[cfg(target_os = "macos")]
    pub bkuptimensec: u32,
    #[cfg(target_os = "macos")]
    pub chgtimensec: u32,
    #[cfg(target_os = "macos")]
    pub crtimensec: u32,
    #[cfg(target_os = "macos")]
    pub flags: u32, // see chflags(2)
}

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_open_in {
    pub flags: u32,
    /// `FUSE_OPEN_*` bits (uapi ≥ 7.33; zero on older kernels) —
    /// carries [`FUSE_OPEN_KILL_SUIDGID`] under FUSE_HANDLE_KILLPRIV_V2.
    pub open_flags: u32,
}

pub const FUSE_CREATE_IN_SIZE: usize = mem::size_of::<fuse_create_in>();

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_create_in {
    pub flags: u32,
    pub mode: u32,
    pub(crate) _umask: u32,
    _padding: u32,
}

pub const FUSE_OPEN_OUT_SIZE: usize = mem::size_of::<fuse_open_out>();

#[derive(Debug, Serialize)]
#[allow(non_camel_case_types)]
pub struct fuse_open_out {
    pub fh: u64,
    pub open_flags: u32,
    pub(crate) _padding: u32,
}

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_release_in {
    pub fh: u64,
    pub flags: u32,
    pub release_flags: u32,
    pub lock_owner: u64,
}

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_flush_in {
    pub fh: u64,
    pub(crate) _unused: u32,
    _padding: u32,
    pub lock_owner: u64,
}

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_read_in {
    pub fh: u64,
    pub offset: u64,
    pub size: u32,
    pub(crate) _read_flags: u32,
    pub lock_owner: u64,
    /// The file's open flags (O_DIRECT et al.) — the kernel sends them on
    /// every READ; exposed for per-request read classification
    /// (SqueezeFS R1b, docs/design-read-path.md §5.3).
    pub flags: u32,
    _padding: u32,
}

pub const FUSE_WRITE_IN_SIZE: usize = mem::size_of::<fuse_write_in>();

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_write_in {
    pub fh: u64,
    pub offset: u64,
    pub size: u32,
    pub write_flags: u32,
    pub(crate) _lock_owner: u64,
    pub flags: u32,
    _padding: u32,
}

pub const FUSE_WRITE_OUT_SIZE: usize = mem::size_of::<fuse_write_out>();

#[derive(Debug, Serialize)]
#[allow(non_camel_case_types)]
pub struct fuse_write_out {
    pub size: u32,
    pub(crate) _padding: u32,
}

pub const FUSE_STATFS_OUT_SIZE: usize = mem::size_of::<fuse_statfs_out>();

#[derive(Debug, Serialize)]
#[allow(non_camel_case_types)]
pub struct fuse_statfs_out {
    pub st: fuse_kstatfs,
}

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_fsync_in {
    pub fh: u64,
    pub fsync_flags: u32,
    _padding: u32,
}

pub const FUSE_SETXATTR_IN_SIZE: usize = mem::size_of::<fuse_setxattr_in>();

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_setxattr_in {
    pub size: u32,
    pub flags: u32,
    #[cfg(target_os = "macos")]
    pub position: u32,
    #[cfg(target_os = "macos")]
    _padding: u32,
}

pub const FUSE_GETXATTR_IN_SIZE: usize = mem::size_of::<fuse_getxattr_in>();

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_getxattr_in {
    pub size: u32,
    _padding: u32,
    #[cfg(target_os = "macos")]
    pub position: u32,
    #[cfg(target_os = "macos")]
    _padding2: u32,
}

pub const FUSE_GETXATTR_OUT_SIZE: usize = mem::size_of::<fuse_getxattr_out>();

#[derive(Debug, Serialize)]
#[allow(non_camel_case_types)]
pub struct fuse_getxattr_out {
    pub size: u32,
    pub(crate) _padding: u32,
}

#[cfg(feature = "file-lock")]
#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_lk_in {
    pub fh: u64,
    pub owner: u64,
    pub lk: fuse_file_lock,
    pub(crate) _lk_flags: u32,
    _padding: u32,
}

#[cfg(feature = "file-lock")]
pub const FUSE_LK_OUT_SIZE: usize = mem::size_of::<fuse_lk_out>();

#[cfg(feature = "file-lock")]
#[derive(Debug, Serialize)]
#[allow(non_camel_case_types)]
pub struct fuse_lk_out {
    pub lk: fuse_file_lock,
}

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_access_in {
    pub mask: u32,
    _padding: u32,
}

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_init_in {
    pub(crate) major: u32,
    pub(crate) minor: u32,
    pub max_readahead: u32,
    pub flags: u32,
}

pub const FUSE_INIT_OUT_SIZE: usize = mem::size_of::<fuse_init_out>();

#[derive(Debug, Serialize)]
#[allow(non_camel_case_types)]
pub struct fuse_init_out {
    pub major: u32,
    pub minor: u32,
    pub max_readahead: u32,
    pub flags: u32,
    pub max_background: u16,
    pub congestion_threshold: u16,
    pub max_write: u32,
    pub time_gran: u32,
    pub max_pages: u16,
    pub map_alignment: u16,
    /// High 32 init flags (bits 32..63). Bit 9 = `FUSE_OVER_IO_URING` (1ULL<<41),
    /// bit 30 = `FUSE_TIME_LIMITS` (1ULL<<62, sqz-kernel private).
    pub flags2: u32,
    pub max_stack_depth: u32,
    pub request_timeout: u16,
    pub unused: [u16; 3],
    /// sqz `FUSE_TIME_LIMITS` (kernel-sqz patch 0027): inode-timestamp
    /// floor in seconds; 0 unless the kernel offered the capability. The
    /// i64 pair sits AFTER the shrunk `unused[3]` so both stay naturally
    /// aligned and the struct stays the uapi's 64 bytes.
    pub time_min: i64,
    /// sqz `FUSE_TIME_LIMITS`: inode-timestamp ceiling in seconds; the
    /// kernel gates on the echoed flag AND a nonzero `time_max`.
    pub time_max: i64,
}

/*#[derive(Debug)]
#[allow(non_camel_case_types)]
pub struct cuse_init_in {
    pub major: u32,
    pub minor: u32,
    pub unused: u32,
    pub flags: u32,
}*/

/*#[derive(Debug)]
#[allow(non_camel_case_types)]
pub struct cuse_init_out {
    pub major: u32,
    pub minor: u32,
    pub unused: u32,
    pub flags: u32,
    pub max_read: u32,
    pub max_write: u32,
    // chardev major
    pub dev_major: u32,
    // chardev minor
    pub dev_minor: u32,
    pub spare: [u32; 10],
}*/

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_interrupt_in {
    pub unique: u64,
}

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_bmap_in {
    pub block: u64,
    pub blocksize: u32,
    _padding: u32,
}

pub const FUSE_BMAP_OUT_SIZE: usize = mem::size_of::<fuse_bmap_out>();

#[derive(Debug, Serialize)]
#[allow(non_camel_case_types)]
pub struct fuse_bmap_out {
    pub block: u64,
}

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_ioctl_in {
    pub fh: u64,
    pub flags: u32,
    pub cmd: u32,
    pub arg: u64,
    pub in_size: u32,
    pub out_size: u32,
}

#[derive(Debug)]
#[allow(non_camel_case_types)]
pub struct fuse_ioctl_iovec {
    pub base: u64,
    pub len: u64,
}

#[derive(Debug, Serialize)]
#[allow(non_camel_case_types)]
pub struct fuse_ioctl_out {
    pub result: i32,
    pub flags: u32,
    pub in_iovs: u32,
    pub out_iovs: u32,
}

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_poll_in {
    pub fh: u64,
    pub kh: u64,
    pub flags: u32,
    pub events: u32,
}

pub const FUSE_POLL_OUT_SIZE: usize = mem::size_of::<fuse_poll_out>();

#[derive(Debug, Serialize)]
#[allow(non_camel_case_types)]
pub struct fuse_poll_out {
    pub revents: u32,
    pub(crate) _padding: u32,
}

pub const FUSE_NOTIFY_POLL_WAKEUP_OUT_SIZE: usize = mem::size_of::<fuse_notify_poll_wakeup_out>();

#[derive(Debug, Serialize)]
#[allow(non_camel_case_types)]
pub struct fuse_notify_poll_wakeup_out {
    pub kh: u64,
}

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_fallocate_in {
    pub fh: u64,
    pub offset: u64,
    pub length: u64,
    pub mode: u32,
    _padding: u32,
}

pub const FUSE_IN_HEADER_SIZE: usize = mem::size_of::<fuse_in_header>();

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_in_header {
    pub len: u32,
    pub opcode: u32,
    pub unique: u64,
    pub nodeid: u64,
    pub uid: u32,
    pub gid: u32,
    pub pid: u32,
    _padding: u32,
}

pub const FUSE_OUT_HEADER_SIZE: usize = mem::size_of::<fuse_out_header>();

#[derive(Debug, Serialize)]
#[allow(non_camel_case_types)]
pub struct fuse_out_header {
    pub len: u32,
    pub error: i32,
    pub unique: u64,
}

pub const FUSE_DIRENT_SIZE: usize = mem::size_of::<fuse_dirent>();

#[derive(Debug, Serialize)]
#[allow(non_camel_case_types)]
pub struct fuse_dirent {
    pub ino: u64,
    pub off: u64,
    pub namelen: u32,
    pub r#type: u32,
    // followed by name of namelen bytes
}

pub const FUSE_DIRENTPLUS_SIZE: usize = mem::size_of::<fuse_direntplus>();

#[derive(Debug, Serialize)]
#[allow(non_camel_case_types)]
pub struct fuse_direntplus {
    pub entry_out: fuse_entry_out,
    pub dirent: fuse_dirent,
}

pub const FUSE_NOTIFY_INVAL_INODE_OUT_SIZE: usize = mem::size_of::<fuse_notify_inval_inode_out>();

#[derive(Debug, Serialize)]
#[allow(non_camel_case_types)]
pub struct fuse_notify_inval_inode_out {
    pub ino: u64,
    pub off: i64,
    pub len: i64,
}

pub const FUSE_NOTIFY_INVAL_ENTRY_OUT_SIZE: usize = mem::size_of::<fuse_notify_inval_entry_out>();

#[derive(Debug, Serialize)]
#[allow(non_camel_case_types)]
pub struct fuse_notify_inval_entry_out {
    pub parent: u64,
    pub namelen: u32,
    pub(crate) _padding: u32,
}

pub const FUSE_NOTIFY_DELETE_OUT_SIZE: usize = mem::size_of::<fuse_notify_delete_out>();

#[derive(Debug, Serialize)]
#[allow(non_camel_case_types)]
pub struct fuse_notify_delete_out {
    pub parent: u64,
    pub child: u64,
    pub namelen: u32,
    pub(crate) _padding: u32,
}

pub const FUSE_NOTIFY_STORE_OUT_SIZE: usize = mem::size_of::<fuse_notify_store_out>();

#[derive(Debug, Serialize)]
#[allow(non_camel_case_types)]
pub struct fuse_notify_store_out {
    pub nodeid: u64,
    pub offset: u64,
    pub size: u32,
    pub(crate) _padding: u32,
}

pub const FUSE_NOTIFY_RETRIEVE_OUT_SIZE: usize = mem::size_of::<fuse_notify_retrieve_out>();

#[derive(Debug, Serialize)]
#[allow(non_camel_case_types)]
pub struct fuse_notify_retrieve_out {
    pub notify_unique: u64,
    pub nodeid: u64,
    pub offset: u64,
    pub size: u32,
    pub(crate) _padding: u32,
}

pub const FUSE_NOTIFY_RETRIEVE_IN_SIZE: usize = mem::size_of::<fuse_notify_retrieve_in>();

#[derive(Debug, Deserialize)]
// matches the size of fuse_write_in
#[allow(non_camel_case_types)]
pub struct fuse_notify_retrieve_in {
    _dummy1: u64,
    pub offset: u64,
    pub size: u32,
    _dummy2: u32,
    _dummy3: u64,
    _dummy4: u64,
}

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_lseek_in {
    pub fh: u64,
    pub offset: u64,
    pub whence: u32,
    _padding: u32,
}

pub const FUSE_LSEEK_OUT_SIZE: usize = mem::size_of::<fuse_lseek_out>();

#[derive(Debug, Serialize)]
#[allow(non_camel_case_types)]
pub struct fuse_lseek_out {
    pub offset: u64,
}

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
pub struct fuse_copy_file_range_in {
    pub fh_in: u64,
    pub off_in: u64,
    pub nodeid_out: u64,
    pub fh_out: u64,
    pub off_out: u64,
    pub len: u64,
    pub flags: u64,
}

#[cfg(test)]
mod killpriv_abi_tests {
    use bincode::Options;

    use super::*;
    use crate::helper::get_bincode_config;

    /// `fuse_open_in.open_flags` is the uapi's second word (byte offset
    /// 4) — the kernel puts `FUSE_OPEN_KILL_SUIDGID` there on O_TRUNC
    /// opens by non-CAP_FSETID callers once `FUSE_HANDLE_KILLPRIV_V2` is
    /// negotiated. Pre-campaign the fork read it as `_unused`; this pins
    /// both the layout (size stays 8) and the decode.
    #[test]
    fn fuse_open_in_carries_open_flags_at_offset_4() {
        assert_eq!(
            mem::size_of::<fuse_open_in>(),
            8,
            "uapi fuse_open_in is 8 bytes"
        );
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(libc::O_TRUNC as u32 | libc::O_WRONLY as u32).to_le_bytes());
        bytes.extend_from_slice(&FUSE_OPEN_KILL_SUIDGID.to_le_bytes());
        let open_in: fuse_open_in = get_bincode_config()
            .deserialize(&bytes)
            .expect("8-byte fuse_open_in decodes");
        assert_eq!(
            open_in.flags,
            libc::O_TRUNC as u32 | libc::O_WRONLY as u32,
            "flags word unchanged at offset 0"
        );
        assert_eq!(
            open_in.open_flags, FUSE_OPEN_KILL_SUIDGID,
            "open_flags (FUSE_OPEN_KILL_SUIDGID) must decode from offset 4"
        );
    }

    /// The uapi bit values the killpriv-v2 contract rides on
    /// (include/uapi/linux/fuse.h): init bit 28, write flag bit 2, open
    /// flag bit 0, setattr valid bit 11. A silent renumbering here would
    /// negotiate one thing and honor another.
    #[test]
    fn killpriv_v2_uapi_bits_are_pinned() {
        assert_eq!(FUSE_HANDLE_KILLPRIV_V2, 1 << 28);
        assert_eq!(FUSE_WRITE_KILL_SUIDGID, 1 << 2);
        assert_eq!(FUSE_OPEN_KILL_SUIDGID, 1 << 0);
        assert_eq!(FATTR_KILL_SUIDGID, 1 << 11);
    }
}
