//! The `Filesystem` interface the daemon implements (`docs/porting/
//! fuse_abi_session.md` §6; `fuse_handlers_lanes.md` §5; charter §4).
//!
//! A vtable with one entry per opcode of the must-implement set and the
//! optional set; every entry has ONE shape — `fn (impl: *anyopaque, c: *Ctx,
//! args: <Op>Args) Error!<Op>Reply` — so an implementation is bound by
//! `Filesystem.of(T, ptr)`: for every entry `T` declares a same-named method
//! it is wrapped, the rest keep the default. Errors are the ONE typed set
//! (`core/errors.zig`); the lifecycle encodes each through the ONE errno
//! table (`-errno` header-only). The default of every optional op is
//! `error.NotImplemented` (ENOSYS): the kernel latches `fc->no_<op>` and
//! stops asking — for FSYNC/FLUSH/RELEASE that latch IS the elision the
//! daemon wants (the clean-handle `no_flush`), so no default lies `Ok`.
//!
//! Rung 3's first mount MUST implement: init, destroy, lookup, forget,
//! getattr, setattr, readlink, symlink, mknod, mkdir, unlink, rmdir, rename
//! (both opcodes land here with `flags`), link, open, read, write, flush,
//! release, fsync, opendir, readdir, readdirplus, releasedir, fsyncdir,
//! statfs, {set,get,list,remove}xattr, create, access (ENOSYS under
//! `default_permissions` is honest), fallocate (charter §6.6, every mode).
//! Rung 4 adds lseek, copy_file_range, ioctl, bmap, poll; rung 10 syncfs (the kernel sends it
//! only once INIT negotiated `FUSE_HAS_SYNCFS`; its ENOSYS default clears
//! `fc->sync_fs`). Never: getlk/setlk (POSIX_LOCKS is
//! never advertised — the kernel arbitrates), notify_reply (no RETRIEVE is
//! ever sent), interrupt (decoded, counted, ignored by the lifecycle: a hook
//! with no effect is dead code — ledger L3).
//!
//! Replies that carry bytes (READ, READLINK, GETXATTR/LISTXATTR data,
//! READDIR/READDIRPLUS) are written by the implementation straight into the
//! reply WINDOW the ctx carries (`Ctx.window`: the transport's ent payload
//! region, or the loopback's buffer) and return the byte count — no copy in
//! the codec, no allocation on the reply path (L5).

const std = @import("std");
const Io = std.Io;
const abi = @import("../abi.zig");
const codec = @import("../codec.zig");
const slot = @import("../transport/slot.zig");
const zc = @import("../transport/zc.zig");
const errors = @import("../../core/errors.zig");
const economy = @import("../../meta/economy.zig");
const op_guard = @import("op_guard.zig");
const op_trace = @import("op_trace.zig");
const watchdog = @import("watchdog.zig");
const write_admit = @import("../../data/write_admit.zig");

pub const Error = errors.Error;

/// The request's identity and the op's venue.
pub const Ctx = struct {
    unique: u64,
    uid: u32,
    gid: u32,
    pid: u32,
    io: Io,
    trace: *op_trace.OpTrace,
    /// The watchdog slot (null when the registry was exhausted).
    wd_slot: ?*watchdog.OpSlot,
    /// The reply window.
    window: []u8,
    /// The op's state across the move gate's re-issues (null outside a
    /// gate: a tool, a test calling the filesystem directly).
    move: ?*MoveAttempt = null,
    /// The request is this mount's kernel's (the lifecycle's dispatch) —
    /// or the times drain cadence's, whose stamps that kernel already
    /// holds through the fold: its caches follow the reply, so its own
    /// change is never invalidated back into it (decision 0003 §AB.2). A
    /// tool, a served verb, a test calling the filesystem directly: false.
    kernel: bool = false,
    /// The caller tells this mount's kernel of its change itself — the IPC
    /// host's handoffs (KD-11) — so its commits are `kernel_told`: never
    /// invalidated back, with the ack posture of a non-kernel origin.
    kernel_told: bool = false,

    /// Stamp a stage: the trace moves (a clock read only under profiling)
    /// and the watchdog station follows (one relaxed store).
    pub fn stage(self: *Ctx, s: op_trace.Stage) void {
        self.trace.stamp(s);
        if (self.wd_slot) |sl| sl.setStation(s);
    }
};

