//! The SDK session (charter §7; spec `docs/porting/il_sdk.md` §1, §3–§5):
//! the second client kind over THE ONE client library (`ipc/client.zig`),
//! establishing as kind SDK with the two-part ABI word's WIRE part and the
//! geometry request, binding files with the strengthen-only durability
//! word, running positional ops on leased arena buffers in place (no slab
//! copy in either direction), the io_uring-shaped batch API over tickets,
//! the heartbeat/deadline poison that turns every later call into
//! `-ENOTCONN`, and the fork/exec lifecycle. Linux only.
//!
//! What is deliberately ABSENT (the interposition machinery the shim pays
//! for): no fd table, no offset mirror, no `dup` refcounts, no TLS
//! reentrancy guard, no libaio emulation — and no silent POSIX fallback:
//! this is a first-party API, so every refusal is EXPLICIT (a NULL + the
//! reason, a negative errno), never a quietly slower op (decision 0002,
//! the SDK rows). Raw `std.os.linux` only: the SDK may share a process
//! with the LD_PRELOAD shim, whose interposers `std.posix` would resolve
//! to.
//!
//! Durability (§5): a handle's class is `max(requested, O_SYNC|O_DSYNC
//! implied)` and is sent in BIND; the daemon's grant may only be ≥ it
//! (strengthen-only — a per-handle weakener would make the mount's stated
//! crash-exposure cadence a function of which app opened the file); the
//! enum has exactly two values, so no weakener is expressible. `fsync` is
//! the real `fsync(2)` in every class. A SYNC binding's write barriers
//! before DONE daemon-side, so its CQE already implies durability.

const std = @import("std");
const checksum = @import("../core/checksum.zig");
const linux = std.os.linux;
const Atomic = std.atomic.Value;
const assert = std.debug.assert;

const client = @import("../ipc/client.zig");
const wire = @import("../ipc/wire.zig");
const layout = @import("../ipc/layout.zig");
const doorbell = @import("../ipc/doorbell.zig");
const rendezvous = @import("../ipc/rendezvous.zig");
const knobs = @import("../core/knobs.zig");
const clock = @import("../core/clock.zig");
const lease = @import("buffer_lease.zig");
const version = @import("version.zig");

pub const Buf = lease.Buf;
pub const Durability = wire.Durability;

/// Descriptor flag bit 0: client-asserted, display-only — the window is an
/// app buffer, not a slot slab (the daemon validates either the same way).
pub const F_ARENA_NATIVE: u32 = 1;
/// `statfs.f_type` of a FUSE mount (not in std).
pub const FUSE_SUPER_MAGIC: i64 = 0x65735546;
/// Flights staged under one doorbell per round of a chunked op: the client
/// library's bound (64 × the 1 MiB default window = the default arena).
pub const MAX_FLIGHTS: usize = client.MAX_FLIGHTS;
/// Lengths above `i32` cannot be a CQE result on the wire.
pub const OP_LEN_MAX: u64 = std.math.maxInt(i32);
/// The once-per-(mount, reason) ledger's size: a process rarely opens more
/// than a handful of mounts; past it the line prints again (a repeat,
/// never a lost line).
const ONCE_LEDGER = 32;

fn neg(e: linux.E) i64 {
    return -@as(i64, @backingInt(e));
}

const kernel_statfs = extern struct {
    f_type: i64,
    f_bsize: i64,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    f_files: u64,
    f_ffree: u64,
    f_fsid: [2]i32,
    f_namelen: i64,
    f_frsize: i64,
    f_flags: i64,
    f_spare: [4]i64,
};

comptime {
    // The 64-bit kernel `struct statfs` the syscall fills.
    assert(@sizeOf(kernel_statfs) == 120);
}

// ---------------------------------------------------------------------------
// The thread-local last error of the constructors (`sqz_session_open`,
// `sqz_open`, `sqz_buf_alloc` return NULL): the negative errno and the
// reason line. Never `errno(3)`.
// ---------------------------------------------------------------------------

threadlocal var last_error: i32 = 0;
/// NUL-terminated inside its width so the C face hands out the buffer.
threadlocal var last_text_buf: [version.TEXT_MAX + 1]u8 = @splat(0);
threadlocal var last_text_len: usize = 0;

pub fn setLastError(e: linux.E, text: []const u8) void {
    last_error = -@as(i32, @backingInt(e));
    const n = @min(text.len, version.TEXT_MAX);
    @memcpy(last_text_buf[0..n], text[0..n]);
    last_text_buf[n] = 0;
    last_text_len = n;
}

pub fn lastError() i32 {
    return last_error;
}

pub fn lastErrorText() []const u8 {
    return last_text_buf[0..last_text_len];
}

pub fn lastErrorTextZ() [*:0]const u8 {
    return last_text_buf[0..last_text_len :0];
}

// ---------------------------------------------------------------------------
// The process registry: every live session, for the atfork child handler
// (the child's sessions are per process: it poisons and `close(2)`s its
// socket COPIES — never `shutdown`, which would sever the parent's) and
// for the once-per-(mount, reason) refusal ledger.
// ---------------------------------------------------------------------------

var registry_lock: client.CtlLock = .{};
var registry_head: ?*Session = null;
var atfork_registered: Atomic(bool) = .init(false);
var once_ledger: [ONCE_LEDGER]struct { dev: u64, tag: u8 } = undefined;
var once_len: usize = 0;

fn atforkPrepare() callconv(.c) void {
    registry_lock.lock();
}

fn atforkParent() callconv(.c) void {
    registry_lock.unlock();
}

/// Async-signal-safe: stores, CASes and `close(2)` only. Every lock a
/// parent thread may have held at the fork is RESET (its holder does not
/// exist in the child), and the SDK's own poison bookkeeping is marked done
/// — the child never submitted the parent's ops, so it owes no `-EIO`
/// CQEs; every call of its answers `-ENOTCONN`.
fn atforkChild() callconv(.c) void {
    var s = registry_head;
    while (s) |session| : (s = session.next) {
        session.cl.poisonChild();
        _ = session.arena.poisonAll();
        session.poisoned_once.store(true, .release);
        session.poison_cause.store(@backingInt(PoisonCause.fork), .release);
        session.batch_lock.word.store(0, .release);
        session.files_lock.word.store(0, .release);
        session.cl.ctl_lock.word.store(0, .release);
        session.arena.lock.unlock();
    }
    registry_lock.word.store(0, .release);
}

/// `dladdr(3)`'s record (absent from `std.c` on Linux).
const DlInfo = extern struct {
    fname: ?[*:0]const u8,
    fbase: ?*anyopaque,
    sname: ?[*:0]const u8,
    saddr: ?*anyopaque,
};
extern "c" fn dladdr(addr: ?*const anyopaque, info: *DlInfo) c_int;

/// Unload safety: an atfork handler can never be unregistered, so the
/// object holding it must never unload. `std.Build` has no `-z nodelete`
/// field, so the pin is made at runtime: re-`dlopen` the object this code
/// lives in with `RTLD_NOLOAD | RTLD_NODELETE`, which glibc applies to the
/// already-loaded link map and makes every later `dlclose` a no-op for it.
/// Statically linked into the executable there is nothing to unload;
/// `dladdr`/`dlopen` then answer the main program or fail, both harmless.
/// Returns whether a pin was placed.
pub fn pinSelf() bool {
    var info: DlInfo = undefined;
    if (dladdr(@ptrCast(&pinSelf), &info) == 0) return false;
    const name = info.fname orelse return false;
    if (name[0] == 0) return false;
    return std.c.dlopen(name, .{ .NOW = true, .NOLOAD = true, .NODELETE = true }) != null;
}

fn registerAtforkOnce() void {
    if (atfork_registered.swap(true, .acq_rel)) return;
    _ = std.c.pthread_atfork(&atforkPrepare, &atforkParent, &atforkChild);
    _ = pinSelf();
}

fn register(s: *Session) void {
    registry_lock.lock();
    defer registry_lock.unlock();
    s.next = registry_head;
    s.prev = null;
    if (registry_head) |h| h.prev = s;
    registry_head = s;
}

fn unregister(s: *Session) void {
    registry_lock.lock();
    defer registry_lock.unlock();
    if (s.prev) |p| p.next = s.next else registry_head = s.next;
    if (s.next) |n| n.prev = s.prev;
}

/// The refusal line, once per `(mount device, reason)` on stderr with the
/// `squeezefs-sdk:` prefix (the shim's `RefusalOnce` lineage), and always
/// into the thread-local last error.
fn refuse(dev: u64, reason: version.Reason) void {
    var buf: [version.TEXT_MAX]u8 = undefined;
    const text = version.text(&buf, reason);
    setLastError(reason.errno(), text);
    registry_lock.lock();
    defer registry_lock.unlock();
    for (once_ledger[0..once_len]) |e| if (e.dev == dev and e.tag == reason.tag()) return;
    if (once_len < ONCE_LEDGER) {
        once_ledger[once_len] = .{ .dev = dev, .tag = reason.tag() };
        once_len += 1;
    }
    var line: [version.TEXT_MAX + 32]u8 = undefined;
    const l = std.fmt.bufPrint(&line, "squeezefs-sdk: {s}\n", .{text}) catch return;
    _ = linux.write(2, l.ptr, l.len);
}

/// How many refusal lines have been printed (tests).
pub fn refusalLinesPrinted() usize {
    registry_lock.lock();
    defer registry_lock.unlock();
    return once_len;
}

// ---------------------------------------------------------------------------
// The ABI records the C header mirrors.
// ---------------------------------------------------------------------------

pub const SessionOpts = extern struct {
    struct_size: u32,
    /// The slot request (0 = default); clamped to the ring.
    slots: u32,
    /// The arena REQUEST (0 = the daemon's default); clamped under the
    /// cap, never refused.
    arena_bytes: u64,
    /// Per-op completion deadline (0 = 30 s); a miss poisons the session.
    op_timeout_ms: u32,
    _reserved: u32 = 0,
};

pub const OpenOpts = extern struct {
    struct_size: u32,
    /// `Durability` as a word: 0 = MOUNT, 1 = SYNC; anything else -EINVAL.
    durability: u32,
};

pub const IoVec = extern struct {
    buf: ?*anyopaque,
    buf_off: u64,
    len: u64,
};

pub const Opcode = enum(u8) { pread = 1, pwrite = 2 };

