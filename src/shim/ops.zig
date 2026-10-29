//! The interposer BODIES (spec `docs/porting/il_shim.md` §1): plain
//! functions over the process state, called by the exported wrappers in
//! `interpose.zig` and directly by the linked-mode tests. A data op answers
//! a `Verdict`: `done` (the shim's result, errno already set on −1) or
//! `real` (the wrapper makes the real call — "resolve eagerly, call
//! lazily": no body ever calls a real function, so a bound-fd write can
//! never double-apply). Lifecycle bodies run before or after the real call
//! as the symbol's law demands (`close` bookkeeping FIRST; `dup` AFTER).
//!
//! Every exit from a served path is one of the bail-out ladder's rows
//! (§4), attributed through `classify.note`. Nothing here allocates.

const std = @import("std");
const linux = std.os.linux;

const sys = @import("sys.zig");
const fd_table = @import("fd_table.zig");
const classify = @import("classify.zig");
const client = @import("../ipc/client.zig");
const layout = @import("../ipc/layout.zig");
const wire = @import("../ipc/wire.zig");
const fiemap_verb = @import("../ipc/fiemap_verb.zig");

pub const State = classify.State;
pub const Session = client.Session;
const Reason = classify.Reason;

pub fn Verdict(comptime T: type) type {
    return union(enum) {
        done: T,
        real,
    };
}

pub const Op = layout.Op;

/// `struct iovec` with a nullable base (`std.posix.iovec` cannot hold
/// address 0, which an app's zero-length segment legally carries).
pub const Iovec = extern struct {
    base: ?[*]u8,
    len: usize,
};

/// `RWF_*` the ring serves identically: `RWF_HIPRI` only (`SYNC`/`DSYNC`/
/// `APPEND`/`NOWAIT` and every unknown bit change the op's semantics and
/// take the real call).
pub const RWF_ALLOWED: u32 = linux.RWF.HIPRI;

pub fn rwfPassthrough(flags: u32) bool {
    return flags & ~RWF_ALLOWED != 0;
}

// ---------------------------------------------------------------------------
// The ring core shared by every data shape.
// ---------------------------------------------------------------------------

const RingOutcome = union(enum) {
    served: usize,
    errno: linux.E,
    /// The real call, binding stays (transient).
    real,
    /// The real call; the session is poisoned: demote + release the entry.
    real_poisoned,
};

fn sessionOf(st: *State, cell: *fd_table.Cell) *Session {
    return st.sessionAt(cell.binding.session);
}

/// Rights → gate → the ring op → the outcome map (§1.2).
fn ringPositional(st: *State, cell: *fd_table.Cell, op: Op, buf: []u8, offset: u64, gate: bool) RingOutcome {
    const s = sessionOf(st, cell);
    const ok = switch (op) {
        .read => cell.binding.rights.read,
        .write => cell.binding.rights.write,
        else => false,
    };
    if (!ok) {
        // The kernel's own EBADF is the identical answer.
        classify.note(.wrong_direction, s);
        return .real;
    }
    // The direct arm (charter §7 "The direct arm") before the lane gate: the
    // gate prices the ring's copies, which the device I/O never pays.
    const direct: ?client.Outcome = switch (op) {
        .read => s.directPread(cell.binding.id, buf, offset),
        else => s.directPwrite(cell.binding.id, buf, offset),
    };
    if (direct) |o| switch (o) {
        .served => |n| return .{ .served = n },
        .errno => |e| return .{ .errno = e },
        .fallthrough => {
            classify.note(.poisoned, s);
            return .real_poisoned;
        },
    };
    if (gate and s.routesKernel(buf.len)) {
        s.noteKernelRoute(buf.len);
        classify.note(.lane_gate, s);
        return .real;
    }
    const outcome = switch (op) {
        .read => s.preadRing(cell.binding.id, buf, offset),
        else => s.pwriteRing(cell.binding.id, buf, offset),
    };
    switch (outcome) {
        .served => |n| return .{ .served = n },
        .errno => |e| return .{ .errno = e },
        .fallthrough => {
            if (s.poisoned()) {
                classify.note(.poisoned, s);
                return .real_poisoned;
            }
            classify.note(.no_slot_or_shape, s);
            return .real;
        },
    }
}