/// One op's state across the move gate's attempts (decision 0003 §X.2):
/// the cross-owner intent an attempt left OPEN at a slot move — the
/// re-issue resumes THAT intent (its id is the op id; the intent stays the
/// live op's, never the cadence's, between the attempts) — and the inos the
/// op is about, recorded by its first attempt: `subject` the child it
/// minted (a re-issue reuses it rather than minting another), the name's
/// child it removes or the inode it links or moves; `subject2` a rename's
/// replaced (or exchanged) destination. A resumed intent's reply and memo
/// updates are derived from them.
pub const MoveAttempt = struct {
    open: ?OpenIntent = null,
    subject: ?u64 = null,
    subject2: ?u64 = null,
    /// A write whose pieces its arms acked before the move refused its
    /// size claim (a foreign-slot file's shipped postlude): the re-issue
    /// re-runs the claim alone — the payload is never admitted twice.
    write_landed: ?WriteLanded = null,

    /// `size_epoch`: a foreign-slot write's holder size epoch, read off its
    /// token before its size (the claim carries it; 0 = none).
    pub const WriteLanded = struct { size_claim: u64, written: u64, size_epoch: u64 = 0 };

    pub const OpenIntent = struct {
        home: u64,
        tx_id: u64,
        /// The give-up: the intent is handed to the roll-forward cadence.
        abandon: *const fn (ctx: *anyopaque, tx_id: u64) void,
        ctx: *anyopaque,
    };

    /// The op ended without resuming its open intent (it answered another
    /// error, or the gate gave up): the cadence completes it.
    pub fn abandonOpen(self: *MoveAttempt) void {
        const oi = self.open orelse return;
        self.open = null;
        oi.abandon(oi.ctx, oi.tx_id);
    }
};

// ---------------------------------------------------------------------------
// Typed replies.
// ---------------------------------------------------------------------------

pub const Entry = codec.EntryReply;
pub const Attr = codec.AttrReply;
pub const Statfs = codec.Statfs;
pub const Opened = struct { fh: u64, open_flags: u32 };
pub const Created = struct { entry: Entry, fh: u64, open_flags: u32 };
/// `len` bytes written into `Ctx.window[0..len]`.
pub const Data = struct { len: usize };
pub const Written = struct { size: u32 };
pub const Xattr = union(enum) {
    /// The size probe (`size == 0` in the request): a SUCCESS reply with
    /// `fuse_getxattr_out{size}`.
    size: u32,
    /// `len` bytes in the window.
    data: usize,
};
pub const Lseek = struct { offset: u64 };
pub const Bmap = struct { block: u64 };
pub const Poll = struct { revents: u32 };
/// `data_len` bytes written into `Ctx.window[IOCTL_DATA_OFFSET..]`: the
/// reply is `fuse_ioctl_out ‖ data` and the window IS the ent's reply
/// payload, so the data sits where the commit lays it (no copy, no
/// aliasing move of the out struct over it).
pub const Ioctl = struct { out: abi.fuse_ioctl_out, data_len: usize };
pub const IOCTL_DATA_OFFSET: usize = @sizeOf(abi.fuse_ioctl_out);

// ---------------------------------------------------------------------------
// SETATTR's decoded request.
// ---------------------------------------------------------------------------

