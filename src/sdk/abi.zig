//! The C ABI of `libsqueezefs_sdk` (charter §12 "the SDK exports a C ABI";
//! spec `docs/porting/il_sdk.md` §1): `export fn sqz_*` only — nothing of
//! Zig's and no libc name leaves this object, so it is safe to link into
//! anything. `include/squeezefs_sdk.h` is the hand-written mirror (0.17's
//! `-femit-h` emits nothing); the rails census pins the two together
//! (`tests/rails.zig`) and the C smoke (`tests/sdk/`) compiles against it.
//!
//! THE ONE ERROR CONVENTION (decision 0002, the SDK rows): every fallible
//! call returns a NEGATIVE errno (`-EBUSY`, `-EINVAL`, …) and never writes
//! `errno(3)`; success is `≥ 0` (a byte count, a count of entries, 0).
//! Constructors return `NULL` with the cause in `sqz_last_error()` (the
//! negative errno) and `sqz_last_error_text()` (the reason line), both
//! thread-local. A `NULL` or stale handle is `-EINVAL`, never a crash in
//! the SDK's own screens; a memory-safety bug INSIDE the SDK still aborts
//! loud (ReleaseSafe, charter §1.8).
//!
//! Thread-safety: a session's data-path calls are safe from any thread
//! (slot claiming and the lease word are lock-free; the batch bookkeeping
//! takes one short futex lock per call); the ctl path (`sqz_open`,
//! `sqz_close`) serializes on the session's ctl lock. A `sqz_file_t` or a
//! `sqz_buf_t` may be used from many threads — the lease word is the
//! arbiter. Handles are not valid after their close/free (a freed buffer
//! record refuses `-EINVAL`; a closed file or session handle is dangling
//! memory, as with any C library).

const std = @import("std");
const linux = std.os.linux;
const session = @import("session.zig");
const lease = @import("buffer_lease.zig");
const version = @import("version.zig");
const errors = @import("../core/errors.zig");
const layout = @import("../ipc/layout.zig");

pub const SqzSession = opaque {};
pub const SqzFile = opaque {};
pub const SqzBuf = opaque {};

/// A host process's `malloc` (the SDK's control plane allocates there;
/// the data path allocates nothing per op).
const gpa = std.heap.c_allocator;

const VERSION_STRING = std.fmt.comptimePrint("squeezefs-sdk wire-abi {d} sdk-abi {d}", .{ version.WIRE_ABI, version.SDK_ABI });

fn negInt(e: linux.E) c_int {
    return -@as(c_int, @backingInt(e));
}

fn negSize(e: linux.E) isize {
    return -@as(isize, @backingInt(e));
}

fn sessionOf(p: ?*SqzSession) ?*session.Session {
    return @ptrCast(@alignCast(p orelse return null));
}

fn fileOf(p: ?*SqzFile) ?*session.File {
    const f: *session.File = @ptrCast(@alignCast(p orelse return null));
    return if (f.live) f else null;
}

/// A buffer record whose arena is known (allocated at least once).
fn bufOf(p: ?*SqzBuf) ?*lease.Buf {
    const b: *lease.Buf = @ptrCast(@alignCast(p orelse return null));
    return if (b.arena != null) b else null;
}

// ---------------------------------------------------------------------------
// Version.
// ---------------------------------------------------------------------------

/// Static text: the two ABI parts.
export fn sqz_version() callconv(.c) [*:0]const u8 {
    return VERSION_STRING;
}

/// The two-part ABI word: `wire << 16 | sdk` (`SQZ_ABI_WORD` in the header).
export fn sqz_abi() callconv(.c) u32 {
    return version.word();
}

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

/// The thread-local negative errno of the last constructor that returned
/// NULL on this thread (0 if none).
export fn sqz_last_error() callconv(.c) c_int {
    return session.lastError();
}

/// The thread-local reason line of the last constructor failure ("" if
/// none); static lifetime per thread, overwritten by the next failure.
export fn sqz_last_error_text() callconv(.c) [*:0]const u8 {
    return session.lastErrorTextZ();
}