/// Demote the mirror (disarm once, write the captured offset through).
fn demoteCell(st: *State, fd: sys.fd_t, cell: *fd_table.Cell) void {
    if (cell.disarmIfCurrent(st.table.currentEpoch())) |off| sys.seekSet(fd, off);
}

/// `demote_and_flush(fd)`: an fd-level demote under a bounded `tryLock`
/// (the callers — `fcntl`, `sendfile`, `*v2` — are AS-safe entry points
/// that must not park on a lock a mid-serve thread holds; the Dekker pair
/// keeps the unlocked demote exact).
pub fn demoteAndFlush(st: *State, fd: sys.fd_t) void {
    const pin = st.table.lookup(fd) orelse return;
    defer if (pin.unpin()) |d| st.release(d);
    var spins: u32 = 0;
    var locked = false;
    while (spins < fd_table.FORK_TRYLOCK_SPINS) : (spins += 1) {
        if (pin.cell.lock.tryLock()) {
            locked = true;
            break;
        }
        std.atomic.spinLoopHint();
    }
    demoteCell(st, fd, pin.cell);
    if (locked) pin.cell.lock.unlock();
}

/// Release the fd's entry (the per-op poison arm: refcount release; the
/// unbind ctl is silent on a poisoned session).
fn dropEntry(st: *State, fd: sys.fd_t) void {
    if (st.table.onClose(fd)) |d| st.release(d);
}

// ---------------------------------------------------------------------------
// Positional: pread/pwrite (§1.2).
// ---------------------------------------------------------------------------

pub fn positional(st: *State, op: Op, fd: sys.fd_t, buf: ?[*]u8, count: usize, offset: i64) Verdict(isize) {
    // The shape rows: the real call's errno/0 is the answer.
    if (offset < 0 or count == 0 or buf == null) return .real;
    const pin = st.table.lookup(fd) orelse return .real;
    defer if (pin.unpin()) |d| st.release(d);
    switch (ringPositional(st, pin.cell, op, buf.?[0..count], @intCast(offset), true)) {
        .served => |n| return .{ .done = @intCast(n) },
        .errno => |e| {
            sys.setErrno(e);
            return .{ .done = -1 };
        },
        .real => return .real,
        .real_poisoned => {
            demoteCell(st, fd, pin.cell);
            dropEntry(st, fd);
            return .real;
        },
    }
}

// ---------------------------------------------------------------------------
// Offsetful: read/write (§1.3) — the mirror law.
// ---------------------------------------------------------------------------

/// The offset an offsetful op starts at: the mirror when authoritative,
/// else the kernel's `f_pos` (raw SEEK_CUR: no FUSE request).
const Start = struct { offset: u64, armed: bool };

fn startOffset(st: *State, fd: sys.fd_t, cell: *fd_table.Cell) ?Start {
    if (cell.authoritative(st.table.currentEpoch())) return .{ .offset = cell.offset.load(.monotonic), .armed = true };
    const k = sys.tell(fd) orelse return null;
    return .{ .offset = k, .armed = false };
}

/// After `n > 0` served bytes: armed ⇒ publish (a racing demote ⇒ the op
/// writes through itself); unarmed ⇒ raw SEEK_SET.
fn finishOffsetful(fd: sys.fd_t, cell: *fd_table.Cell, start: Start, n: usize) void {
    const end = start.offset + n;
    if (start.armed) {
        if (!cell.publish(end)) sys.seekSet(fd, end);
    } else sys.seekSet(fd, end);
}