pub const Sqe = extern struct {
    opcode: u8,
    flags: u8 = 0,
    _pad: u16 = 0,
    len: u32,
    offset: u64,
    file: ?*anyopaque,
    buf: ?*anyopaque,
    buf_off: u64,
    /// Client-private: never crosses the wire (the slot has no such field;
    /// nothing in shm is trusted).
    user_data: u64,
};

pub const Cqe = extern struct {
    user_data: u64,
    /// Bytes or `-errno`.
    res: i64,
};

pub const Stat = extern struct {
    dev: u64,
    ino: u64,
    size: u64,
    blocks: u64,
    blksize: u32,
    nlink: u32,
    mode: u32,
    uid: u32,
    gid: u32,
    _pad: u32 = 0,
    atime_sec: i64,
    mtime_sec: i64,
    ctime_sec: i64,
    atime_nsec: u32,
    mtime_nsec: u32,
    ctime_nsec: u32,
    _pad2: u32 = 0,
};

comptime {
    assert(@sizeOf(SessionOpts) == 24);
    assert(@sizeOf(OpenOpts) == 8);
    assert(@sizeOf(IoVec) == 24);
    assert(@sizeOf(Sqe) == 48);
    assert(@offsetOf(Sqe, "len") == 4);
    assert(@offsetOf(Sqe, "offset") == 8);
    assert(@offsetOf(Sqe, "file") == 16);
    assert(@offsetOf(Sqe, "buf") == 24);
    assert(@offsetOf(Sqe, "buf_off") == 32);
    assert(@offsetOf(Sqe, "user_data") == 40);
    assert(@sizeOf(Cqe) == 16);
    assert(@sizeOf(Stat) == 96);
}

// ---------------------------------------------------------------------------
// Files.
// ---------------------------------------------------------------------------

pub const File = struct {
    session: *Session,
    fd: linux.fd_t,
    binding: client.Binding,
    /// The effective class: `max(requested, O_SYNC-implied)`, granted.
    durability: Durability,
    /// Ops SUBMITTED against this handle (close refuses `-EBUSY`).
    inflight: Atomic(u32) = .init(0),
    live: bool = true,
    next: ?*File = null,
    prev: ?*File = null,
};

// ---------------------------------------------------------------------------
// The batch bookkeeping: one op record per SQE, one flight per slot (the
// ticket IS the slot), a ready ring of folded CQEs. All sized by the
// granted slot count; allocation-free per op.
// ---------------------------------------------------------------------------

const NONE: u32 = std.math.maxInt(u32);

const Op = struct {
    user_data: u64 = 0,
    buf: ?*Buf = null,
    file: ?*File = null,
    /// The flight chain in offset order.
    head: u32 = NONE,
    tail: u32 = NONE,
    /// Flights not yet DONE.
    remaining: u32 = 0,
    submitted_ns: u64 = 0,
    live: bool = false,
    next_free: u32 = NONE,
};

const Flight = struct {
    op: u32 = NONE,
    generation: u64 = 0,
    len: u32 = 0,
    res: ?i64 = null,
    next: u32 = NONE,
    live: bool = false,
};

pub const PoisonCause = enum(u8) { none = 0, deadline = 1, heartbeat = 2, generation = 3, ring = 4, fork = 5 };