/// Static text for an SDK result code (a negative errno as every `sqz_*`
/// call returns it, or 0): the SDK's meaning, not libc's.
export fn sqz_strerror(err: c_int) callconv(.c) [*:0]const u8 {
    if (err == 0) return "success";
    if (err > 0 or err < -std.math.maxInt(u16)) return "not an SDK result code (the SDK returns negative errnos)";
    const e: linux.E = @fromBackingInt(@intCast(@as(u16, @intCast(-err))));
    return switch (e) {
        .BUSY => "busy: the buffer is SUBMITTED (free while leased is refused, never deferred), or the session still has open files / leased buffers",
        .INVAL => "invalid argument: a NULL or stale handle, a window past the buffer, a bad opcode, durability or struct_size",
        .NOTCONN => "session poisoned: the daemon died, an op deadline passed or this is a fork child — every later call refuses",
        .AGAIN => "no free slot: the session's depth is exhausted (sqz_submit placed a prefix; a sync op could not place its first chunk), or the daemon's admission cap refused",
        .NOMEM => "no free arena block of that order (sqz_buf_alloc), or out of memory",
        .IO => "the op was in flight when the session poisoned (-EIO), or a data-plane I/O error",
        .BADF => "wrong direction: a read on a write-only handle or a write on a read-only one",
        .XDEV => "the file is not on the session's mount",
        .PROTO => "malformed reply from the daemon, or a durability grant weaker than requested",
        .PROTONOSUPPORT => "abi outside the daemon's window (sqz_last_error_text names both values)",
        .NODATA => "no bootstrap xattr: the mount is not armed (mount with -o interception)",
        .OPNOTSUPP => "not a squeezefs mount, or interception not armed",
        .ACCES => "daemon authentication failed (rendezvous directory, peer uid or memfd seals)",
        .CONNREFUSED => "the daemon's ctl socket refused the connection",
        .NOSYS => "futex_waitv(2) is unsupported by this kernel",
        .FBIG => "offset + length overflows the file offset space",
        .NOTDIR => "the mount path is not a directory",
        .NOENT => "no such file or directory",
        else => "squeezefs-sdk: a kernel errno (see errno(3) for its number)",
    };
}

// ---------------------------------------------------------------------------
// Sessions.
// ---------------------------------------------------------------------------

/// One shm session on one mount: the HELLO credential is the mount root,
/// the arena is bound here (so `sqz_buf_alloc` may precede `sqz_open`).
/// NULL + `sqz_last_error()`: `-ENOTSUP` not a squeezefs mount / not
/// armed, `-ENODATA` no bootstrap xattr, `-EPROTONOSUPPORT` abi outside
/// the window (the text names both values), `-EAGAIN` budget, `-EACCES`
/// the daemon failed OUR authentication, `-EINVAL` a bad `struct_size`.
export fn sqz_session_open(mount_path: ?[*:0]const u8, opts: ?*const session.SessionOpts) callconv(.c) ?*SqzSession {
    const path = mount_path orelse {
        session.setLastError(.INVAL, "mount_path is NULL");
        return null;
    };
    const o: session.SessionOpts = if (opts) |p| p.* else .{ .struct_size = @sizeOf(session.SessionOpts), .arena_bytes = 0, .slots = 0, .op_timeout_ms = 0 };
    return @ptrCast(session.Session.open(gpa, path, o) orelse return null);
}

/// `-EBUSY` while any file is open or any buffer is SUBMITTED; 0 unmaps
/// the arena (POISONED ranges with it) and closes the ctl socket.
export fn sqz_session_close(s: ?*SqzSession) callconv(.c) c_int {
    const sess = sessionOf(s) orelse return negInt(.INVAL);
    return sess.close();
}

/// The session's granted geometry: arena bytes, slots, the per-op window.
export fn sqz_session_arena_bytes(s: ?*SqzSession) callconv(.c) u64 {
    const sess = sessionOf(s) orelse return 0;
    return sess.cl.geometry().arena_bytes;
}

export fn sqz_session_slots(s: ?*SqzSession) callconv(.c) u32 {
    const sess = sessionOf(s) orelse return 0;
    return sess.cl.geometry().slots;
}

export fn sqz_session_max_op_bytes(s: ?*SqzSession) callconv(.c) u32 {
    const sess = sessionOf(s) orelse return 0;
    return sess.cl.geometry().max_op_bytes;
}

// ---------------------------------------------------------------------------
// Files.
// ---------------------------------------------------------------------------