/// The sticky latch + the gate trigger under the held lock (§1.3 steps
/// 2 and 4). True = the op takes the real call.
fn latchedOrGated(st: *State, fd: sys.fd_t, cell: *fd_table.Cell, op: Op, len: usize) bool {
    const s = sessionOf(st, cell);
    const rights_ok = switch (op) {
        .read => cell.binding.rights.read,
        .write => cell.binding.rights.write,
        else => false,
    };
    if (rights_ok and s.routesKernel(len)) {
        cell.kernel_latch.store(true, .release);
        demoteCell(st, fd, cell);
        s.noteKernelRoute(len);
        classify.note(.lane_gate, s);
        return true;
    }
    return false;
}

pub fn offsetful(st: *State, op: Op, fd: sys.fd_t, buf: ?[*]u8, count: usize) Verdict(isize) {
    if (count == 0 or buf == null) return .real;
    const pin = st.table.lookup(fd) orelse return .real;
    defer if (pin.unpin()) |d| st.release(d);
    const cell = pin.cell;
    if (cell.kernel_latch.load(.acquire)) {
        classify.note(.kernel_latch, sessionOf(st, cell));
        return .real;
    }
    cell.lock.lock();
    defer cell.lock.unlock();
    if (latchedOrGated(st, fd, cell, op, count)) return .real;
    const start = startOffset(st, fd, cell) orelse {
        classify.note(.unseekable, sessionOf(st, cell));
        return .real;
    };
    switch (ringPositional(st, cell, op, buf.?[0..count], start.offset, false)) {
        .served => |n| {
            if (n > 0) finishOffsetful(fd, cell, start, n);
            return .{ .done = @intCast(n) };
        },
        .errno => |e| {
            // No bytes consumed: NEITHER offset moves.
            sys.setErrno(e);
            return .{ .done = -1 };
        },
        .real => {
            // Disarm-once + flush under the held lock: the real call
            // consumes kernel `f_pos` next, and this fd prices at the
            // two-syscall resync discipline thereafter.
            if (start.armed) demoteCell(st, fd, cell);
            return .real;
        },
        .real_poisoned => {
            if (start.armed) demoteCell(st, fd, cell);
            dropEntry(st, fd);
            return .real;
        },
    }
}

// ---------------------------------------------------------------------------
// Vector: readv/writev/preadv*/pwritev* (§1.4).
// ---------------------------------------------------------------------------

/// Total bytes of a vector (checked; null = the kernel's EINVAL shape).
fn vectorTotal(iov: [*]const Iovec, n: usize) ?usize {
    var total: usize = 0;
    var i: usize = 0;
    while (i < n) : (i += 1) {
        total = std.math.add(usize, total, iov[i].len) catch return null;
        if (total > std.math.maxInt(isize)) return null;
    }
    return total;
}