pub const Session = struct {
    gpa: std.mem.Allocator,
    cl: client.Session,
    arena: lease.Arena,
    mount_dev: u64,
    op_timeout_ns: u64,
    // ---- files ----
    files_lock: client.CtlLock = .{},
    files_head: ?*File = null,
    open_files: Atomic(u32) = .init(0),
    // ---- the batch API ----
    batch_lock: client.CtlLock = .{},
    ops: []Op,
    op_free: u32,
    flights: []Flight,
    ready: []Cqe,
    ready_head: u32 = 0,
    ready_len: u32 = 0,
    live_ops: u32 = 0,
    // ---- poison ----
    poisoned_once: Atomic(bool) = .init(false),
    poison_cause: Atomic(u8) = .init(0),
    // ---- the `sdk_*` census (display-only, plain atomics) ----
    sdk_ops_sync: Atomic(u64) = .init(0),
    sdk_ops_batch: Atomic(u64) = .init(0),
    sdk_arena_native_writes: Atomic(u64) = .init(0),
    sdk_arena_native_reads: Atomic(u64) = .init(0),
    sdk_arena_native_bytes: Atomic(u64) = .init(0),
    /// The daemon rejected OUR descriptor (`-EINVAL`): must stay 0.
    sdk_descriptor_rejects: Atomic(u64) = .init(0),
    sdk_slot_exhaustion: Atomic(u64) = .init(0),
    sdk_park_eras: Atomic(u64) = .init(0),
    // ---- registry ----
    next: ?*Session = null,
    prev: ?*Session = null,

    // -----------------------------------------------------------------
    // Establish / close.
    // -----------------------------------------------------------------

    /// `sqz_session_open`: the mount root is the HELLO credential (the kind
    /// SDK screen admits a directory), so an arena exists before the first
    /// `sqz_open` and `sqz_buf_alloc` may precede it. Null = the reason in
    /// the thread-local last error (and once on stderr).
    pub fn open(gpa: std.mem.Allocator, mount_path: [*:0]const u8, opts: SessionOpts) ?*Session {
        return openInner(gpa, mount_path, opts) catch null;
    }

    /// Every failure arm sets the last error before returning `Refused`.
    fn openInner(gpa: std.mem.Allocator, mount_path: [*:0]const u8, opts: SessionOpts) error{Refused}!*Session {
        if (opts.struct_size != @sizeOf(SessionOpts)) {
            setLastError(.INVAL, "sqz_session_opts.struct_size");
            return error.Refused;
        }
        const drc = linux.openat(linux.AT.FDCWD, mount_path, .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .CLOEXEC = true }, 0);
        if (linux.errno(drc) != .SUCCESS) {
            setLastError(linux.errno(drc), "open(mount_path, O_DIRECTORY) failed");
            return error.Refused;
        }
        const dirfd: linux.fd_t = @intCast(drc);
        defer _ = linux.close(dirfd);
        var st: linux.Statx = undefined;
        if (linux.errno(linux.statx(dirfd, "", linux.AT.EMPTY_PATH, .{ .TYPE = true, .UID = true }, &st)) != .SUCCESS) {
            setLastError(.IO, "statx(mount_path) failed");
            return error.Refused;
        }
        const mount_dev = makedev(st.dev_major, st.dev_minor);
        // The process environment, read like any C library reads it; the
        // client library consumes it during establish only.
        var env = std.process.Environ.createMap(.{ .block = .{ .slice = std.mem.span(std.c.environ) } }, gpa) catch {
            setLastError(.NOMEM, "out of memory");
            return error.Refused;
        };
        defer env.deinit();
        const blob = switch (readBlob(dirfd, &env)) {
            .blob => |b| b,
            .refused => |r| {
                refuse(mount_dev, r);
                return error.Refused;
            },
        };
        if (blob.abi != wire.IPC_ABI) {
            refuse(mount_dev, .{ .ipc_abi = .{ .sdk = wire.IPC_ABI, .daemon = blob.abi } });
            return error.Refused;
        }
        var refusal: ?client.Refusal = null;
        var cl = client.Session.establish(gpa, &blob, dirfd, .{
            .kind = .sdk,
            // The SDK has no commit identity on the wire: it pairs on the window.
            .build_commit = "",
            .mount_uid = st.uid,
            .env = &env,
            .sdk_abi = version.WIRE_ABI,
            .arena_bytes_req = opts.arena_bytes,
            .slots_req = opts.slots,
        }, &refusal) catch |e| {
            refuse(mount_dev, establishReason(e, refusal));
            return error.Refused;
        };
        errdefer cl.teardown();
        const g = cl.geometry();
        var arena = lease.Arena.init(gpa, g.arena_bytes) catch {
            refuse(mount_dev, .out_of_memory);
            return error.Refused;
        };
        errdefer arena.deinit();
        const ops = gpa.alloc(Op, g.slots) catch {
            refuse(mount_dev, .out_of_memory);
            return error.Refused;
        };
        errdefer gpa.free(ops);
        const flights = gpa.alloc(Flight, g.slots) catch {
            refuse(mount_dev, .out_of_memory);
            return error.Refused;
        };
        errdefer gpa.free(flights);
        const ready = gpa.alloc(Cqe, g.slots) catch {
            refuse(mount_dev, .out_of_memory);
            return error.Refused;
        };
        errdefer gpa.free(ready);
        const self = gpa.create(Session) catch {
            refuse(mount_dev, .out_of_memory);
            return error.Refused;
        };
        for (ops, 0..) |*o, i| o.* = .{ .next_free = if (i + 1 < ops.len) @intCast(i + 1) else NONE };
        @memset(flights, .{});
        const op_timeout_ns = if (opts.op_timeout_ms != 0) @as(u64, opts.op_timeout_ms) * std.time.ns_per_ms else cl.op_timeout_ns;
        // The client's own deadline must agree with the app's lever.
        cl.op_timeout_ns = op_timeout_ns;
        self.* = .{
            .gpa = gpa,
            .cl = cl,
            .arena = arena,
            .mount_dev = mount_dev,
            .op_timeout_ns = op_timeout_ns,
            .ops = ops,
            .op_free = 0,
            .flights = flights,
            .ready = ready,
        };
        registerAtforkOnce();
        register(self);
        return self;
    }

    fn makedev(major: u32, minor: u32) u64 {
        return (@as(u64, major) << 8) | minor | (@as(u64, major & ~@as(u32, 0xfff)) << 32) | (@as(u64, minor & ~@as(u32, 0xff)) << 12);
    }

    const BlobRead = union(enum) { blob: wire.Il0Blob, refused: version.Reason };

    /// The bootstrap blob: the seam's file, else `fstatfs` + GETXATTR.
    fn readBlob(dirfd: linux.fd_t, env: *const std.process.Environ.Map) BlobRead {
        var raw: [wire.BLOB_LEN]u8 = undefined;
        if (knobs.parseStr(env.get("SQUEEZEFS_TEST_SDK_IL0_BLOB"))) |path| {
            var z: [std.fs.max_path_bytes]u8 = undefined;
            if (path.len >= z.len) return .{ .refused = .no_bootstrap_xattr };
            @memcpy(z[0..path.len], path);
            z[path.len] = 0;
            const rc = linux.openat(linux.AT.FDCWD, z[0..path.len :0], .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
            if (linux.errno(rc) != .SUCCESS) return .{ .refused = .no_bootstrap_xattr };
            const fd: linux.fd_t = @intCast(rc);
            defer _ = linux.close(fd);
            const n = linux.pread(fd, &raw, raw.len, 0);
            if (linux.errno(n) != .SUCCESS or n != raw.len) return .{ .refused = .no_bootstrap_xattr };
        } else {
            var sf: kernel_statfs = undefined;
            if (linux.errno(linux.syscall2(.fstatfs, @as(usize, @bitCast(@as(isize, dirfd))), @intFromPtr(&sf))) != .SUCCESS) return .{ .refused = .not_squeezefs_mount };
            if (sf.f_type != FUSE_SUPER_MAGIC) return .{ .refused = .not_squeezefs_mount };
            const n = linux.fgetxattr(dirfd, wire.IL0_XATTR, &raw, raw.len);
            switch (linux.errno(n)) {
                .SUCCESS => if (n != raw.len) return .{ .refused = .protocol },
                .NODATA, .OPNOTSUPP => return .{ .refused = .no_bootstrap_xattr },
                else => return .{ .refused = .protocol },
            }
        }
        return .{ .blob = wire.Il0Blob.decode(&raw) catch return .{ .refused = .protocol } };
    }

    fn establishReason(e: client.EstablishError, refusal: ?client.Refusal) version.Reason {
        return switch (e) {
            error.VersionSkew => .{ .ipc_abi = .{ .sdk = wire.IPC_ABI, .daemon = 0 } },
            error.UntrustedRendezvous => .untrusted_rendezvous,
            error.PeerUntrusted => .peer_untrusted,
            error.Unsealed => .unsealed,
            error.Refused => blk: {
                const r = refusal orelse break :blk .protocol;
                break :blk switch (r.class) {
                    .version => .{ .sdk_abi_window = .{ .app = version.WIRE_ABI, .min = r.sdk_abi_min, .max = r.sdk_abi_max } },
                    .disabled => .not_armed,
                    .budget => .budget,
                    .nonce => .nonce,
                    .flags, .mount, .rights => .{ .credential = r.class },
                    .peercred, .internal => .internal,
                };
            },
            error.Protocol => .protocol,
            error.Socket => .socket,
            error.Map => .map,
            error.FutexWaitvUnsupported => .futex_waitv_unsupported,
        };
    }

    /// `sqz_session_close`: `-EBUSY` while any file is open or any buffer
    /// is SUBMITTED (an unmap under a live lease is the custody hazard);
    /// POISONED ranges are unmapped with the arena — the daemon's own
    /// mapping keeps the memfd pages alive for its late write.
    pub fn close(self: *Session) i32 {
        if (self.open_files.load(.acquire) != 0) return @intCast(neg(.BUSY));
        if (self.arena.anySubmitted()) return @intCast(neg(.BUSY));
        unregister(self);
        self.cl.teardown();
        self.arena.deinit();
        self.gpa.free(self.ops);
        self.gpa.free(self.flights);
        self.gpa.free(self.ready);
        const gpa = self.gpa;
        self.* = undefined;
        gpa.destroy(self);
        return 0;
    }

    pub fn poisoned(self: *Session) bool {
        if (self.cl.poisoned()) {
            self.poisonNow(.generation);
            return true;
        }
        return false;
    }

    pub fn poisonCause(self: *const Session) PoisonCause {
        return @fromBackingInt(@intCast(self.poison_cause.load(.acquire)));
    }

    /// The SDK's poison bookkeeping, exactly once: the client poisons,
    /// every SUBMITTED buffer → POISONED, every live batch op completes
    /// `-EIO` onto the ready ring with its unfinished flights abandoned
    /// (their slots are never recycled), every file's inflight count
    /// drops (a poisoned session holds no custody through its files).
    fn poisonNow(self: *Session, cause: PoisonCause) void {
        if (self.poisoned_once.swap(true, .acq_rel)) return;
        self.cl.poison();
        _ = self.poison_cause.cmpxchgStrong(0, @backingInt(cause), .acq_rel, .acquire);
        _ = self.arena.poisonAll();
        self.batch_lock.lock();
        defer self.batch_lock.unlock();
        for (self.ops, 0..) |*op, i| {
            if (!op.live) continue;
            var f = op.head;
            while (f != NONE) {
                const fl = &self.flights[f];
                if (fl.res == null) self.cl.abandonTicket(.{ .slot = f, .generation = fl.generation });
                const nx = fl.next;
                fl.* = .{};
                f = nx;
            }
            self.pushReady(.{ .user_data = op.user_data, .res = neg(.IO) });
            if (op.file) |file| _ = file.inflight.fetchSub(1, .acq_rel);
            op.* = .{ .next_free = self.op_free };
            self.op_free = @intCast(i);
            self.live_ops -= 1;
        }
    }

    // -----------------------------------------------------------------
    // Files.
    // -----------------------------------------------------------------

    pub const OpenError = error{ InvalidArgument, NotConnected, CrossDevice, Protocol, OutOfMemory, Kernel };

    /// `sqz_open`: the real `open(2)` (`O_CLOEXEC` forced: the fd belongs
    /// to the handle), then BIND with the durability word. `O_APPEND`,
    /// `O_PATH`, `O_TMPFILE` are refused `-EINVAL` up front (the daemon
    /// would refuse them too; a first-party API says so before the open).
    /// A refused BIND is never silent: `flags`/`rights` ⇒ `-EINVAL`,
    /// `mount` ⇒ `-EXDEV`. The thread-local last error carries the errno.
    pub fn openFile(self: *Session, path: [*:0]const u8, oflags: u32, mode: u32, opts: OpenOpts) ?*File {
        if (self.poisoned()) {
            setLastError(.NOTCONN, "session poisoned");
            return null;
        }
        if (opts.struct_size != @sizeOf(OpenOpts)) {
            setLastError(.INVAL, "sqz_open_opts.struct_size");
            return null;
        }
        const requested: Durability = switch (opts.durability) {
            0 => .mount,
            1 => .sync,
            else => {
                setLastError(.INVAL, "durability is neither SQZ_D_MOUNT nor SQZ_D_SYNC");
                return null;
            },
        };
        var o: linux.O = @bitCast(oflags);
        if (o.APPEND or o.PATH or o.TMPFILE) {
            setLastError(.INVAL, "O_APPEND / O_PATH / O_TMPFILE handles are not ring-bindable");
            return null;
        }
        // O_SYNC / O_DSYNC raise the class, never lower it.
        const durability: Durability = if (o.SYNC or o.DSYNC) .sync else requested;
        o.CLOEXEC = true;
        const rc = linux.openat(linux.AT.FDCWD, path, o, mode);
        if (linux.errno(rc) != .SUCCESS) {
            setLastError(linux.errno(rc), "open(2) failed");
            return null;
        }
        const fd: linux.fd_t = @intCast(rc);
        const binding = self.cl.bind(fd, durability) catch |e| {
            _ = linux.close(fd);
            switch (e) {
                error.Poisoned => setLastError(.NOTCONN, "session poisoned"),
                error.Refused => switch (self.cl.last_refusal orelse .internal) {
                    .mount => setLastError(.XDEV, "the file is not on this session's mount"),
                    .flags, .rights => setLastError(.INVAL, "the daemon's screen refused the fd (flags/rights)"),
                    else => setLastError(.IO, "the daemon refused the bind"),
                },
                error.Socket => setLastError(.NOTCONN, "ctl socket failed"),
                error.Protocol => setLastError(.PROTO, "malformed BIND reply"),
            }
            return null;
        };
        // Strengthen-only: a grant below the request is a protocol violation.
        if (@backingInt(binding.durability) < @backingInt(durability)) {
            self.cl.unbind(binding.id);
            _ = linux.close(fd);
            setLastError(.PROTO, "the daemon granted a weaker durability than requested");
            return null;
        }
        const f = self.gpa.create(File) catch {
            self.cl.unbind(binding.id);
            _ = linux.close(fd);
            setLastError(.NOMEM, "out of memory");
            return null;
        };
        f.* = .{ .session = self, .fd = fd, .binding = binding, .durability = binding.durability };
        self.files_lock.lock();
        defer self.files_lock.unlock();
        f.next = self.files_head;
        if (self.files_head) |h| h.prev = f;
        self.files_head = f;
        _ = self.open_files.fetchAdd(1, .acq_rel);
        return f;
    }

    /// `sqz_close`: `-EBUSY` while an op on this handle is SUBMITTED (a
    /// poisoned session holds none); UNBIND (silent, never on a poisoned
    /// session), then the real `close(2)`.
    pub fn closeFile(self: *Session, f: *File) i32 {
        if (!self.poisoned() and f.inflight.load(.acquire) != 0) return @intCast(neg(.BUSY));
        self.cl.unbind(f.binding.id);
        const rc = linux.close(f.fd);
        self.files_lock.lock();
        if (f.prev) |p| p.next = f.next else self.files_head = f.next;
        if (f.next) |n| n.prev = f.prev;
        _ = self.open_files.fetchSub(1, .acq_rel);
        self.files_lock.unlock();
        f.live = false;
        self.gpa.destroy(f);
        return if (linux.errno(rc) == .SUCCESS) 0 else @intCast(neg(linux.errno(rc)));
    }

    /// Is `f` a live file of this session?
    pub fn ownsFile(self: *Session, f: *const File) bool {
        return f.live and f.session == self;
    }

    /// The real `fsync(2)`: identical in every durability class.
    pub fn fsync(f: *File) i32 {
        const rc = linux.fsync(f.fd);
        return if (linux.errno(rc) == .SUCCESS) 0 else @intCast(neg(linux.errno(rc)));
    }

    pub fn fstat(f: *File, out: *Stat) i32 {
        var st: linux.Statx = undefined;
        const rc = linux.statx(f.fd, "", linux.AT.EMPTY_PATH, .{ .TYPE = true, .MODE = true, .NLINK = true, .UID = true, .GID = true, .ATIME = true, .MTIME = true, .CTIME = true, .INO = true, .SIZE = true, .BLOCKS = true }, &st);
        if (linux.errno(rc) != .SUCCESS) return @intCast(neg(linux.errno(rc)));
        out.* = .{
            .dev = makedev(st.dev_major, st.dev_minor),
            .ino = st.ino,
            .size = st.size,
            .blocks = st.blocks,
            .blksize = st.blksize,
            .nlink = st.nlink,
            .mode = st.mode,
            .uid = st.uid,
            .gid = st.gid,
            .atime_sec = st.atime.sec,
            .atime_nsec = st.atime.nsec,
            .mtime_sec = st.mtime.sec,
            .mtime_nsec = st.mtime.nsec,
            .ctime_sec = st.ctime.sec,
            .ctime_nsec = st.ctime.nsec,
        };
        return 0;
    }

    // -----------------------------------------------------------------
    // Buffers.
    // -----------------------------------------------------------------

    /// `sqz_buf_alloc`: a 4 KiB-aligned arena range, before or after any
    /// `sqz_open`. Null + `-ENOMEM` when no block of the order is free
    /// (explicit; never a hidden heap buffer with a bounce copy per op),
    /// `-EINVAL` for a zero length, `-ENOTCONN` on a poisoned session.
    pub fn bufAlloc(self: *Session, len: u64) ?*Buf {
        if (self.poisoned()) {
            setLastError(.NOTCONN, "session poisoned");
            return null;
        }
        return self.arena.alloc(len) catch |e| {
            switch (e) {
                error.InvalidArgument => setLastError(.INVAL, "sqz_buf_alloc(len == 0)"),
                error.OutOfMemory => setLastError(.NOMEM, "no free arena block of that order (fragmentation or exhaustion)"),
            }
            return null;
        };
    }

    /// The session a buffer record belongs to (records never leave their
    /// arena, and the arena is embedded in its session).
    pub fn ofBuf(b: *Buf) *Session {
        return @fieldParentPtr("arena", b.arena.?);
    }

    /// `sqz_buf_addr`: the mapped address of the range; null once FREED.
    pub fn bufAddr(self: *Session, b: *Buf) ?[*]u8 {
        if (!b.live) return null;
        return self.cl.arena().ptr + b.range.off;
    }

    // -----------------------------------------------------------------
    // The sync positional ops: submit the flights, reap inline, in place.
    // -----------------------------------------------------------------

    /// Validate one vector against the lease law and the arithmetic:
    /// every buffer this arena's and APP_OWNED/SUBMITTED, every window
    /// inside its buffer, the total ≤ `OP_LEN_MAX`.
    fn validateVec(self: *Session, iov: []const IoVec) ?u64 {
        var total: u64 = 0;
        for (iov) |v| {
            const b: *Buf = @ptrCast(@alignCast(v.buf orelse return null));
            if (!self.arena.owns(b) or !b.live) return null;
            const end = std.math.add(u64, v.buf_off, v.len) catch return null;
            if (end > b.len) return null;
            total = std.math.add(u64, total, v.len) catch return null;
        }
        if (total > OP_LEN_MAX) return null;
        return total;
    }

    fn bufOf(v: IoVec) *Buf {
        return @ptrCast(@alignCast(v.buf.?));
    }

    const StagedFlight = struct { ticket: client.Ticket, len: u32 };

    /// `sqz_pread/pwrite/preadv/pwritev`: one lease per vector entry, the
    /// file's inflight count, then rounds of ≤ `MAX_FLIGHTS` flights under
    /// ONE doorbell each, reaped in offset order with POSIX prefix
    /// semantics (the count is the contiguous prefix ending at the first
    /// short or failed chunk). No slot for the first chunk ⇒ `-EAGAIN`
    /// (the app's own depth exceeded the geometry it asked for); an
    /// abandoned flight (the daemon died) ⇒ the prefix, or `-EIO` with
    /// nothing done — and every later call `-ENOTCONN`.
    pub fn rw(self: *Session, f: *File, op: layout.Op, iov: []const IoVec, offset: i64) i64 {
        if (self.poisoned()) return neg(.NOTCONN);
        if (offset < 0) return neg(.INVAL);
        const total = self.validateVec(iov) orelse return neg(.INVAL);
        if (std.math.add(u64, @intCast(offset), total) catch null == null) return neg(.FBIG);
        if (total == 0) return 0;
        // Leases first: a FREED/POISONED entry refuses the whole op.
        for (iov, 0..) |v, i| {
            bufOf(v).lease.submit() catch {
                for (iov[0..i]) |u| bufOf(u).lease.complete();
                return neg(.INVAL);
            };
        }
        _ = f.inflight.fetchAdd(1, .acq_rel);
        defer {
            for (iov) |v| bufOf(v).lease.complete();
            _ = f.inflight.fetchSub(1, .acq_rel);
        }
        _ = self.sdk_ops_sync.fetchAdd(1, .monotonic);
        const max_op: u64 = self.cl.geometry().max_op_bytes;
        const base: u64 = @intCast(offset);
        var done: u64 = 0;
        var first_err: ?i64 = null;
        var stop = false;
        // The cursor over the vector.
        var vi: usize = 0;
        var voff: u64 = 0;
        var cursor: u64 = 0;
        while (!stop and cursor < total) {
            var staged: [MAX_FLIGHTS]StagedFlight = undefined;
            var n: usize = 0;
            const round_start = cursor;
            while (n < MAX_FLIGHTS and cursor < total) {
                const v = iov[vi];
                if (voff == v.len) {
                    vi += 1;
                    voff = 0;
                    continue;
                }
                const chunk: u32 = @intCast(@min(v.len - voff, max_op));
                const b = bufOf(v);
                const t = self.cl.submitArena(op, f.binding.id, base + cursor, chunk, b.range.off + v.buf_off + voff, F_ARENA_NATIVE) orelse {
                    if (self.cl.poisoned()) {
                        self.poisonNow(.generation);
                        break;
                    }
                    _ = self.sdk_slot_exhaustion.fetchAdd(1, .monotonic);
                    break;
                };
                staged[n] = .{ .ticket = t, .len = chunk };
                n += 1;
                cursor += chunk;
                voff += chunk;
            }
            if (n == 0) {
                if (self.cl.poisoned()) return if (done > 0) @intCast(done) else neg(.IO);
                return if (done > 0) @intCast(done) else neg(.AGAIN);
            }
            self.cl.kickDoorbell();
            self.countNative(op, cursor - round_start);
            // Reap in offset order; drain the rest after a stop.
            var i: usize = 0;
            while (i < n) : (i += 1) {
                const s = staged[i];
                switch (self.cl.waitTicket(s.ticket)) {
                    .abandoned => {
                        self.poisonNow(.deadline);
                        return if (done > 0) @intCast(done) else neg(.IO);
                    },
                    .done => {},
                }
                const res = self.cl.ticketResult(s.ticket).?;
                self.cl.releaseTicket(s.ticket);
                if (stop) continue;
                if (res >= 0) {
                    done += @intCast(res);
                    if (res < s.len) stop = true;
                } else {
                    if (res == neg(.INVAL)) _ = self.sdk_descriptor_rejects.fetchAdd(1, .monotonic);
                    if (done == 0 and first_err == null) first_err = res;
                    stop = true;
                }
            }
        }
        if (done == 0) if (first_err) |e| return e;
        return @intCast(done);
    }

    fn countNative(self: *Session, op: layout.Op, bytes: u64) void {
        switch (op) {
            .read => _ = self.sdk_arena_native_reads.fetchAdd(1, .monotonic),
            .write => _ = self.sdk_arena_native_writes.fetchAdd(1, .monotonic),
            _ => {},
        }
        _ = self.sdk_arena_native_bytes.fetchAdd(bytes, .monotonic);
    }

    // -----------------------------------------------------------------
    // The batch API: submit / poll / wait.
    // -----------------------------------------------------------------

    fn pushReady(self: *Session, c: Cqe) void {
        // Capacity = slots ≥ live ops; every live op yields exactly one CQE.
        assert(self.ready_len < self.ready.len);
        self.ready[(self.ready_head + self.ready_len) % @as(u32, @intCast(self.ready.len))] = c;
        self.ready_len += 1;
    }

    fn popReady(self: *Session) ?Cqe {
        if (self.ready_len == 0) return null;
        const c = self.ready[self.ready_head];
        self.ready_head = (self.ready_head + 1) % @as(u32, @intCast(self.ready.len));
        self.ready_len -= 1;
        return c;
    }

    /// Validate one SQE (the batch is atomic in validation, never in
    /// execution): opcode, a live file of this session, a live buffer of
    /// this arena, the window inside the buffer, the length bound.
    fn validateSqe(self: *Session, s: *const Sqe) bool {
        if (s.opcode != @backingInt(Opcode.pread) and s.opcode != @backingInt(Opcode.pwrite)) return false;
        const f: *File = @ptrCast(@alignCast(s.file orelse return false));
        if (!self.ownsFile(f)) return false;
        const b: *Buf = @ptrCast(@alignCast(s.buf orelse return false));
        if (!self.arena.owns(b) or !b.live) return false;
        const end = std.math.add(u64, s.buf_off, s.len) catch return false;
        if (end > b.len) return false;
        if (s.len > OP_LEN_MAX) return false;
        if (std.math.add(u64, s.offset, s.len) catch null == null) return false;
        return s.offset <= std.math.maxInt(i64);
    }

    /// `sqz_submit`: the count of SQEs whose first chunk was published (a
    /// prefix under slot exhaustion: client-visible backpressure); a later
    /// chunk without a slot shortens THAT op (its CQE reports the staged
    /// prefix — the POSIX short count); `-EINVAL` with nothing submitted on
    /// the first bad SQE; `-EAGAIN` when not even the first could be
    /// placed; ONE doorbell per call.
    pub fn submit(self: *Session, sqes: []const Sqe) i32 {
        if (self.poisoned()) return @intCast(neg(.NOTCONN));
        if (sqes.len == 0) return 0;
        for (sqes) |*s| if (!self.validateSqe(s)) return @intCast(neg(.INVAL));
        const max_op: u64 = self.cl.geometry().max_op_bytes;
        var submitted: usize = 0;
        var staged_any = false;
        self.batch_lock.lock();
        defer self.batch_lock.unlock();
        for (sqes) |*s| {
            if (self.op_free == NONE) break;
            const f: *File = @ptrCast(@alignCast(s.file.?));
            const b: *Buf = @ptrCast(@alignCast(s.buf.?));
            const op: layout.Op = if (s.opcode == @backingInt(Opcode.pread)) .read else .write;
            b.lease.submit() catch break;
            const oi = self.op_free;
            const rec = &self.ops[oi];
            var cursor: u64 = 0;
            var head: u32 = NONE;
            var tail: u32 = NONE;
            var flights: u32 = 0;
            while (cursor < s.len) {
                const chunk: u32 = @intCast(@min(s.len - cursor, max_op));
                const t = self.cl.submitArena(op, f.binding.id, s.offset + cursor, chunk, b.range.off + s.buf_off + cursor, F_ARENA_NATIVE) orelse {
                    if (!self.cl.poisoned()) _ = self.sdk_slot_exhaustion.fetchAdd(1, .monotonic);
                    break;
                };
                self.flights[t.slot] = .{ .op = oi, .generation = t.generation, .len = chunk, .live = true };
                if (tail == NONE) head = t.slot else self.flights[tail].next = t.slot;
                tail = t.slot;
                flights += 1;
                cursor += chunk;
            }
            if (flights == 0 and s.len != 0) {
                b.lease.complete();
                break;
            }
            self.op_free = rec.next_free;
            rec.* = .{ .user_data = s.user_data, .buf = b, .file = f, .head = head, .tail = tail, .remaining = flights, .submitted_ns = clock.monotonicNs(), .live = true };
            _ = f.inflight.fetchAdd(1, .acq_rel);
            self.live_ops += 1;
            submitted += 1;
            staged_any = staged_any or flights > 0;
            self.countNative(op, cursor);
            // A zero-length op completes at once.
            if (flights == 0) self.foldOp(oi);
        }
        if (staged_any) self.cl.kickDoorbell();
        _ = self.sdk_ops_batch.fetchAdd(submitted, .monotonic);
        if (submitted == 0) return @intCast(if (self.cl.poisoned()) neg(.NOTCONN) else neg(.AGAIN));
        return @intCast(submitted);
    }

    /// Fold a finished op's flights in offset order (the prefix law),
    /// release its slots, retire its lease and file count, queue its CQE.
    /// Under `batch_lock`.
    fn foldOp(self: *Session, oi: u32) void {
        const op = &self.ops[oi];
        var done: u64 = 0;
        var first_err: ?i64 = null;
        var stop = false;
        var f = op.head;
        while (f != NONE) {
            const fl = &self.flights[f];
            const res = fl.res orelse 0;
            if (!stop) {
                if (res >= 0) {
                    done += @intCast(res);
                    if (res < fl.len) stop = true;
                } else {
                    if (res == neg(.INVAL)) _ = self.sdk_descriptor_rejects.fetchAdd(1, .monotonic);
                    if (done == 0) first_err = res;
                    stop = true;
                }
            }
            self.cl.releaseTicket(.{ .slot = f, .generation = fl.generation });
            const nx = fl.next;
            fl.* = .{};
            f = nx;
        }
        const res: i64 = if (done == 0 and first_err != null) first_err.? else @intCast(done);
        self.pushReady(.{ .user_data = op.user_data, .res = res });
        op.buf.?.lease.complete();
        _ = op.file.?.inflight.fetchSub(1, .acq_rel);
        op.* = .{ .next_free = self.op_free };
        self.op_free = oi;
        self.live_ops -= 1;
    }

    /// Scan every live flight once; fold the ops that completed. Under
    /// `batch_lock`.
    fn harvest(self: *Session) void {
        for (self.flights, 0..) |*fl, slot| {
            if (!fl.live or fl.res != null) continue;
            const res = self.cl.ticketResult(.{ .slot = @intCast(slot), .generation = fl.generation }) orelse continue;
            fl.res = res;
            const op = &self.ops[fl.op];
            op.remaining -= 1;
            if (op.remaining == 0) self.foldOp(fl.op);
        }
    }

    /// Move ready CQEs out. Under `batch_lock`.
    fn drainReady(self: *Session, out: []Cqe) usize {
        var n: usize = 0;
        while (n < out.len) : (n += 1) out[n] = self.popReady() orelse break;
        return n;
    }

    /// `sqz_poll`: the CQEs ready now (0 if none); after a poison the
    /// `-EIO` CQEs of the ops in flight at the death are delivered, then
    /// `-ENOTCONN`.
    pub fn poll(self: *Session, out: []Cqe) i32 {
        if (out.len == 0) return @intCast(neg(.INVAL));
        const dead = self.poisoned();
        self.batch_lock.lock();
        defer self.batch_lock.unlock();
        if (!dead) self.harvest();
        const n = self.drainReady(out);
        if (n == 0 and dead) return @intCast(neg(.NOTCONN));
        return @intCast(n);
    }

    /// Visible pending flights and the oldest op's age (0 if none). Under
    /// `batch_lock`.
    fn pending(self: *Session, now_ns: u64) struct { flights: u32, oldest_age_ns: u64 } {
        var flights: u32 = 0;
        var oldest: u64 = now_ns;
        for (self.ops) |*op| {
            if (!op.live) continue;
            flights += op.remaining;
            oldest = @min(oldest, op.submitted_ns);
        }
        return .{ .flights = flights, .oldest_age_ns = now_ns - oldest };
    }

    /// `sqz_wait`: ≥ 1 CQE, 0 on the timeout (or with nothing in flight),
    /// `-ENOTCONN` once poisoned and drained. Parks on the completion
    /// doorbell (register → snapshot → re-scan, `k = clamp(pending/4, 2,
    /// pending)`) in 50 ms slices re-checking the heartbeat ladder and
    /// the op deadline; multi-reaper-safe through the doorbell protocol.
    pub fn wait(self: *Session, out: []Cqe, timeout_ns: ?u64) i32 {
        if (out.len == 0) return @intCast(neg(.INVAL));
        const start = clock.monotonicNs();
        const deadline: ?u64 = if (timeout_ns) |t| start + t else null;
        var spun: u32 = 0;
        while (true) {
            const n = self.poll(out);
            if (n != 0) return n;
            const now = clock.monotonicNs();
            self.batch_lock.lock();
            const p = self.pending(now);
            self.batch_lock.unlock();
            if (p.flights == 0) return 0;
            if (p.oldest_age_ns >= self.op_timeout_ns) {
                self.poisonNow(.deadline);
                continue;
            }
            if (deadline) |d| if (now >= d) return 0;
            // The adaptive spin before the first park of this wait.
            if (spun < client.WAIT_SPINS_PARKY) {
                spun += 1;
                std.atomic.spinLoopHint();
                continue;
            }
            const k = doorbell.reapBatchWakeThreshold(p.flights);
            const snap = self.cl.cqeParkBeginBatch(k);
            _ = self.sdk_park_eras.fetchAdd(1, .monotonic);
            // The mandatory re-scan after registering.
            const again = self.poll(out);
            if (again != 0) {
                self.cl.cqeParkEnd();
                return again;
            }
            var slice = rendezvous.WAIT_SLICE_NS;
            if (deadline) |d| slice = @min(slice, d -| clock.monotonicNs());
            var entries = [_]client.WaitEntry{.{ .session = &self.cl, .expected = snap }};
            _ = client.Session.waitAny(&entries, slice);
            self.cl.cqeParkEnd();
            if (!self.cl.heartbeatAlive(clock.monotonicNs())) self.poisonNow(.heartbeat);
        }
    }

    pub fn liveOps(self: *Session) u32 {
        self.batch_lock.lock();
        defer self.batch_lock.unlock();
        return self.live_ops;
    }
};