/// `fuse_setattr_in` decoded by its `valid` bits; `*_NOW` resolved against
/// `now_ns` (the coarse realtime stamp the mutation carries). Times are
/// i64-ns bit patterns in u64 (the inode value's form).
pub const SetAttr = struct {
    mode: ?u32 = null,
    uid: ?u32 = null,
    gid: ?u32 = null,
    size: ?u64 = null,
    atime: ?u64 = null,
    mtime: ?u64 = null,
    ctime: ?u64 = null,
    fh: ?u64 = null,
    lock_owner: ?u64 = null,
    /// `FATTR_KILL_SUIDGID`: the daemon owns the suid/sgid/caps clearing.
    kill_suidgid: bool = false,
    /// An atime or mtime set EXPLICITLY (the kernel's `ATTR_TIMES_SET`:
    /// `utimensat` with a value), as opposed to the `*_NOW` forms a
    /// `touch`/`utimes(NULL)` sends — the append-only law admits the
    /// latter alone (`inode_flags.setattrRefused`).
    times_set: bool = false,

    pub fn fromWire(in: abi.fuse_setattr_in, now_ns: u64) SetAttr {
        const v = in.valid;
        return .{
            .mode = if (v & abi.FATTR_MODE != 0) in.mode else null,
            .uid = if (v & abi.FATTR_UID != 0) in.uid else null,
            .gid = if (v & abi.FATTR_GID != 0) in.gid else null,
            .size = if (v & abi.FATTR_SIZE != 0) in.size else null,
            .atime = if (v & abi.FATTR_ATIME_NOW != 0) now_ns else if (v & abi.FATTR_ATIME != 0) timeNs(in.atime, in.atimensec) else null,
            .mtime = if (v & abi.FATTR_MTIME_NOW != 0) now_ns else if (v & abi.FATTR_MTIME != 0) timeNs(in.mtime, in.mtimensec) else null,
            .ctime = if (v & abi.FATTR_CTIME != 0) timeNs(in.ctime, in.ctimensec) else null,
            .fh = if (v & abi.FATTR_FH != 0) in.fh else null,
            .lock_owner = if (v & abi.FATTR_LOCKOWNER != 0) in.lock_owner else null,
            .kill_suidgid = v & abi.FATTR_KILL_SUIDGID != 0,
            .times_set = (v & abi.FATTR_ATIME != 0 and v & abi.FATTR_ATIME_NOW == 0) or (v & abi.FATTR_MTIME != 0 and v & abi.FATTR_MTIME_NOW == 0),
        };
    }

    /// The economy's request shape (the absorber + the field RMW).
    pub fn toEconomy(self: SetAttr) economy.SetattrReq {
        return .{ .mode = self.mode, .uid = self.uid, .gid = self.gid, .size = self.size, .atime = self.atime, .mtime = self.mtime, .ctime = self.ctime };
    }
};

/// `(time_t bit pattern, nsec)` → the i64-ns word, saturating (the kernel
/// clamps to the daemon's advertised range under FUSE_TIME_LIMITS; without
/// it a stamp past ±292 years saturates rather than wraps).
pub fn timeNs(sec: u64, nsec: u32) u64 {
    const s: i64 = @bitCast(sec);
    return @bitCast(s *| @as(i64, std.time.ns_per_s) +| @as(i64, nsec));
}

// ---------------------------------------------------------------------------
// Per-op argument records (the nodeid of the request is the op's ino).
// ---------------------------------------------------------------------------