/// `offset == null` is the offsetful shape (readv/writev, `*v2` at −1).
pub fn vector(st: *State, op: Op, fd: sys.fd_t, iov_opt: ?[*]const Iovec, iovcnt: c_int, offset: ?i64, rwf: u32) Verdict(isize) {
    const iov = iov_opt orelse return .real;
    if (iovcnt <= 0) return .real;
    if (offset) |o| if (o < 0) return .real;
    const n: usize = @intCast(iovcnt);
    const total = vectorTotal(iov, n) orelse return .real;
    if (total == 0) return .real;
    const pin = st.table.lookup(fd) orelse return .real;
    defer if (pin.unpin()) |d| st.release(d);
    const cell = pin.cell;
    const s = sessionOf(st, cell);
    const offsetful_shape = offset == null;
    if (rwfPassthrough(rwf)) {
        if (offsetful_shape) demoteCell(st, fd, cell);
        classify.note(.rwf_flags, s);
        return .real;
    }
    var locked = false;
    defer if (locked) cell.lock.unlock();
    var start: Start = .{ .offset = @intCast(offset orelse 0), .armed = false };
    if (offsetful_shape) {
        if (cell.kernel_latch.load(.acquire)) {
            classify.note(.kernel_latch, s);
            return .real;
        }
        cell.lock.lock();
        locked = true;
        if (latchedOrGated(st, fd, cell, op, total)) return .real;
        start = startOffset(st, fd, cell) orelse {
            classify.note(.unseekable, s);
            return .real;
        };
    } else if (s.routesKernel(total)) {
        s.noteKernelRoute(total);
        classify.note(.lane_gate, s);
        return .real;
    }
    // Segment by segment; a short segment ends the op; an errno or a
    // fallthrough mid-chain returns the partial count.
    var done: usize = 0;
    var i: usize = 0;
    var stop = false;
    var dying = false;
    while (i < n and !stop) : (i += 1) {
        const seg = iov[i];
        if (seg.len == 0) continue;
        const base = seg.base orelse {
            // A NULL base with a length: the kernel's EFAULT — only as the
            // first segment is it the real call's verdict.
            if (done == 0) return .real;
            break;
        };
        switch (ringPositional(st, cell, op, base[0..seg.len], start.offset + done, false)) {
            .served => |k| {
                done += k;
                if (k < seg.len) stop = true;
            },
            .errno => |e| {
                if (done == 0) {
                    sys.setErrno(e);
                    return .{ .done = -1 };
                }
                stop = true;
            },
            .real => {
                if (done == 0) {
                    if (offsetful_shape and start.armed) demoteCell(st, fd, cell);
                    return .real;
                }
                stop = true;
            },
            .real_poisoned => {
                dying = true;
                if (done == 0) {
                    if (offsetful_shape and start.armed) demoteCell(st, fd, cell);
                    dropEntry(st, fd);
                    return .real;
                }
                stop = true;
            },
        }
    }
    if (offsetful_shape and done > 0) {
        if (dying) {
            // Short-served and dying: park the kernel `f_pos` at start +
            // done under the held lock, then release the entry.
            if (start.armed) _ = cell.disarmIfCurrent(st.table.currentEpoch());
            sys.seekSet(fd, start.offset + done);
        } else finishOffsetful(fd, cell, start, done);
    }
    if (dying) dropEntry(st, fd);
    return .{ .done = @intCast(done) };
}

// ---------------------------------------------------------------------------
// lseek (§1.5).
// ---------------------------------------------------------------------------

pub fn lseek(st: *State, fd: sys.fd_t, off: i64, whence: c_int) Verdict(i64) {
    const pin = st.table.lookup(fd) orelse return .real;
    defer if (pin.unpin()) |d| st.release(d);
    const cell = pin.cell;
    const epoch = st.table.currentEpoch();
    if (!cell.authoritative(epoch)) return .real;
    cell.lock.lock();
    defer cell.lock.unlock();
    if (!cell.authoritative(epoch)) return .real;
    switch (whence) {
        linux.SEEK.SET, linux.SEEK.CUR => {
            const target = cell.seekArith(@intCast(whence), off) orelse {
                sys.setErrno(.INVAL);
                return .{ .done = -1 };
            };
            if (!cell.publish(target)) sys.seekSet(fd, target);
            return .{ .done = @intCast(target) };
        },
        else => {
            // SEEK_END / SEEK_DATA / SEEK_HOLE / unknown: the kernel's; on
            // success the mirror re-seeds and the arm SURVIVES (none of
            // these read `f_pos`).
            const rc = sys.lseekRaw(fd, off, @intCast(whence));
            if (linux.errno(rc) != .SUCCESS) return .{ .done = sys.libcResult(rc) };
            if (!cell.publish(rc)) sys.seekSet(fd, rc);
            return .{ .done = @intCast(rc) };
        },
    }
}

// ---------------------------------------------------------------------------
// `f_pos`-consuming passthroughs (§1.6): demote triggers only.
// ---------------------------------------------------------------------------