// ---------------------------------------------------------------------------
// Tests (spec §8), against the in-process stub host of `ipc/client.zig`.
// ---------------------------------------------------------------------------

const testing = std.testing;
const rl = @import("../ipc/rendezvous_linux.zig");
const shm = @import("../ipc/shm.zig");

/// libc's environment setters (not in `std.c`): the SDK reads `environ`
/// like any C library, so the tests arm the seam through libc.
const c_env = struct {
    extern "c" fn setenv(name: [*:0]const u8, value: [*:0]const u8, overwrite: c_int) c_int;
    extern "c" fn unsetenv(name: [*:0]const u8) c_int;
};

fn sleepNs(ns: u64) void {
    const ts: linux.timespec = .{ .sec = @intCast(ns / std.time.ns_per_s), .nsec = @intCast(ns % std.time.ns_per_s) };
    _ = linux.nanosleep(&ts, null);
}

/// A mount for the SDK: a scratch directory on /tmp (the credential and the
/// "mount device"), a file in it, the stub host armed on that device, the
/// blob written to a file the `SQUEEZEFS_TEST_SDK_IL0_BLOB` seam names.
pub const Fixture = struct {
    host: client.StubHost = undefined,
    dir_z: [64]u8 = undefined,
    dir_len: usize = 0,
    file_z: [96]u8 = undefined,
    file_len: usize = 0,
    blob_z: [96]u8 = undefined,
    blob_len: usize = 0,
    acceptor: ?std.Thread = null,
    commit: []const u8 = "0123456789ab",
    store: ?[]u8 = null,

    pub fn dir(self: *Fixture) [:0]const u8 {
        return self.dir_z[0..self.dir_len :0];
    }
    pub fn file(self: *Fixture) [:0]const u8 {
        return self.file_z[0..self.file_len :0];
    }
    pub fn blobPath(self: *Fixture) [:0]const u8 {
        return self.blob_z[0..self.blob_len :0];
    }

    pub fn init(self: *Fixture, gpa: std.mem.Allocator, geometry: layout.Geometry, store_bytes: usize) !void {
        self.* = .{};
        const pid = linux.getpid();
        const d = try std.fmt.bufPrintSentinel(&self.dir_z, "/tmp/sqz-sdk-{d}", .{pid}, 0);
        self.dir_len = d.len;
        _ = linux.mkdirat(linux.AT.FDCWD, d, 0o700);
        const f = try std.fmt.bufPrintSentinel(&self.file_z, "{s}/data", .{d}, 0);
        self.file_len = f.len;
        const frc = linux.openat(linux.AT.FDCWD, f, .{ .ACCMODE = .RDWR, .CREAT = true, .TRUNC = true, .CLOEXEC = true }, 0o600);
        if (linux.errno(frc) != .SUCCESS) return error.OpenFailed;
        _ = linux.close(@intCast(frc));
        const drc = linux.openat(linux.AT.FDCWD, d, .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .CLOEXEC = true }, 0);
        if (linux.errno(drc) != .SUCCESS) return error.OpenFailed;
        const dirfd: linux.fd_t = @intCast(drc);
        defer _ = linux.close(dirfd);
        const dev = rl.deviceOf(dirfd).?;
        try self.host.init(.{ .armed = true, .our_commit = self.commit, .arena_cap_bytes = 1 << 32, .per_uid_session_cap = 64, .default_geometry = geometry }, dev);
        if (store_bytes > 0) {
            self.store = try gpa.alloc(u8, store_bytes);
            @memset(self.store.?, 0);
            self.host.store = self.store;
        }
        try self.writeBlob();
    }

    pub fn writeBlob(self: *Fixture) !void {
        const b = try self.host.blob(self.commit);
        const p = try std.fmt.bufPrintSentinel(&self.blob_z, "{s}/il0.blob", .{self.dir()}, 0);
        self.blob_len = p.len;
        const rc = linux.openat(linux.AT.FDCWD, p, .{ .ACCMODE = .WRONLY, .CREAT = true, .TRUNC = true, .CLOEXEC = true }, 0o600);
        if (linux.errno(rc) != .SUCCESS) return error.OpenFailed;
        const fd: linux.fd_t = @intCast(rc);
        defer _ = linux.close(fd);
        const n = linux.write(fd, b.bytes(), wire.BLOB_LEN);
        if (n != wire.BLOB_LEN) return error.WriteFailed;
    }

    /// Point the seam at the blob (the process environment: the SDK reads
    /// `environ` like any C library).
    pub fn armSeam(self: *Fixture) void {
        _ = c_env.setenv("SQUEEZEFS_TEST_SDK_IL0_BLOB", self.blobPath().ptr, 1);
    }

    pub fn disarmSeam() void {
        _ = c_env.unsetenv("SQUEEZEFS_TEST_SDK_IL0_BLOB");
    }

    fn acceptorRun(h: *client.StubHost) void {
        h.acceptOne() catch {};
    }

    /// Accept the next session on a thread (one at a time).
    pub fn acceptNext(self: *Fixture) !void {
        if (self.acceptor) |t| t.join();
        self.acceptor = try std.Thread.spawn(.{}, acceptorRun, .{&self.host});
    }

    pub fn joinAcceptor(self: *Fixture) void {
        if (self.acceptor) |t| t.join();
        self.acceptor = null;
    }

    /// Retire the stub's current session so a second one can be accepted.
    pub fn resetHost(self: *Fixture) void {
        self.joinAcceptor();
        self.host.stop.store(true, .release);
        if (self.host.thread) |t| t.join();
        self.host.thread = null;
        self.host.stop.store(false, .release);
        if (self.host.accepted) |a| shm.unmap(a.mapped);
        self.host.accepted = null;
        if (self.host.ctl >= 0) _ = linux.close(self.host.ctl);
        self.host.ctl = -1;
        self.host.refused = null;
        self.host.frozen.store(false, .release);
        self.host.sever.store(false, .release);
    }

    pub fn deinit(self: *Fixture, gpa: std.mem.Allocator) void {
        disarmSeam();
        self.joinAcceptor();
        self.host.deinit();
        if (self.store) |s| gpa.free(s);
        _ = linux.unlinkat(linux.AT.FDCWD, self.file(), 0);
        _ = linux.unlinkat(linux.AT.FDCWD, self.blobPath(), 0);
        _ = linux.unlinkat(linux.AT.FDCWD, self.dir(), linux.AT.REMOVEDIR);
    }

    pub fn openSession(self: *Fixture, gpa: std.mem.Allocator, opts: SessionOpts) !*Session {
        self.armSeam();
        try self.acceptNext();
        const s = Session.open(gpa, self.dir().ptr, opts) orelse return error.SessionRefused;
        self.joinAcceptor();
        return s;
    }
};