/// The real `open(2)` (`O_CLOEXEC` forced) + BIND with the durability
/// class (`SQZ_D_MOUNT` | `SQZ_D_SYNC`; `O_SYNC`/`O_DSYNC` raise it to
/// SYNC; strengthen-only — no weaker class exists). NULL + the errno:
/// the kernel's for the open; `-EINVAL` for `O_APPEND`/`O_PATH`/
/// `O_TMPFILE`, a bad durability or `struct_size`, or a screen refusal;
/// `-EXDEV` off the mount; `-ENOTCONN` poisoned.
export fn sqz_open(s: ?*SqzSession, path: ?[*:0]const u8, oflags: c_int, mode: u32, opts: ?*const session.OpenOpts) callconv(.c) ?*SqzFile {
    const sess = sessionOf(s) orelse {
        session.setLastError(.INVAL, "session is NULL");
        return null;
    };
    const p = path orelse {
        session.setLastError(.INVAL, "path is NULL");
        return null;
    };
    const o: session.OpenOpts = if (opts) |x| x.* else .{ .struct_size = @sizeOf(session.OpenOpts), .durability = 0 };
    return @ptrCast(sess.openFile(p, @bitCast(oflags), mode, o) orelse return null);
}

/// `-EBUSY` while an op on this handle is SUBMITTED; UNBIND then the real
/// `close(2)` (the kernel's FLUSH/RELEASE unchanged).
export fn sqz_close(f: ?*SqzFile) callconv(.c) c_int {
    const file = fileOf(f) orelse return negInt(.INVAL);
    return file.session.closeFile(file);
}

/// The real `fsync(2)`: identical in every durability class.
export fn sqz_fsync(f: ?*SqzFile) callconv(.c) c_int {
    const file = fileOf(f) orelse return negInt(.INVAL);
    return session.Session.fsync(file);
}

export fn sqz_fstat(f: ?*SqzFile, st: ?*session.Stat) callconv(.c) c_int {
    const file = fileOf(f) orelse return negInt(.INVAL);
    return session.Session.fstat(file, st orelse return negInt(.INVAL));
}

/// The file's size in bytes, or `-errno`.
export fn sqz_fsize(f: ?*SqzFile) callconv(.c) i64 {
    const file = fileOf(f) orelse return negInt(.INVAL);
    var st: session.Stat = undefined;
    const rc = session.Session.fstat(file, &st);
    return if (rc < 0) rc else @intCast(st.size);
}

/// The real fd for everything the SDK does not do (stat, locks, …); the
/// binding belongs to the handle, which owns this ONE fd.
export fn sqz_fd(f: ?*SqzFile) callconv(.c) c_int {
    const file = fileOf(f) orelse return negInt(.INVAL);
    return file.fd;
}

/// The handle's effective durability class (`SQZ_D_*`), or `-EINVAL`.
export fn sqz_durability(f: ?*SqzFile) callconv(.c) c_int {
    const file = fileOf(f) orelse return negInt(.INVAL);
    return @intCast(@backingInt(file.durability));
}

// ---------------------------------------------------------------------------
// Buffers — THE BUFFER-LEASE LAW.
// ---------------------------------------------------------------------------

/// A `SQZ_BUF_ALIGN`-aligned arena range of `len` bytes (any `sqz_open`
/// may follow). NULL + `-ENOMEM` when no block of that order is free,
/// `-EINVAL` for `len == 0`, `-ENOTCONN` poisoned.
export fn sqz_buf_alloc(s: ?*SqzSession, len: usize) callconv(.c) ?*SqzBuf {
    const sess = sessionOf(s) orelse {
        session.setLastError(.INVAL, "session is NULL");
        return null;
    };
    return @ptrCast(sess.bufAlloc(len) orelse return null);
}

/// 0 and the range returns to the allocator; `-EBUSY` while SUBMITTED (one
/// CAS, never deferred); 0 for a POISONED buffer with the range quarantined
/// forever; `-EINVAL` for a double free.
export fn sqz_buf_free(b: ?*SqzBuf) callconv(.c) c_int {
    const buf = bufOf(b) orelse return negInt(.INVAL);
    buf.arena.?.free(buf) catch |e| return -@as(c_int, @backingInt(errors.toErrno(e)));
    return 0;
}

/// The buffer's address (reads are legal in every state but FREED; writes
/// only while APP_OWNED). NULL once FREED.
export fn sqz_buf_addr(b: ?*SqzBuf) callconv(.c) ?*anyopaque {
    const buf = bufOf(b) orelse return null;
    return @ptrCast(session.Session.ofBuf(buf).bufAddr(buf) orelse return null);
}

/// The requested length (0 once FREED).
export fn sqz_buf_len(b: ?*SqzBuf) callconv(.c) usize {
    const buf = bufOf(b) orelse return 0;
    return if (buf.live) @intCast(buf.len) else 0;
}