/// What INIT negotiated: the folded capability word the kernel will apply
/// and the protocol minor (the two facts a filesystem's posture keys on).
pub const InitArgs = struct {
    negotiated: u64,
    minor: u32,
    /// The no-open posture (decision 0003 §AF): OPEN answers ENOSYS before
    /// the filesystem sees it, a CREATE's reply must leave the kernel a
    /// FLUSH at every close (no `FOPEN_NOFLUSH`), and FLUSH carries the
    /// write-back drain RELEASE carried.
    no_open: bool = false,
    /// The write-back timer (decision 0003 §AJ): close is no durability
    /// point — every regular open and CREATE reply carries `FOPEN_NOFLUSH`
    /// (in either open posture) and RELEASE drains nothing; the mount's
    /// timer lands custody within `T_wb`.
    writeback_timer: bool = false,

    pub fn has(self: InitArgs, bit: u64) bool {
        return self.negotiated & bit != 0;
    }
};
pub const DestroyArgs = struct {};
pub const LookupArgs = struct { parent: u64, name: []const u8 };
/// `evicted`: the open table's verdict — the kernel's last reference went
/// (the filesystem invalidates its caches and queues the reclaim iff the
/// ino is not open); a partial forget hands the count through unchanged.
pub const ForgetArgs = struct { ino: u64, nlookup: u64, evicted: bool };
pub const GetattrArgs = struct { ino: u64, fh: ?u64 };
pub const SetattrArgs = struct { ino: u64, attr: SetAttr };
pub const ReadlinkArgs = struct { ino: u64 };
pub const SymlinkArgs = struct { parent: u64, name: []const u8, link: []const u8 };
pub const MknodArgs = struct { parent: u64, name: []const u8, mode: u32, rdev: u32, umask: u32 };
pub const MkdirArgs = struct { parent: u64, name: []const u8, mode: u32, umask: u32 };
pub const UnlinkArgs = struct { parent: u64, name: []const u8 };
pub const RenameArgs = struct { parent: u64, name: []const u8, newparent: u64, newname: []const u8, flags: u32 };
pub const LinkArgs = struct { ino: u64, newparent: u64, newname: []const u8 };
pub const OpenArgs = struct { ino: u64, flags: u32, open_flags: u32 };
/// `O_TRUNC` / `O_DIRECT` as the kernel's open-flag word carries them (the
/// lifecycle's guard choice and the filesystem's arms read the same bits).
pub const O_TRUNC: u32 = @bitCast(std.os.linux.O{ .TRUNC = true });
pub const O_DIRECT: u32 = @bitCast(std.os.linux.O{ .DIRECT = true });
pub const ReadArgs = struct { ino: u64, fh: u64, offset: u64, size: u32, flags: u32 };
/// `data` is the payload in hand (empty when `held`); `held` is a
/// zero-copy payload still sitting in the registered slot (the handler
/// consumes it by its source coordinates — `WRITE_FIXED(device ← slot)` —
/// or extracts it lazily, exactly once, before the lease ends); `placed`
/// is a ring write's placed sever (its bytes ARE `data`, already in a
/// shared assembly the striped arm adopts or copies from — charter §7);
/// `class` is the level-1 admission's verdict (charter §6.2 shared-mode
/// admission).
/// `shared_size`: with `class == .shared`, the claimed size of the EXACT
/// resident striped-passthrough snapshot the admission classified on (the
/// write's end is within it) — the filesystem's striped arm runs off it
/// with no layout lock and no KV read of the kind or the inode; null = the
/// snapshot was not exact (a transformed set) and the arm re-derives.
pub const WriteArgs = struct { ino: u64, fh: u64, offset: u64, data: []const u8, held: ?HeldPayload = null, placed: ?write_admit.PlacedPayload = null, class: op_guard.Class = .entire_op, shared_size: ?u64 = null, write_flags: u32, flags: u32 };

/// A HELD zero-copy WRITE payload (portable face of `slot.PayloadLease`):
/// the slot source for a direct device store and the memoized extraction.
pub const HeldPayload = struct {
    src: slot.HeldSource,
    class: zc.PayloadClass,
    ctx: ?*anyopaque,
    extract: *const fn (ctx: ?*anyopaque, io: Io) anyerror![]const u8,
};
/// `drain`: the no-open posture's FLUSH — the write-back drain at close
/// that RELEASE carries in the control arm (charter §6.2).
pub const FlushArgs = struct { ino: u64, fh: u64, lock_owner: u64, drain: bool = false };
pub const ReleaseArgs = struct { ino: u64, fh: u64, flags: u32, lock_owner: u64, flush: bool };
pub const FsyncArgs = struct { ino: u64, fh: u64, datasync: bool };
/// `ino` = the request's nodeid (the root's): the op names the mount.
pub const SyncfsArgs = struct { ino: u64 };
pub const ReaddirArgs = struct { ino: u64, fh: u64, offset: u64, size: u32 };
pub const StatfsArgs = struct { ino: u64 };
pub const SetxattrArgs = struct { ino: u64, name: []const u8, value: []const u8, flags: u32 };
pub const GetxattrArgs = struct { ino: u64, name: []const u8, size: u32 };
pub const ListxattrArgs = struct { ino: u64, size: u32 };
pub const RemovexattrArgs = struct { ino: u64, name: []const u8 };
pub const AccessArgs = struct { ino: u64, mask: u32 };
pub const CreateArgs = struct { parent: u64, name: []const u8, mode: u32, flags: u32, umask: u32, open_flags: u32 };
pub const BmapArgs = struct { ino: u64, blocksize: u32, block: u64 };
pub const IoctlArgs = struct { ino: u64, fh: u64, flags: u32, cmd: u32, arg: u64, in: []const u8, out_size: u32 };
pub const PollArgs = struct { ino: u64, fh: u64, kh: u64, flags: u32, events: u32 };
pub const FallocateArgs = struct { ino: u64, fh: u64, offset: u64, length: u64, mode: u32 };
pub const LseekArgs = struct { ino: u64, fh: u64, offset: u64, whence: u32 };
pub const CopyFileRangeArgs = struct { ino_in: u64, fh_in: u64, off_in: u64, ino_out: u64, fh_out: u64, off_out: u64, len: u64, flags: u64 };