const test_geometry: layout.Geometry = .{ .ring_entries = 64, .slots = 64, .arena_bytes = 64 * 16 * layout.PAGE_BYTES, .max_op_bytes = 4 * layout.PAGE_BYTES };
const default_opts: SessionOpts = .{ .struct_size = @sizeOf(SessionOpts), .arena_bytes = 0, .slots = 0, .op_timeout_ms = 0 };

const O_RDWR: u32 = @bitCast(linux.O{ .ACCMODE = .RDWR });
const O_RDONLY: u32 = @bitCast(linux.O{ .ACCMODE = .RDONLY });

test "sdk e2e: establish as kind SDK (a directory credential, the wire ABI word, the geometry request clamped); a buffer is allocated BEFORE any open; a pwrite/pread round trip is byte-exact and in place; fsync/fstat; strengthen-only durability; close refuses -EBUSY until the files close" {
    const gpa = testing.allocator;
    var fx: Fixture = undefined;
    try fx.init(gpa, test_geometry, 2 << 20);
    defer fx.deinit(gpa);
    var opts = default_opts;
    // A request above the default geometry's arena, under the cap: granted.
    opts.arena_bytes = 2 * test_geometry.arena_bytes;
    opts.slots = 1000;
    const s = try fx.openSession(gpa, opts);
    try testing.expectEqual(wire.ClientKind.sdk, fx.host.accepted.?.admission.kind);
    try testing.expectEqual(2 * test_geometry.arena_bytes, s.cl.geometry().arena_bytes);
    try testing.expectEqual(@as(u32, 64), s.cl.geometry().slots);
    try testing.expectEqual(2 * test_geometry.arena_bytes, s.arena.arenaBytes());
    // Buffers before any file.
    const b = s.bufAlloc(3 * 4096 + 17).?;
    try testing.expectEqual(@as(u64, 0), @intFromPtr(s.bufAddr(b).?) % lease.GRANULE);
    try testing.expectEqual(s, Session.ofBuf(b));
    const open_opts: OpenOpts = .{ .struct_size = @sizeOf(OpenOpts), .durability = 0 };
    const f = s.openFile(fx.file().ptr, O_RDWR, 0, open_opts).?;
    try testing.expectEqual(Durability.mount, f.durability);
    try testing.expect(f.binding.rights.read and f.binding.rights.write);
    try testing.expectEqual(@as(i32, @intCast(neg(.BUSY))), s.close());
    // A pattern through the arena, written, cleared, read back in place.
    const data = s.bufAddr(b).?[0..b.len];
    for (data, 0..) |*d, i| d.* = @truncate(i * 7 + 3);
    const iov = [_]IoVec{.{ .buf = b, .buf_off = 0, .len = b.len }};
    try testing.expectEqual(@as(i64, @intCast(b.len)), s.rw(f, .write, &iov, 4096));
    @memset(data, 0);
    try testing.expectEqual(@as(i64, @intCast(b.len)), s.rw(f, .read, &iov, 4096));
    for (data, 0..) |d, i| try testing.expectEqual(@as(u8, @truncate(i * 7 + 3)), d);
    // The stub's store agrees byte for byte.
    try testing.expectEqualSlices(u8, data, fx.store.?[4096..][0..b.len]);
    // base+offset addressing: a window inside the buffer.
    const win = [_]IoVec{.{ .buf = b, .buf_off = 4096, .len = 100 }};
    try testing.expectEqual(@as(i64, 100), s.rw(f, .read, &win, 4096 + 4096));
    // A window past the buffer is -EINVAL; a negative offset too.
    const bad = [_]IoVec{.{ .buf = b, .buf_off = 4096, .len = b.len }};
    try testing.expectEqual(neg(.INVAL), s.rw(f, .read, &bad, 0));
    try testing.expectEqual(neg(.INVAL), s.rw(f, .read, &iov, -1));
    // fsync and fstat on the real fd.
    try testing.expectEqual(@as(i32, 0), Session.fsync(f));
    var st: Stat = undefined;
    try testing.expectEqual(@as(i32, 0), Session.fstat(f, &st));
    try testing.expect(st.mode & 0o170000 == 0o100000);
    // Strengthen-only: SYNC requested is granted SYNC; a third value refuses.
    const sync_opts: OpenOpts = .{ .struct_size = @sizeOf(OpenOpts), .durability = 1 };
    const fs = s.openFile(fx.file().ptr, O_RDWR, 0, sync_opts).?;
    try testing.expectEqual(Durability.sync, fs.durability);
    try testing.expectEqual(@as(i32, 0), s.closeFile(fs));
    try testing.expect(s.openFile(fx.file().ptr, O_RDWR, 0, .{ .struct_size = @sizeOf(OpenOpts), .durability = 2 }) == null);
    try testing.expectEqual(@as(i32, @intCast(neg(.INVAL))), lastError());
    // O_APPEND is refused before any open.
    try testing.expect(s.openFile(fx.file().ptr, @bitCast(linux.O{ .ACCMODE = .WRONLY, .APPEND = true }), 0, open_opts) == null);
    try testing.expectEqual(@as(i32, @intCast(neg(.INVAL))), lastError());
    // A read-only handle's write completes -EBADF kernel-identically.
    const ro = s.openFile(fx.file().ptr, O_RDONLY, 0, open_opts).?;
    try testing.expectEqual(neg(.BADF), s.rw(ro, .write, &iov, 0));
    try testing.expectEqual(@as(i32, 0), s.closeFile(ro));
    try testing.expectEqual(@as(i32, 0), s.closeFile(f));
    try s.arena.free(b);
    try testing.expect(s.arena.census().holds());
    try testing.expectEqual(@as(u64, 0), s.sdk_descriptor_rejects.load(.monotonic));
    try testing.expectEqual(@as(i32, 0), s.close());
}