// ---------------------------------------------------------------------------
// Sync positional ops: submit the flights, reap inline, IN PLACE (no copy
// in either direction). Bytes or `-errno`: `-EINVAL` a window past the
// buffer / a FREED or POISONED buffer / a negative offset, `-EBADF` the
// wrong direction, `-EAGAIN` no slot for the first chunk, `-EIO` the op
// in flight at the daemon's death, `-ENOTCONN` after it.
// ---------------------------------------------------------------------------

fn rw1(f: ?*SqzFile, b: ?*SqzBuf, buf_off: u64, len: usize, offset: i64, op: layout.Op) isize {
    const file = fileOf(f) orelse return negSize(.INVAL);
    const buf = bufOf(b) orelse return negSize(.INVAL);
    const iov = [_]session.IoVec{.{ .buf = buf, .buf_off = buf_off, .len = len }};
    return @intCast(file.session.rw(file, op, &iov, offset));
}

export fn sqz_pread(f: ?*SqzFile, b: ?*SqzBuf, buf_off: u64, len: usize, offset: i64) callconv(.c) isize {
    return rw1(f, b, buf_off, len, offset, .read);
}

export fn sqz_pwrite(f: ?*SqzFile, b: ?*SqzBuf, buf_off: u64, len: usize, offset: i64) callconv(.c) isize {
    return rw1(f, b, buf_off, len, offset, .write);
}

fn rwv(f: ?*SqzFile, iov: ?[*]const session.IoVec, iovcnt: c_uint, offset: i64, op: layout.Op) isize {
    const file = fileOf(f) orelse return negSize(.INVAL);
    if (iovcnt == 0) return 0;
    const v = iov orelse return negSize(.INVAL);
    return @intCast(file.session.rw(file, op, v[0..iovcnt], offset));
}

/// A vector of `(buf, buf_off, len)` windows folded into one result with
/// POSIX prefix semantics.
export fn sqz_preadv(f: ?*SqzFile, iov: ?[*]const session.IoVec, iovcnt: c_uint, offset: i64) callconv(.c) isize {
    return rwv(f, iov, iovcnt, offset, .read);
}

export fn sqz_pwritev(f: ?*SqzFile, iov: ?[*]const session.IoVec, iovcnt: c_uint, offset: i64) callconv(.c) isize {
    return rwv(f, iov, iovcnt, offset, .write);
}

// ---------------------------------------------------------------------------
// The io_uring-shaped batch API.
// ---------------------------------------------------------------------------

/// Submit `n` SQEs under ONE doorbell: the count submitted (a prefix under
/// slot exhaustion — client-visible backpressure), `-EINVAL` with nothing
/// submitted on the first bad SQE (the batch is atomic in validation,
/// never in execution), `-EAGAIN` when not even the first could be placed,
/// `-ENOTCONN` poisoned. Each SQE's buffer becomes SUBMITTED until its CQE
/// is reaped.
export fn sqz_submit(s: ?*SqzSession, sqes: ?[*]const session.Sqe, n: c_uint) callconv(.c) c_int {
    const sess = sessionOf(s) orelse return negInt(.INVAL);
    if (n == 0) return 0;
    const q = sqes orelse return negInt(.INVAL);
    return sess.submit(q[0..n]);
}

/// The CQEs ready now (0 if none); `-EINVAL` for `max == 0`; after a
/// poison the `-EIO` CQEs of the ops then in flight, then `-ENOTCONN`.
export fn sqz_poll(s: ?*SqzSession, cqes: ?[*]session.Cqe, max: c_uint) callconv(.c) c_int {
    const sess = sessionOf(s) orelse return negInt(.INVAL);
    if (max == 0) return negInt(.INVAL);
    const c = cqes orelse return negInt(.INVAL);
    return sess.poll(c[0..max]);
}

/// Park on the completion doorbell until ≥ 1 CQE or the timeout (NULL =
/// forever): the count, 0 on the timeout or with nothing in flight,
/// `-ENOTCONN` once poisoned and drained. Multi-reaper-safe.
export fn sqz_wait(s: ?*SqzSession, cqes: ?[*]session.Cqe, max: c_uint, timeout: ?*const linux.timespec) callconv(.c) c_int {
    const sess = sessionOf(s) orelse return negInt(.INVAL);
    if (max == 0) return negInt(.INVAL);
    const c = cqes orelse return negInt(.INVAL);
    var ns: ?u64 = null;
    if (timeout) |t| {
        if (t.sec < 0 or t.nsec < 0 or t.nsec >= std.time.ns_per_s) return negInt(.INVAL);
        ns = @as(u64, @intCast(t.sec)) *| std.time.ns_per_s +| @as(u64, @intCast(t.nsec));
    }
    return sess.wait(c[0..max], ns);
}