fn Op(comptime Args: type, comptime Reply: type) type {
    return *const fn (*anyopaque, *Ctx, Args) Error!Reply;
}

/// The vtable: every entry defaults to ENOSYS except the three with no
/// reply shape (init, destroy, forget), which default to no-ops.
pub const VTable = struct {
    init: *const fn (*anyopaque, *Ctx, InitArgs) Error!void = &defaults.init,
    destroy: *const fn (*anyopaque, *Ctx, DestroyArgs) void = &defaults.destroy,
    forget: *const fn (*anyopaque, *Ctx, ForgetArgs) void = &defaults.forget,
    lookup: Op(LookupArgs, Entry) = &defaults.enosys(LookupArgs, Entry),
    getattr: Op(GetattrArgs, Attr) = &defaults.enosys(GetattrArgs, Attr),
    setattr: Op(SetattrArgs, Attr) = &defaults.enosys(SetattrArgs, Attr),
    readlink: Op(ReadlinkArgs, Data) = &defaults.enosys(ReadlinkArgs, Data),
    symlink: Op(SymlinkArgs, Entry) = &defaults.enosys(SymlinkArgs, Entry),
    mknod: Op(MknodArgs, Entry) = &defaults.enosys(MknodArgs, Entry),
    mkdir: Op(MkdirArgs, Entry) = &defaults.enosys(MkdirArgs, Entry),
    unlink: Op(UnlinkArgs, void) = &defaults.enosys(UnlinkArgs, void),
    rmdir: Op(UnlinkArgs, void) = &defaults.enosys(UnlinkArgs, void),
    rename: Op(RenameArgs, void) = &defaults.enosys(RenameArgs, void),
    link: Op(LinkArgs, Entry) = &defaults.enosys(LinkArgs, Entry),
    open: Op(OpenArgs, Opened) = &defaults.enosys(OpenArgs, Opened),
    read: Op(ReadArgs, Data) = &defaults.enosys(ReadArgs, Data),
    write: Op(WriteArgs, Written) = &defaults.enosys(WriteArgs, Written),
    flush: Op(FlushArgs, void) = &defaults.enosys(FlushArgs, void),
    release: Op(ReleaseArgs, void) = &defaults.enosys(ReleaseArgs, void),
    fsync: Op(FsyncArgs, void) = &defaults.enosys(FsyncArgs, void),
    opendir: Op(OpenArgs, Opened) = &defaults.enosys(OpenArgs, Opened),
    readdir: Op(ReaddirArgs, Data) = &defaults.enosys(ReaddirArgs, Data),
    readdirplus: Op(ReaddirArgs, Data) = &defaults.enosys(ReaddirArgs, Data),
    releasedir: Op(ReleaseArgs, void) = &defaults.enosys(ReleaseArgs, void),
    fsyncdir: Op(FsyncArgs, void) = &defaults.enosys(FsyncArgs, void),
    statfs: Op(StatfsArgs, Statfs) = &defaults.enosys(StatfsArgs, Statfs),
    setxattr: Op(SetxattrArgs, void) = &defaults.enosys(SetxattrArgs, void),
    getxattr: Op(GetxattrArgs, Xattr) = &defaults.enosys(GetxattrArgs, Xattr),
    listxattr: Op(ListxattrArgs, Xattr) = &defaults.enosys(ListxattrArgs, Xattr),
    removexattr: Op(RemovexattrArgs, void) = &defaults.enosys(RemovexattrArgs, void),
    access: Op(AccessArgs, void) = &defaults.enosys(AccessArgs, void),
    create: Op(CreateArgs, Created) = &defaults.enosys(CreateArgs, Created),
    bmap: Op(BmapArgs, Bmap) = &defaults.enosys(BmapArgs, Bmap),
    ioctl: Op(IoctlArgs, Ioctl) = &defaults.enosys(IoctlArgs, Ioctl),
    poll: Op(PollArgs, Poll) = &defaults.enosys(PollArgs, Poll),
    fallocate: Op(FallocateArgs, void) = &defaults.enosys(FallocateArgs, void),
    lseek: Op(LseekArgs, Lseek) = &defaults.enosys(LseekArgs, Lseek),
    copy_file_range: Op(CopyFileRangeArgs, Written) = &defaults.enosys(CopyFileRangeArgs, Written),
    syncfs: Op(SyncfsArgs, void) = &defaults.enosys(SyncfsArgs, void),
    /// The try-only twins of `lookup` / `getattr` the FUSE drain worker asks
    /// first (decision 0003 §AB.1): answer from RAM or fail — any error,
    /// `WouldBlock` above all, sends the request to the lane, which asks
    /// the blocking op. Never a device touch, a wire verb or a park.
    probe_lookup: Op(LookupArgs, Entry) = &defaults.enosys(LookupArgs, Entry),
    probe_getattr: Op(GetattrArgs, Attr) = &defaults.enosys(GetattrArgs, Attr),
    /// The no-open FLUSH's try-only twin (decision 0003 §AF): success iff
    /// the ino holds no write custody the drain would land — read under
    /// the custody tables' O(1)-section spin locks, never a device wait, a
    /// park or a lock a pass holds; any error sends the FLUSH to the data
    /// lane, which drains.
    probe_flush: Op(FlushArgs, void) = &defaults.enosys(FlushArgs, void),
    /// Not an opcode: a READ of `ino` served by a path that never reaches
    /// `read` (the interception host's ring fast path, charter §7) tells
    /// the filesystem so the mount's atime posture applies to it too
    /// (charter §4 "Access time": the law is the file's, not the path's).
    /// Never blocks, never fails; the default notes nothing.
    note_read: *const fn (*anyopaque, *Ctx, NoteReadArgs) void = &defaults.note_read,
};