test "sdk e2e: a 64 MiB buffer — the whole default arena as ONE allocation — pwrite/pread round trip byte-exact through 64 flights of the 1 MiB window; preadv/pwritev fold a vector with prefix semantics" {
    const gpa = testing.allocator;
    var fx: Fixture = undefined;
    try fx.init(gpa, layout.Geometry.default, 64 << 20);
    defer fx.deinit(gpa);
    const s = try fx.openSession(gpa, default_opts);
    const big = s.bufAlloc(64 << 20).?;
    try testing.expectEqual(@as(u64, 0), big.range.off);
    const f = s.openFile(fx.file().ptr, O_RDWR, 0, .{ .struct_size = @sizeOf(OpenOpts), .durability = 0 }).?;
    const data = s.bufAddr(big).?[0..big.len];
    var prng = std.Random.DefaultPrng.init(0x64);
    prng.random().bytes(data);
    const expect_hash = checksum.hash64(data);
    const iov = [_]IoVec{.{ .buf = big, .buf_off = 0, .len = big.len }};
    try testing.expectEqual(@as(i64, 64 << 20), s.rw(f, .write, &iov, 0));
    @memset(data, 0);
    try testing.expectEqual(@as(i64, 64 << 20), s.rw(f, .read, &iov, 0));
    try testing.expectEqual(expect_hash, checksum.hash64(data));
    try testing.expectEqual(expect_hash, checksum.hash64(fx.store.?));
    for (s.cl.m.slots()) |*sl| try testing.expectEqual(@as(u32, 0), sl.core.state.load(.monotonic));
    // A vector: three windows of the one buffer; a read past the store's
    // end is short and the fold stops there (prefix).
    const vec = [_]IoVec{ .{ .buf = big, .buf_off = 0, .len = 4096 }, .{ .buf = big, .buf_off = 1 << 20, .len = 3 << 20 }, .{ .buf = big, .buf_off = 8 << 20, .len = 100 } };
    try testing.expectEqual(@as(i64, 4096 + (3 << 20) + 100), s.rw(f, .read, &vec, 0));
    try testing.expectEqual(@as(i64, 4096 + 1000), s.rw(f, .read, &vec, (64 << 20) - 4096 - 1000));
    try testing.expectEqual(@as(i64, 4096 + (3 << 20) + 100), s.rw(f, .write, &vec, 0));
    try testing.expectEqual(@as(i64, 0), s.rw(f, .read, &vec, 64 << 20));
    try testing.expectEqual(@as(i32, 0), s.closeFile(f));
    try s.arena.free(big);
    try testing.expectEqual(@as(i32, 0), s.close());
}