// ---------------------------------------------------------------------------
// Tests: the ABI records and the handle screens (the surface itself is
// exercised by the C smoke, `tests/sdk/c_smoke.c`).
// ---------------------------------------------------------------------------

const testing = std.testing;

test "sqe/cqe/iovec/opts/stat are the pinned extern records: 48 / 16 / 24 / 24 / 8 / 96 bytes with user_data at 40" {
    try testing.expectEqual(@as(usize, 48), @sizeOf(session.Sqe));
    try testing.expectEqual(@as(usize, 40), @offsetOf(session.Sqe, "user_data"));
    try testing.expectEqual(@as(usize, 16), @sizeOf(session.Cqe));
    try testing.expectEqual(@as(usize, 24), @sizeOf(session.IoVec));
    try testing.expectEqual(@as(usize, 24), @sizeOf(session.SessionOpts));
    try testing.expectEqual(@as(usize, 8), @sizeOf(session.OpenOpts));
    try testing.expectEqual(@as(usize, 96), @sizeOf(session.Stat));
}

test "NULL and stale handles are -EINVAL on every call that takes one, never a crash; sqz_strerror covers every SDK code; sqz_abi is the two-part word" {
    const einval = negInt(.INVAL);
    try testing.expectEqual(einval, sqz_session_close(null));
    try testing.expectEqual(einval, sqz_close(null));
    try testing.expectEqual(einval, sqz_fsync(null));
    try testing.expectEqual(einval, sqz_fstat(null, null));
    try testing.expectEqual(@as(i64, einval), sqz_fsize(null));
    try testing.expectEqual(einval, sqz_fd(null));
    try testing.expectEqual(einval, sqz_durability(null));
    try testing.expectEqual(einval, sqz_buf_free(null));
    try testing.expectEqual(@as(?*anyopaque, null), sqz_buf_addr(null));
    try testing.expectEqual(@as(usize, 0), sqz_buf_len(null));
    try testing.expectEqual(@as(isize, einval), sqz_pread(null, null, 0, 1, 0));
    try testing.expectEqual(@as(isize, einval), sqz_pwritev(null, null, 1, 0));
    try testing.expectEqual(einval, sqz_submit(null, null, 1));
    try testing.expectEqual(einval, sqz_poll(null, null, 1));
    try testing.expectEqual(einval, sqz_wait(null, null, 1, null));
    try testing.expect(sqz_open(null, "x", 0, 0, null) == null);
    try testing.expectEqual(einval, sqz_last_error());
    try testing.expectEqualStrings("session is NULL", std.mem.span(sqz_last_error_text()));
    try testing.expect(sqz_buf_alloc(null, 4096) == null);
    try testing.expect(sqz_session_open(null, null) == null);
    try testing.expectEqual(einval, sqz_last_error());
    // A record never allocated has no arena: refused, not dereferenced.
    var fresh: lease.Buf = .{};
    try testing.expectEqual(einval, sqz_buf_free(@ptrCast(&fresh)));
    try testing.expectEqual(@as(u32, 1 << 16 | 1), sqz_abi());
    try testing.expectEqualStrings("squeezefs-sdk wire-abi 1 sdk-abi 1", std.mem.span(sqz_version()));
    try testing.expectEqualStrings("success", std.mem.span(sqz_strerror(0)));
    for ([_]linux.E{ .BUSY, .INVAL, .NOTCONN, .AGAIN, .NOMEM, .IO, .BADF, .XDEV, .PROTO, .PROTONOSUPPORT, .NODATA, .OPNOTSUPP, .ACCES, .CONNREFUSED, .NOSYS, .FBIG, .NOTDIR, .NOENT }) |e| {
        const t = std.mem.span(sqz_strerror(negInt(e)));
        try testing.expect(t.len > 0 and !std.mem.startsWith(u8, t, "squeezefs-sdk: a kernel errno"));
    }
    try testing.expect(std.mem.startsWith(u8, std.mem.span(sqz_strerror(negInt(.LOOP))), "squeezefs-sdk: a kernel errno"));
    try testing.expect(std.mem.startsWith(u8, std.mem.span(sqz_strerror(5)), "not an SDK result code"));
}