pub fn sendfilePre(st: *State, out_fd: sys.fd_t, in_fd: sys.fd_t, in_offset_null: bool) void {
    noteDemote(st, out_fd);
    demoteAndFlush(st, out_fd);
    if (in_offset_null) {
        noteDemote(st, in_fd);
        demoteAndFlush(st, in_fd);
    }
}

/// `copy_file_range`/`splice`: the side whose offset pointer is NULL.
pub fn copyRangePre(st: *State, fd_in: sys.fd_t, in_null: bool, fd_out: sys.fd_t, out_null: bool) void {
    if (in_null) {
        noteDemote(st, fd_in);
        demoteAndFlush(st, fd_in);
    }
    if (out_null) {
        noteDemote(st, fd_out);
        demoteAndFlush(st, fd_out);
    }
}

fn noteDemote(st: *State, fd: sys.fd_t) void {
    if (st.table.lookup(fd)) |pin| {
        classify.note(.fpos_consumer, sessionOf(st, pin.cell));
        if (pin.unpin()) |d| st.release(d);
    }
}

// ---------------------------------------------------------------------------
// Lifecycle (§1.7, §1.9–§1.11).
// ---------------------------------------------------------------------------

/// `close(fd)`: bookkeeping FIRST (the number can be reused the instant the
/// real close returns).
pub fn closePre(st: *State, fd: sys.fd_t) void {
    if (st.table.onClose(fd)) |d| st.release(d);
}

/// `fsync`/`fdatasync` BEFORE the real call: a bound fd on the direct
/// write arm sends and acks its pending postlude note and `fdatasync`s the
/// devices it wrote (decision 0003 §AY), so the kernel's FSYNC that follows
/// finds the stamp owed and the bytes flushed. Every other fd: nothing.
pub fn fsyncPre(st: *State, fd: sys.fd_t) void {
    const pin = st.table.lookup(fd) orelse return;
    defer if (pin.unpin()) |d| st.release(d);
    const s = sessionOf(st, pin.cell);
    _ = s.directFsync(pin.cell.binding.id);
}

/// `dup*`/`fcntl(F_DUPFD*)` after the real call succeeded.
pub fn dupPost(st: *State, oldfd: sys.fd_t, newfd: sys.fd_t) void {
    if (newfd < 0) return;
    if (st.table.onDup(oldfd, newfd)) |d| st.release(d);
}

fn dropOne(st: *State, d: fd_table.Drop) void {
    st.release(d);
}

/// `close_range`: `CLOSE_RANGE_CLOEXEC` marks only ⇒ no sweep.
pub fn closeRangePre(st: *State, first: c_uint, last: c_uint, flags: c_uint) void {
    const cloexec: u32 = @bitCast(linux.CLOSE_RANGE{ .UNSHARE = false, .CLOEXEC = true });
    if (flags & cloexec != 0) return;
    st.table.onCloseRange(first, @min(last, std.math.maxInt(i32)), st, dropOne);
}

/// The fd-creator hygiene sweep: a returned number carrying a stale entry
/// (a bound fd closed by a raw syscall / `IORING_OP_CLOSE`) is released
/// EXACTLY as `close` would. The common empty case is a segment load + a
/// slot load.
pub fn sweep(st: *State, fd: sys.fd_t) void {
    if (fd < 0 or !st.table.isBound(fd)) return;
    if (st.table.onClose(fd)) |d| st.release(d);
}