test "sdk e2e: the batch API — 8 tickets submitted under one doorbell, reaped by poll/wait with user_data intact; free while SUBMITTED is -EBUSY (one CAS, never deferred) and 0 after the CQE; prefix under slot exhaustion; -EINVAL validates the batch atomically" {
    const gpa = testing.allocator;
    var fx: Fixture = undefined;
    try fx.init(gpa, test_geometry, 1 << 20);
    defer fx.deinit(gpa);
    const s = try fx.openSession(gpa, default_opts);
    const f = s.openFile(fx.file().ptr, O_RDWR, 0, .{ .struct_size = @sizeOf(OpenOpts), .durability = 0 }).?;
    var bufs: [8]*Buf = undefined;
    for (&bufs, 0..) |*b, i| {
        b.* = s.bufAlloc(4096).?;
        @memset(s.bufAddr(b.*).?[0..4096], @intCast(i + 1));
    }
    var sqes: [8]Sqe = undefined;
    for (&sqes, 0..) |*q, i| q.* = .{ .opcode = @backingInt(Opcode.pwrite), .len = 4096, .offset = @as(u64, i) * 4096, .file = f, .buf = bufs[i], .buf_off = 0, .user_data = 100 + i };
    const before = fx.host.served.load(.monotonic);
    try testing.expectEqual(@as(i32, 8), s.submit(&sqes));
    // Free while SUBMITTED: -EBUSY, the lease unchanged.
    for (bufs) |b| try testing.expectError(error.Busy, s.arena.free(b));
    try testing.expectEqual(@as(u64, 8), s.arena.census().busy_refusals);
    var cqes: [8]Cqe = undefined;
    var got: usize = 0;
    var seen: [8]bool = @splat(false);
    while (got < 8) {
        const n = s.wait(cqes[got..], 2 * std.time.ns_per_s);
        try testing.expect(n > 0);
        got += @intCast(n);
    }
    for (cqes) |c| {
        try testing.expectEqual(@as(i64, 4096), c.res);
        try testing.expect(c.user_data >= 100 and c.user_data < 108);
        seen[c.user_data - 100] = true;
    }
    for (seen) |v| try testing.expect(v);
    try testing.expectEqual(before + 8, fx.host.served.load(.monotonic));
    for (0..bufs.len) |i| for (fx.store.?[i * 4096 ..][0..4096]) |v| try testing.expectEqual(@as(u8, @intCast(i + 1)), v);
    try testing.expectEqual(@as(i32, 0), s.poll(&cqes));
    try testing.expectEqual(@as(i32, 0), s.wait(&cqes, 10 * std.time.ns_per_ms));
    try testing.expectEqual(@as(u32, 0), s.liveOps());
    // After the CQEs: free succeeds; a second free is -EINVAL; a submit on a freed buffer -EINVAL with nothing submitted.
    try s.arena.free(bufs[0]);
    try testing.expectError(error.InvalidArgument, s.arena.free(bufs[0]));
    try testing.expectEqual(@as(i32, @intCast(neg(.INVAL))), s.submit(&sqes));
    try testing.expectEqual(@as(u32, 0), f.inflight.load(.monotonic));
    // Prefix under slot exhaustion: 64 slots, 70 one-page reads ⇒ 64 submitted.
    const many = s.bufAlloc(70 * 4096).?;
    var more: [70]Sqe = undefined;
    for (&more, 0..) |*q, i| q.* = .{ .opcode = @backingInt(Opcode.pread), .len = 4096, .offset = @as(u64, i) * 4096, .file = f, .buf = many, .buf_off = @as(u64, i) * 4096, .user_data = i };
    const prefix = s.submit(&more);
    try testing.expectEqual(@as(i32, 64), prefix);
    try testing.expectEqual(@as(u24, 64), many.lease.load().inflight);
    var all: [70]Cqe = undefined;
    got = 0;
    while (got < 64) {
        const n = s.wait(all[got..], 2 * std.time.ns_per_s);
        try testing.expect(n > 0);
        got += @intCast(n);
    }
    try testing.expectEqual(@as(u24, 0), many.lease.load().inflight);
    // A chunked op: 70 pages = 18 flights of the 4-page window, ONE CQE.
    const one = [_]Sqe{.{ .opcode = @backingInt(Opcode.pread), .len = 70 * 4096, .offset = 0, .file = f, .buf = many, .buf_off = 0, .user_data = 7 }};
    try testing.expectEqual(@as(i32, 1), s.submit(&one));
    try testing.expectEqual(@as(i32, 1), s.wait(all[0..1], 2 * std.time.ns_per_s));
    try testing.expectEqual(@as(u64, 7), all[0].user_data);
    // The store is 1 MiB: 70 pages from 0 are all served.
    try testing.expectEqual(@as(i64, 70 * 4096), all[0].res);
    // Every slot FREE again; the batch closes clean.
    for (s.cl.m.slots()) |*sl| try testing.expectEqual(@as(u32, 0), sl.core.state.load(.monotonic));
    try s.arena.free(many);
    for (bufs[1..]) |b| try s.arena.free(b);
    try testing.expect(s.arena.census().holds());
    try testing.expectEqual(@as(i32, 0), s.closeFile(f));
    try testing.expectEqual(@as(i32, 0), s.close());
}

test "sdk e2e: daemon death — the heartbeat watch poisons within the 1 s ladder; the batch op in flight completes -EIO, its buffer is POISONED and its free quarantines; every later call is -ENOTCONN; close succeeds once the files are closed" {
    const gpa = testing.allocator;
    var fx: Fixture = undefined;
    try fx.init(gpa, test_geometry, 1 << 20);
    defer fx.deinit(gpa);
    var opts = default_opts;
    opts.op_timeout_ms = 20_000;
    const s = try fx.openSession(gpa, opts);
    const f = s.openFile(fx.file().ptr, O_RDWR, 0, .{ .struct_size = @sizeOf(OpenOpts), .durability = 0 }).?;
    const b = s.bufAlloc(4096).?;
    const idle = s.bufAlloc(4096).?;
    // Freeze + sever the daemon, then submit: the wait must poison in ≈ 1 s.
    fx.host.sever.store(true, .release);
    fx.host.frozen.store(true, .release);
    sleepNs(20 * std.time.ns_per_ms);
    const sqe = [_]Sqe{.{ .opcode = @backingInt(Opcode.pread), .len = 4096, .offset = 0, .file = f, .buf = b, .buf_off = 0, .user_data = 42 }};
    try testing.expectEqual(@as(i32, 1), s.submit(&sqe));
    const t0 = clock.monotonicNs();
    var cqe: [1]Cqe = undefined;
    try testing.expectEqual(@as(i32, 1), s.wait(&cqe, 10 * std.time.ns_per_s));
    const elapsed = clock.monotonicNs() - t0;
    // The heartbeat's ladder, never the 20 s op deadline: the cause says
    // which (the wall is the host's).
    try testing.expect(elapsed >= rendezvous.HEARTBEAT_STALE_NS);
    try testing.expectEqual(@as(u64, 42), cqe[0].user_data);
    try testing.expectEqual(neg(.IO), cqe[0].res);
    try testing.expectEqual(PoisonCause.heartbeat, s.poisonCause());
    try testing.expectEqual(lease.State.poisoned, b.lease.load().state);
    try testing.expectEqual(lease.State.app_owned, idle.lease.load().state);
    // Every later call: -ENOTCONN (reads of the poisoned bytes stay legal).
    try testing.expectEqual(@as(i32, @intCast(neg(.NOTCONN))), s.wait(&cqe, 0));
    try testing.expectEqual(@as(i32, @intCast(neg(.NOTCONN))), s.poll(&cqe));
    try testing.expectEqual(@as(i32, @intCast(neg(.NOTCONN))), s.submit(&sqe));
    const iov = [_]IoVec{.{ .buf = idle, .buf_off = 0, .len = 4096 }};
    try testing.expectEqual(neg(.NOTCONN), s.rw(f, .read, &iov, 0));
    try testing.expect(s.bufAlloc(4096) == null);
    try testing.expectEqual(@as(i32, @intCast(neg(.NOTCONN))), lastError());
    try testing.expect(s.openFile(fx.file().ptr, O_RDWR, 0, .{ .struct_size = @sizeOf(OpenOpts), .durability = 0 }) == null);
    try testing.expect(s.bufAddr(b) != null);
    // The abandoned slot is never recycled; the quarantine takes the range.
    try testing.expectEqual(@as(u64, 1), s.cl.il_abandoned_slots.load(.monotonic));
    try s.arena.free(b);
    try s.arena.free(idle);
    const c = s.arena.census();
    try testing.expectEqual(@as(u64, 1), c.quarantined_granules);
    try testing.expectEqual(@as(u64, 1), c.poisoned);
    try testing.expect(c.holds());
    try testing.expectEqual(@as(i32, @intCast(neg(.BUSY))), s.close());
    try testing.expectEqual(@as(i32, 0), s.closeFile(f));
    try testing.expectEqual(@as(i32, 0), s.close());
}