pub const NoteReadArgs = struct { ino: u64 };

const defaults = struct {
    fn init(_: *anyopaque, _: *Ctx, _: InitArgs) Error!void {}
    fn destroy(_: *anyopaque, _: *Ctx, _: DestroyArgs) void {}
    fn forget(_: *anyopaque, _: *Ctx, _: ForgetArgs) void {}
    fn note_read(_: *anyopaque, _: *Ctx, _: NoteReadArgs) void {}
    fn enosys(comptime Args: type, comptime Reply: type) fn (*anyopaque, *Ctx, Args) Error!Reply {
        return struct {
            fn f(_: *anyopaque, _: *Ctx, _: Args) Error!Reply {
                return error.NotImplemented;
            }
        }.f;
    }
};

/// The bound interface: a type-erased implementation and its vtable.
pub const Filesystem = struct {
    impl: *anyopaque,
    vtable: *const VTable,
    /// The filesystem serves the kernel's export operations — `LOOKUP(nodeid,
    /// ".")` for any inode kind and `LOOKUP(dir, "..")` from a durable parent
    /// — so INIT may advertise `FUSE_EXPORT_SUPPORT` (charter §4). Declared
    /// by `T` as `pub const export_support = true`; a wrapper forwards its
    /// inner's. Default false: a filesystem that never said so is never
    /// advertised (the kernel would answer ESTALE on its own otherwise).
    export_support: bool = false,

    /// Bind `T`: every vtable entry `T` declares as `pub fn <name>(self: *T,
    /// c: *Ctx, args: <Op>Args) Error!<Op>Reply` is wrapped; the rest
    /// default. A declared method of the wrong shape is a compile error at
    /// the wrapper.
    pub fn of(comptime T: type, impl: *T) Filesystem {
        const Holder = struct {
            const vt: VTable = build();

            fn build() VTable {
                var v: VTable = .{};
                const info = @typeInfo(VTable).@"struct";
                inline for (info.field_names, info.field_types) |name, FnPtr| {
                    if (@hasDecl(T, name)) @field(v, name) = &Thunk(T, name, FnPtr).call;
                }
                return v;
            }
        };
        return .{ .impl = impl, .vtable = &Holder.vt, .export_support = @hasDecl(T, "export_support") and T.export_support };
    }

    fn Thunk(comptime T: type, comptime name: []const u8, comptime FnPtr: type) type {
        const Fn = @typeInfo(FnPtr).pointer.child;
        const info = @typeInfo(Fn).@"fn";
        const Args = info.param_types[2].?;
        const R = info.return_type.?;
        return struct {
            fn call(impl: *anyopaque, c: *Ctx, args: Args) R {
                const self: *T = @ptrCast(@alignCast(impl));
                return @field(T, name)(self, c, args);
            }
        };
    }

    /// Whether `T` provides every op of rung 3's must-implement set (a
    /// mount-time check, so a missing op refuses loud instead of answering
    /// ENOSYS to the first user).
    pub fn implementsMustSet(comptime T: type) bool {
        inline for (must_implement) |name| {
            if (!@hasDecl(T, name)) return false;
        }
        return true;
    }

    pub const must_implement = [_][]const u8{
        "init",      "destroy",     "lookup",     "forget",   "getattr", "setattr",  "readlink",
        "symlink",   "mknod",       "mkdir",      "unlink",   "rmdir",   "rename",   "link",
        "open",      "read",        "write",      "flush",    "release", "fsync",    "opendir",
        "readdir",   "readdirplus", "releasedir", "fsyncdir", "statfs",  "setxattr", "getxattr",
        "listxattr", "removexattr", "create",
    };
};

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