/// `fcntl` BEFORE the real call: locks, `F_SETFL` + O_APPEND, an O_DIRECT
/// or O_NOATIME toggle on a bound fd ⇒ demote + unbind (the daemon
/// captured the description's read class and its atime exemption at bind;
/// a diverged class would serve the wrong admission semantics, a diverged
/// exemption the wrong atime law).
pub fn fcntlPre(st: *State, fd: sys.fd_t, cmd: c_int, arg: usize) void {
    const F = linux.F;
    const unbind = switch (cmd) {
        F.SETLK, F.SETLKW, F.OFD_SETLK, F.OFD_SETLKW => true,
        F.SETFL => blk: {
            const want: u32 = @truncate(arg);
            if (want & sys.O.APPEND != 0) break :blk true;
            if (!st.table.isBound(fd)) break :blk false;
            const cur = sys.getfl(fd) orelse break :blk false;
            break :blk (cur ^ want) & (sys.O.DIRECT | sys.O.NOATIME) != 0;
        },
        else => false,
    };
    if (!unbind) return;
    if (st.table.lookup(fd)) |pin| {
        classify.note(.fcntl_unbind, sessionOf(st, pin.cell));
        if (pin.unpin()) |d| st.release(d);
    }
    demoteAndFlush(st, fd);
    dropEntry(st, fd);
}

/// `MAP_SHARED`/`MAP_SHARED_VALIDATE` in the low 4 bits (a small enum,
/// not a bitmask).
pub fn mapShared(flags: c_int) bool {
    const kind: u32 = @as(u32, @bitCast(flags)) & 0xf;
    return kind == 0x01 or kind == 0x03;
}

fn flushOne(_: void, f: fd_table.Flush) void {
    sys.seekSet(f.fd, f.offset);
}

fn flushOneSt(_: *State, f: fd_table.Flush) void {
    sys.seekSet(f.fd, f.offset);
}

/// `mmap` BEFORE the real call (§1.10): (a) POSIX-7 poison — a shared
/// mapping of a regular file on a device with any session (or this fd
/// bound) marks the inode never-bind; (b) a bound fd unbinds EVERY entry
/// naming its inode, flushing each armed mirror first.
pub fn mmapPre(st: *State, fd: sys.fd_t, flags: c_int) void {
    if (fd < 0) return;
    const pin = st.table.lookup(fd);
    const bound: ?fd_table.Binding = if (pin) |p| p.cell.binding else null;
    if (pin) |p| if (p.unpin()) |d| st.release(d);
    if (mapShared(flags)) {
        if (bound) |b| {
            st.mapped.insert(b.dev, b.ino);
        } else if (sys.fstat(fd)) |stat| {
            if (stat.kind == .regular and !st.dev_cache.contains(stat.dev, 0) and st.hasSessionOn(stat.dev)) st.mapped.insert(stat.dev, stat.ino);
        }
    }
    if (bound) |b| {
        classify.note(.mmap_unbind, st.sessionAt(b.session));
        st.table.unbindIno(b.dev, b.ino, st, flushOneSt, dropOne);
    }
}

/// `posix_spawn{,p}` BEFORE the real spawn (glibc spawns via `clone` and
/// runs NO atfork handlers): the fork flush.
pub fn spawnPre(st: *State) void {
    st.table.forkFlushAll({}, flushOne);
}

// ---------------------------------------------------------------------------
// FIEMAP (charter §4 "FIEMAP", decision 0003 §CJ): `ioctl_fiemap`'s copy law
// over the IPC verb — Linux answers FS_IOC_FIEMAP on a FUSE file in the VFS
// (EOPNOTSUPP), so the interposer is the only place it can be served.
// ---------------------------------------------------------------------------

const fiemap = fiemap_verb.fiemap;
pub const FS_IOC_FIEMAP: u32 = fiemap.FS_IOC_FIEMAP;