test "sdk e2e: the op deadline is the last rung — a frozen daemon with a live socket poisons a sync op at the seam's deadline; the op answers -EIO" {
    const gpa = testing.allocator;
    var fx: Fixture = undefined;
    try fx.init(gpa, test_geometry, 1 << 20);
    defer fx.deinit(gpa);
    var opts = default_opts;
    opts.op_timeout_ms = 300;
    const s = try fx.openSession(gpa, opts);
    try testing.expectEqual(300 * std.time.ns_per_ms, s.op_timeout_ns);
    const f = s.openFile(fx.file().ptr, O_RDWR, 0, .{ .struct_size = @sizeOf(OpenOpts), .durability = 0 }).?;
    const b = s.bufAlloc(4096).?;
    fx.host.frozen.store(true, .release);
    sleepNs(20 * std.time.ns_per_ms);
    const iov = [_]IoVec{.{ .buf = b, .buf_off = 0, .len = 4096 }};
    const t0 = clock.monotonicNs();
    try testing.expectEqual(neg(.IO), s.rw(f, .read, &iov, 0));
    const elapsed = clock.monotonicNs() - t0;
    try testing.expect(elapsed >= 300 * std.time.ns_per_ms);
    try testing.expectEqual(PoisonCause.deadline, s.poisonCause());
    try testing.expectEqual(lease.State.poisoned, b.lease.load().state);
    try testing.expectEqual(neg(.NOTCONN), s.rw(f, .read, &iov, 0));
    try s.arena.free(b);
    try testing.expectEqual(@as(i32, 0), s.closeFile(f));
    try testing.expectEqual(@as(i32, 0), s.close());
}

test "sdk e2e: the ABI window — a daemon whose window excludes the app's wire word refuses with the text naming both values (once per mount on stderr, every time in the last error); a -dirty daemon admits the SDK (the window, never the commit) while it refuses a shim" {
    const gpa = testing.allocator;
    // Every expected value below is DERIVED from the daemon's window
    // (`wire.sdk_abi_accepted`) and the coarse ipc word, never written as
    // a number: a release act that bumps either edits the words alone.
    const window = version.windowBounds(&wire.sdk_abi_accepted);
    const outside_window: u32 = window.max + 1;
    try testing.expect(!version.windowAccepts(&wire.sdk_abi_accepted, outside_window));
    try testing.expect(version.windowAccepts(&wire.sdk_abi_accepted, version.WIRE_ABI));
    var fx: Fixture = undefined;
    try fx.init(gpa, test_geometry, 0);
    defer fx.deinit(gpa);
    // A -dirty daemon: the SDK pairs on the window.
    fx.host.policy.our_commit = "0123456789ab-dirty";
    fx.commit = "0123456789ab-dirty";
    try fx.writeBlob();
    const s = try fx.openSession(gpa, default_opts);
    try testing.expectEqual(wire.ClientKind.sdk, fx.host.accepted.?.admission.kind);
    try testing.expectEqual(@as(i32, 0), s.close());
    fx.resetHost();
    // The same daemon refuses a shim built clean (commit inequality).
    {
        try fx.acceptNext();
        var env = std.process.Environ.Map.init(gpa);
        defer env.deinit();
        var refusal: ?client.Refusal = null;
        const blob = try fx.host.blob(fx.commit);
        const drc = linux.openat(linux.AT.FDCWD, fx.file().ptr, .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
        const cred: linux.fd_t = @intCast(drc);
        defer _ = linux.close(cred);
        try testing.expectError(error.VersionSkew, client.Session.establish(gpa, &blob, cred, .{ .build_commit = "0123456789ab", .mount_uid = linux.geteuid(), .env = &env }, &refusal));
        // Nothing connected: retire the pending acceptor by connecting a throwaway shim that the daemon refuses.
        var dirty_env = std.process.Environ.Map.init(gpa);
        defer dirty_env.deinit();
        try testing.expectError(error.Refused, client.Session.establish(gpa, &blob, cred, .{ .build_commit = "0123456789ab-dirty", .mount_uid = linux.geteuid(), .env = &dirty_env, .kind = .sdk, .sdk_abi = outside_window }, &refusal));
        try testing.expectEqual(wire.RefuseClass.version, refusal.?.class);
        // The daemon names ITS window: the set's oldest and newest words.
        try testing.expectEqual(window.min, refusal.?.sdk_abi_min);
        try testing.expectEqual(window.max, refusal.?.sdk_abi_max);
        fx.joinAcceptor();
        fx.resetHost();
    }
    // The refusal text through the SDK's own open: the SDK's HELLO always
    // carries this build's wire word, which its own daemon's window holds
    // (driving the client directly, above, is how a word outside it is
    // sent); here the SDK's path is exercised with the blob's coarse
    // ipc_abi off by one — the pre-check names both values.
    {
        var blob = try fx.host.blob(fx.commit);
        blob.abi += 1;
        const rc = linux.openat(linux.AT.FDCWD, fx.blobPath().ptr, .{ .ACCMODE = .WRONLY, .TRUNC = true, .CLOEXEC = true }, 0);
        const fd: linux.fd_t = @intCast(rc);
        _ = linux.write(fd, blob.bytes(), wire.BLOB_LEN);
        _ = linux.close(fd);
        fx.armSeam();
        var want_buf: [version.TEXT_MAX]u8 = undefined;
        const want_ipc = try std.fmt.bufPrint(&want_buf, "ipc abi mismatch (sdk {d}, daemon {d})", .{ wire.IPC_ABI, wire.IPC_ABI + 1 });
        const lines_before = refusalLinesPrinted();
        try testing.expect(Session.open(gpa, fx.dir().ptr, default_opts) == null);
        try testing.expectEqual(@as(i32, @intCast(neg(.PROTONOSUPPORT))), lastError());
        try testing.expectEqualStrings(want_ipc, lastErrorText());
        try testing.expectEqual(lines_before + 1, refusalLinesPrinted());
        // Once per (mount, reason): a second refusal prints nothing new.
        try testing.expect(Session.open(gpa, fx.dir().ptr, default_opts) == null);
        try testing.expectEqual(lines_before + 1, refusalLinesPrinted());
        try testing.expectEqualStrings(want_ipc, lastErrorText());
    }
    // Not armed: a seam path that does not exist reads as no bootstrap xattr.
    {
        _ = c_env.setenv("SQUEEZEFS_TEST_SDK_IL0_BLOB", "/nonexistent/il0.blob", 1);
        try testing.expect(Session.open(gpa, fx.dir().ptr, default_opts) == null);
        try testing.expectEqual(@as(i32, @intCast(neg(.NODATA))), lastError());
    }
    // A path that is not a directory.
    try testing.expect(Session.open(gpa, fx.file().ptr, default_opts) == null);
    try testing.expectEqual(@as(i32, @intCast(neg(.NOTDIR))), lastError());
    // The window text itself, as the SDK would print THIS daemon's refusal
    // of the word just past its window.
    const r: version.Reason = .{ .sdk_abi_window = .{ .app = outside_window, .min = window.min, .max = window.max } };
    var tb: [version.TEXT_MAX]u8 = undefined;
    var wb: [version.TEXT_MAX]u8 = undefined;
    const want_window = try std.fmt.bufPrint(&wb, "sdk abi outside the daemon's window (app {d}, daemon accepts {d}..{d})", .{ outside_window, window.min, window.max });
    try testing.expectEqualStrings(want_window, version.text(&tb, r));
}

test "sdk: the fork child poisons every session (SUBMITTED buffers POISONED, the socket copy closed) while the parent's session serves on; the unload pin is harmless in a static image" {
    const gpa = testing.allocator;
    var fx: Fixture = undefined;
    try fx.init(gpa, test_geometry, 1 << 20);
    defer fx.deinit(gpa);
    const s = try fx.openSession(gpa, default_opts);
    // Statically linked: the pin finds the main program (or nothing) and
    // never fails loud; the C smoke's shared variant runs it inside the .so.
    _ = pinSelf();
    const f = s.openFile(fx.file().ptr, O_RDWR, 0, .{ .struct_size = @sizeOf(OpenOpts), .durability = 0 }).?;
    const b = s.bufAlloc(4096).?;
    try b.lease.submit(); // a lease the child must see as SUBMITTED → POISONED
    // libc's fork (the raw syscall runs no atfork handler).
    const pid = std.c.fork();
    if (pid == 0) {
        // The child: poisoned by the atfork handler; a data call is -ENOTCONN.
        var code: u8 = 0;
        if (s.poisonCause() != .fork) code |= 1;
        if (b.lease.load().state != .poisoned) code |= 2;
        const iov = [_]IoVec{.{ .buf = b, .buf_off = 0, .len = 4096 }};
        if (s.rw(f, .read, &iov, 0) != neg(.NOTCONN)) code |= 4;
        var cqe: [1]Cqe = undefined;
        if (s.poll(&cqe) != neg(.NOTCONN)) code |= 8;
        linux.exit(code);
    }
    var status: i32 = 0;
    _ = linux.waitpid(pid, &status, 0);
    try testing.expectEqual(@as(i32, 0), (status >> 8) & 0xff);
    // The parent is untouched: its lease is still SUBMITTED, its session serves.
    try testing.expectEqual(lease.State.app_owned, b.lease.load().state);
    try testing.expectEqual(@as(u24, 1), b.lease.load().inflight);
    b.lease.complete();
    const iov = [_]IoVec{.{ .buf = b, .buf_off = 0, .len = 4096 }};
    try testing.expectEqual(@as(i64, 4096), s.rw(f, .read, &iov, 0));
    try testing.expect(!s.poisoned());
    try s.arena.free(b);
    try testing.expectEqual(@as(i32, 0), s.closeFile(f));
    try testing.expectEqual(@as(i32, 0), s.close());
}