const testing = std.testing;

const Minimal = struct {
    lookups: u32 = 0,
    destroyed: bool = false,

    pub fn lookup(self: *Minimal, c: *Ctx, a: LookupArgs) Error!Entry {
        self.lookups += 1;
        c.stage(.meta_backend_start);
        if (a.name.len == 0) return error.InvalidArgument;
        return .{ .nodeid = 2, .entry_ttl_ns = 1, .attr_ttl_ns = 2, .attr = abi.zeroed(abi.fuse_attr) };
    }

    pub fn destroy(self: *Minimal, _: *Ctx, _: DestroyArgs) void {
        self.destroyed = true;
    }
};

fn testCtx(trace: *op_trace.OpTrace, window: []u8) Ctx {
    return .{ .unique = 1, .uid = 0, .gid = 0, .pid = 0, .io = testing.io, .trace = trace, .wd_slot = null, .window = window };
}

test "binding: a declared op is wrapped, an undeclared one answers ENOSYS, destroy/forget/init default to no-ops" {
    var m: Minimal = .{};
    const fs = Filesystem.of(Minimal, &m);
    var tr = op_trace.OpTrace.begin(1, .lookup);
    var win: [16]u8 = undefined;
    var c = testCtx(&tr, &win);
    const e = try fs.vtable.lookup(fs.impl, &c, .{ .parent = 1, .name = "x" });
    try testing.expectEqual(@as(u64, 2), e.nodeid);
    try testing.expectEqual(@as(u32, 1), m.lookups);
    try testing.expectEqual(op_trace.Stage.meta_backend_start, tr.last_stage);
    try testing.expectError(error.InvalidArgument, fs.vtable.lookup(fs.impl, &c, .{ .parent = 1, .name = "" }));
    try testing.expectError(error.NotImplemented, fs.vtable.getattr(fs.impl, &c, .{ .ino = 1, .fh = null }));
    try testing.expectError(error.NotImplemented, fs.vtable.fsync(fs.impl, &c, .{ .ino = 1, .fh = 0, .datasync = false }));
    try testing.expectError(error.NotImplemented, fs.vtable.copy_file_range(fs.impl, &c, .{ .ino_in = 1, .fh_in = 0, .off_in = 0, .ino_out = 2, .fh_out = 0, .off_out = 0, .len = 1, .flags = 0 }));
    try testing.expectError(error.NotImplemented, fs.vtable.syncfs(fs.impl, &c, .{ .ino = 1 }));
    try fs.vtable.init(fs.impl, &c, .{ .negotiated = 0, .minor = abi.FUSE_KERNEL_MINOR_VERSION });
    fs.vtable.forget(fs.impl, &c, .{ .ino = 5, .nlookup = 1, .evicted = true });
    fs.vtable.destroy(fs.impl, &c, .{});
    try testing.expect(m.destroyed);
    try testing.expect(!Filesystem.implementsMustSet(Minimal));
    tr.finish();
}