/// `ioctl(fd, FS_IOC_FIEMAP, arg)`: a regular file of an interception mount
/// is served by the daemon's map; anything else — another filesystem, an
/// unarmed mount, `O_PATH`, a directory, a dead session — is the real call.
/// The copies are the kernel's: the 32 B header in (EFAULT on an unmapped
/// address, never a fault in the app), the extents out batch by batch from
/// `arg + 32` (a batch the daemon filled without LAST continues from its
/// last extent's end, SYNC asked once), and the header back with
/// `fm_mapped_extents` and `fm_flags` — on an error too, as `ioctl_fiemap`
/// copies it back after `->fiemap` failed (EBADR: the refused flags).
pub fn fiemapIoctl(st: *State, p: classify.Probes, fd: sys.fd_t, arg: usize) Verdict(c_int) {
    const fl = p.getfl(fd) orelse return .real;
    if (fl & sys.O.PATH != 0) return .real;
    const stat = p.fstat(fd) orelse return .real;
    if (stat.kind != .regular) return .real;
    const s = classify.sessionFor(st, p, fd, stat) orelse return .real;
    var h: fiemap.Fiemap = undefined;
    switch (sys.copyFromUser(std.mem.asBytes(&h), arg)) {
        .SUCCESS => {},
        .FAULT => return fiemapFail(.FAULT),
        // `process_vm_readv` refused (a seccomp filter): the real call.
        else => return .real,
    }
    const buf = sys.mapAnon(wire.CTL_MSG_MAX) orelse return .real;
    defer sys.unmapAnon(buf, wire.CTL_MSG_MAX);
    const want = h.fm_extent_count;
    const end = h.fm_start +| h.fm_length;
    var start = h.fm_start;
    var flags = h.fm_flags;
    var mapped: u32 = 0;
    var first = true;
    while (true) {
        const batch: u32 = if (want == 0) 0 else @min(want - mapped, fiemap_verb.REPLY_MAX_EXTENTS);
        const req: fiemap_verb.FiemapReq = .{ .fm_flags = flags, .fm_start = start, .fm_length = if (first) h.fm_length else end - start, .fm_extent_count = batch };
        const rep = fiemap_verb.ask(s, fd, req, buf[0..wire.CTL_MSG_MAX]) catch {
            classify.note(.poisoned, s);
            // Nothing of the caller's memory moved yet: the kernel's answer.
            if (mapped == 0) return .real;
            return fiemapFinish(arg, h, h.fm_flags, mapped, .IO);
        };
        if (rep.errno != 0) return fiemapFinish(arg, h, rep.fm_flags, mapped, @fromBackingInt(@as(u16, @intCast(rep.errno))));
        if (want == 0) return fiemapFinish(arg, h, h.fm_flags, rep.fm_mapped_extents, null);
        const n: u32 = @intCast(rep.extents.len);
        const bytes = std.mem.sliceAsBytes(rep.extents);
        const moved = sys.copyToUserPartial(arg + @sizeOf(fiemap.Fiemap) + @as(usize, mapped) * @sizeOf(fiemap.Extent), bytes);
        mapped += @intCast(moved.n / @sizeOf(fiemap.Extent));
        if (moved.n != bytes.len) return fiemapFinish(arg, h, h.fm_flags, mapped, .FAULT);
        if (n == 0 or mapped == want) break;
        const last = rep.extents[n - 1];
        if (last.fe_flags & fiemap.FIEMAP_EXTENT_LAST != 0) break;
        const next = last.fe_logical +| last.fe_length;
        // A batch that does not advance or reaches the end is the last.
        if (next <= start or next >= end) break;
        start = next;
        flags &= ~fiemap.FIEMAP_FLAG_SYNC;
        first = false;
    }
    return fiemapFinish(arg, h, h.fm_flags, mapped, null);
}

fn fiemapFail(e: linux.E) Verdict(c_int) {
    sys.setErrno(e);
    return .{ .done = -1 };
}

/// The header back (`fm_flags`, `fm_mapped_extents`; every other field as
/// the caller wrote it), then the answer.
fn fiemapFinish(arg: usize, h: fiemap.Fiemap, fm_flags: u32, mapped: u32, err: ?linux.E) Verdict(c_int) {
    var back = h;
    back.fm_flags = fm_flags;
    back.fm_mapped_extents = mapped;
    if (sys.copyToUser(arg, std.mem.asBytes(&back)) != .SUCCESS) return fiemapFail(.FAULT);
    if (err) |e| return fiemapFail(e);
    return .{ .done = 0 };
}