test "SetAttr decodes by valid bits; *_NOW resolves to now; times are ns-exact and saturating" {
    const now: u64 = 1_700_000_000 * std.time.ns_per_s + 5;
    var in = abi.zeroed(abi.fuse_setattr_in);
    in.valid = abi.FATTR_MODE | abi.FATTR_SIZE | abi.FATTR_ATIME_NOW | abi.FATTR_MTIME | abi.FATTR_CTIME | abi.FATTR_KILL_SUIDGID | abi.FATTR_FH;
    in.mode = 0o644;
    in.size = 4096;
    in.mtime = 1_600_000_000;
    in.mtimensec = 123;
    in.ctime = @bitCast(@as(i64, -1));
    in.ctimensec = 999_999_999;
    in.fh = 77;
    const s = SetAttr.fromWire(in, now);
    try testing.expectEqual(@as(?u32, 0o644), s.mode);
    try testing.expectEqual(@as(?u32, null), s.uid);
    try testing.expectEqual(@as(?u64, 4096), s.size);
    try testing.expectEqual(@as(?u64, now), s.atime);
    try testing.expectEqual(@as(?u64, 1_600_000_000 * std.time.ns_per_s + 123), s.mtime);
    // −1 s + 999 999 999 ns = −1 ns.
    try testing.expectEqual(@as(i64, -1), @as(i64, @bitCast(s.ctime.?)));
    try testing.expectEqual(@as(?u64, 77), s.fh);
    try testing.expect(s.kill_suidgid);
    try testing.expect(!s.toEconomy().timesOnly());
    // An explicit mtime beside a NOW atime is a set time (ATTR_TIMES_SET).
    try testing.expect(s.times_set);
    // Saturation: a time_t past the i64-ns range never wraps.
    try testing.expectEqual(@as(i64, std.math.maxInt(i64)), @as(i64, @bitCast(timeNs(1 << 40, 0))));
    const echo = SetAttr.fromWire(.{ .valid = abi.FATTR_MTIME | abi.FATTR_CTIME, .padding = 0, .fh = 0, .size = 0, .lock_owner = 0, .atime = 0, .mtime = 1, .ctime = 2, .atimensec = 0, .mtimensec = 0, .ctimensec = 0, .mode = 0, .unused4 = 0, .uid = 0, .gid = 0, .unused5 = 0 }, now);
    try testing.expect(echo.toEconomy().timesOnly());
    try testing.expect(echo.times_set);
    // `touch`: both times NOW — not a set time; a ctime echo alone neither.
    var touch = abi.zeroed(abi.fuse_setattr_in);
    touch.valid = abi.FATTR_ATIME | abi.FATTR_ATIME_NOW | abi.FATTR_MTIME | abi.FATTR_MTIME_NOW;
    try testing.expect(!SetAttr.fromWire(touch, now).times_set);
    touch.valid = abi.FATTR_CTIME;
    try testing.expect(!SetAttr.fromWire(touch, now).times_set);
}
